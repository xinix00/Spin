//! Vaste verbindingspool en berichtentabel: alleen de app-taak raakt de Store aan.
#![no_std]
#![deny(unsafe_code)]
extern crate alloc;
use alloc::{string::String, sync::Arc, vec::Vec};
use core::{
    cell::{Cell, RefCell},
    task::{Context, Poll, Waker},
};
use spin_core::runner::Ticket;
use spin_domain::{Timestamp, try_string};
use spin_server::{
    CapsuleWait, ChatLink, Outcome, PasswordWork, Request, Response, Result, RunnerEvent,
    RunnerLink, Runtime, Server, StateWatch, TerminalLink, UploadWait,
};
use spin_store::Persistence;
mod assets;
mod outbound;
mod restore;
mod routing;
pub mod tenancy;
pub use routing::{Route, Routing};
#[allow(unsafe_code)]
mod task;
mod websocket;
/// Monotone klok, onafhankelijk van gecorrigeerde UTC-tijd.
pub trait Clock: Copy + 'static {
    /// Milliseconden sinds een vast platformtijdstip.
    fn millis(self) -> u64;
}
/// De boot-schil bezit listener, klok en executor; de app bezit alle staat.
pub trait Platform {
    /// Een verbinding met begrensde lees- en schrijfdeadlines.
    type Connection: leanhttp::Conn;
    /// Kopieerbaar handvat naar de monotone klok.
    type Clock: Clock;
    /// Verse uitgaande verbindingen, met TLS-ketencontrole op HTTPS.
    type Dial: leanhttp::Dial + 'static;
    /// Geeft een onafhankelijke dialer zonder de listener te lenen.
    fn dial(&self) -> Result<Self::Dial>;
    /// Neemt één verbinding aan zonder te wachten.
    fn accept(&mut self, context: &mut Context<'_>) -> Result<Option<(Self::Connection, String)>>;
    /// Geeft een onafhankelijke klok aan sockettaken.
    fn clock(&self) -> Self::Clock;
    /// Gevalideerde UTC voor duurzame gebeurtenissen.
    fn timestamp(&self) -> Result<Timestamp>;
    /// Vraagt de eigenaar te stoppen en alle sockettaken te droppen.
    fn stopped(&self) -> bool;
    /// Geeft netwerkpompen en andere platformtaken een ronde en laat de eigenaar
    /// rusten tot [`Mailbox::nudge`] of tot `next`: meteen weer bij
    /// [`Idle::Yield`] (gesneden CPU-werk ligt klaar), anders tot zijn
    /// vroegste echte deadline (handboek apps.md: een timer is een deadline,
    /// geen peiling).
    fn idle(&mut self, mail: &Mailbox, next: Idle) -> Result;
    /// Schrijft één diagnostische regel zonder verzoekinhoud.
    fn log(message: core::fmt::Arguments<'_>);
}
/// Wanneer de eigenaar weer aan de beurt wil zijn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Idle {
    /// Er ligt gesneden CPU-werk (een wachtwoordafleiding): één ronde aan de
    /// buren geven en meteen terugkomen.
    Yield,
    /// Slapen tot de deurbel of tot dit tijdstip in milliseconden van
    /// [`Clock`], de vroegste echte deadline van de eigenaar.
    Until(u64),
}
fn boundary(error: impl core::fmt::Display) -> spin_server::Error {
    let _ = error;
    spin_server::Error::Http(503, "runtime resource unavailable")
}
/// De bovengrens van alle actieve HTTP-verbindingen samen.
pub const CONNECTIONS: usize = 64;
/// CPU-intensieve verzoeken hebben naast de socketpool hun eigen kleinere grens.
pub const PASSWORD_TASKS: usize = 8;
pub(crate) struct Local<T>(pub(crate) T);
struct Input {
    method: String,
    path: String,
    raw_query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    peer: String,
    secure: bool,
}
#[derive(Default)]
pub(crate) struct Slot {
    request: Option<Input>,
    /// Klokstand van de sockettaak toen hij `request` neerlegde: de wachttijd
    /// in de rij is het verschil met de klok van de eigenaar als die hem pakt.
    queued_ms: u64,
    response: Option<Response>,
    streaming: bool,
    next_chunk: bool,
    chunk: Option<Vec<u8>>,
    upgrade: bool,
    pub(crate) frame: Option<Frame>,
    pub(crate) frame_bytes: usize,
    pub(crate) acknowledged: Option<Ticket>,
    pub(crate) incoming: Option<spin_domain::protocol::WireMessage>,
    pub(crate) browser: Option<spin_domain::json::Value>,
    pub(crate) touched: bool,
    pub(crate) close: Option<u16>,
    abort: bool,
    occupied: bool,
    routed: bool,
    finished: bool,
}
pub(crate) struct Frame {
    pub(crate) bytes: Vec<u8>,
    pub(crate) ticket: Option<Ticket>,
}
/// Socket mailboxes are separate from the SQLite owner's parked stack.
/// Interior borrowing only transfers bounded messages; no guard survives storage I/O or await.
pub struct Mailbox {
    slots: Local<RefCell<[Slot; CONNECTIONS]>>,
    health: Local<RefCell<Option<Response>>>,
    active: Cell<bool>,
    /// De deurbel van de eigenaar; in een Arc zodat hij ook als [`Waker`]
    /// aan providertaken en de socketpomp mee kan.
    bell: Arc<Nudge>,
    /// De voortgang van een herstel uit S3, voor de openingspagina.
    restore: alloc::rc::Rc<Restore>,
    /// Waar de eigenaar is: de laatst afgeronde stap (of `idle`, `woke`,
    /// `request`) en sinds wanneer (ms, de klok van de eigenaar). De boot-schil
    /// logt een eigenaar die lang in één fase blijft.
    phase: Cell<(&'static str, u64, bool)>,
    /// Bij een verzoek: methode en route met ids als `:id`.
    detail: RefCell<String>,
}
/// Hoeveel van een herstel uit S3 binnen is: de bytes die de S3-verbinding van
/// de eigenaar ontving, en het verwachte totaal (de snapshot in de bucket).
#[derive(Default)]
pub struct Restore {
    /// Ontvangen bytes.
    pub downloaded: Cell<u64>,
    /// Verwacht totaal; 0 als onbekend.
    pub total: Cell<u64>,
    /// Zolang de vorige start de schrijverlease nog houdt: wanneer die
    /// verloopt (Unix-milliseconden). 0 als er niet gewacht wordt.
    pub lease_until: Cell<u64>,
}
/// De deurbel: level-triggered en samengevoegd, tien bellen in één idle zijn
/// er één. Als [`Waker`] doet hij precies wat [`Mailbox::nudge`] doet, zodat
/// bytes voor een providerverzoek de eigenaar wekken in plaats van een
/// vloertimer.
pub(crate) struct Nudge {
    nudged: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}
// SAFETY: `Waker::from(Arc<W>)` eist Send + Sync, maar de bel wordt alleen
// aangeraakt door de eigenaar, zijn sockettaken en zijn providertaken, die
// samen op één executor op één core draaien (de host: één thread); er is geen
// tweede thread of core die hem kan zien.
#[allow(unsafe_code)]
unsafe impl Send for Nudge {}
// SAFETY: zie hierboven.
#[allow(unsafe_code)]
unsafe impl Sync for Nudge {}
impl Nudge {
    fn ring(&self) {
        self.nudged.set(true);
        let waker = self.waker.borrow_mut().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}
impl alloc::task::Wake for Nudge {
    fn wake(self: Arc<Self>) {
        self.ring();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.ring();
    }
}
impl Default for Mailbox {
    fn default() -> Self {
        Self {
            slots: Local(RefCell::new(core::array::from_fn(|_| Slot::default()))),
            health: Local(RefCell::new(None)),
            active: Cell::new(false),
            // Arc::new breekt af bij geheugengebrek; dat is aanvaard, alleen
            // hier bij de bouw van de ene mailbox per tenant, nooit per verzoek.
            bell: Arc::new(Nudge {
                nudged: Cell::new(false),
                waker: RefCell::new(None),
            }),
            restore: alloc::rc::Rc::new(Restore::default()),
            phase: Cell::new(("start", 0, true)),
            detail: RefCell::new(String::new()),
        }
    }
}
impl Mailbox {
    /// De fase van de eigenaar, sinds wanneer (ms), en of die stap nog loopt
    /// (`false`: hij is klaar en de eigenaar zit in wat erna komt).
    pub fn phase(&self) -> (&'static str, u64, bool) {
        self.phase.get()
    }
    /// Bij een verzoek: welke (methode en route, ids als `:id`).
    pub fn phase_detail(&self) -> String {
        self.detail.borrow().clone()
    }
    /// De voortgangsteller van een herstel; de opslag van de eigenaar telt erin.
    pub fn restore(&self) -> alloc::rc::Rc<Restore> {
        self.restore.clone()
    }
    /// Belt de eigenaar: een sockettaak legde iets in een slot, of een
    /// platformtaak rondde werk af. De eigenaar verlaat zijn idle in de
    /// volgende ronde.
    pub fn nudge(&self) {
        self.bell.ring();
    }
    /// De deurbel als waker, voor futures die de eigenaar zelf polt
    /// (providerverzoeken, de socketpomp): bytes bellen hem.
    pub fn waker(&self) -> Waker {
        Waker::from(self.bell.clone())
    }
    /// Wacht tot er gebeld is; een bel die tijdens het werk viel, wordt bij
    /// de volgende idle meteen gezien.
    pub fn nudged(&self) -> impl Future<Output = ()> + '_ {
        core::future::poll_fn(move |cx| {
            if self.bell.nudged.replace(false) {
                Poll::Ready(())
            } else {
                *self.bell.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    }
}
pub(crate) type Mail = Mailbox;
/// Pollable transport owns sockets only; it never borrows application state.
pub struct Transport<'a> {
    routing: routing::Selection<'a>,
    tasks: [Option<task::Task<'a>>; CONNECTIONS],
    secure: bool,
}
impl<'a> Transport<'a> {
    /// Create a fixed-capacity socket pool bound to this mailbox.
    pub fn new(mail: &'a Mailbox, secure: bool) -> Self {
        Self {
            routing: routing::Selection::Single(mail),
            tasks: core::array::from_fn(|_| None),
            secure,
        }
    }
    /// Create one global socket pool routed to isolated application mailboxes.
    pub fn routed(routes: &'a dyn Routing, secure: bool) -> Self {
        Self {
            routing: routing::Selection::Routed(routes),
            tasks: core::array::from_fn(|_| None),
            secure,
        }
    }
    /// Advance each socket once, even while its application owner is parked in Replica.
    pub fn poll<H: Platform + 'a>(
        &mut self,
        platform: &mut H,
        context: &mut Context<'_>,
    ) -> Result {
        if !self.routing.active() {
            return Ok(());
        }
        for index in 0..CONNECTIONS {
            if self.tasks[index].is_some() || self.routing.occupied(index) {
                continue;
            }
            let Some((socket, peer)) = platform.accept(context)? else {
                break;
            };
            match task::task(connection::<H>(
                socket,
                self.routing,
                index,
                peer,
                self.secure,
                platform.clock(),
            )) {
                Ok(task) => {
                    self.tasks[index] = Some(task);
                }
                Err(error) => H::log(format_args!("SPIN_CONNECTION_REJECTED error={error}")),
            }
        }
        for index in 0..CONNECTIONS {
            if self.tasks[index]
                .as_mut()
                .is_some_and(|task| task.as_mut().poll(context).is_ready())
            {
                self.tasks[index] = None;
                // Een vrijgekomen slot kan een wachtende verbinding aannemen:
                // nog een ronde, ook zonder tik.
                context.waker().wake_by_ref();
            }
        }
        Ok(())
    }
}
// Telt ook frames die de sockettaak al bezit maar nog niet heeft geschreven.
const FRAME_BUDGET: usize = 64 << 20;
fn queue_frame(slots: &mut [Slot; CONNECTIONS], index: usize, frame: Frame) -> bool {
    if slots[index].frame_bytes != 0
        || slots[index].close.is_some()
        || slots
            .iter()
            .map(|s| s.frame_bytes)
            .sum::<usize>()
            .saturating_add(frame.bytes.len())
            > FRAME_BUDGET
    {
        return false;
    }
    slots[index].frame_bytes = frame.bytes.len();
    slots[index].frame = Some(frame);
    true
}
fn http_alloc(_: spin_domain::Error) -> leanhttp::Error {
    leanhttp::Error::Alloc { bytes: 1 }
}
async fn connection<H: Platform>(
    socket: H::Connection,
    routes: routing::Selection<'_>,
    index: usize,
    peer: String,
    secure: bool,
    clock: H::Clock,
) {
    let (socket, restore_size) = match restore::prefix(socket).await {
        Ok(value) => value,
        Err(error) => {
            H::log(format_args!(
                "SPIN_HTTP_PREFACE_FAILED slot={index} error={error}"
            ));
            return;
        }
    };
    let claim = routing::Claim {
        mail: Cell::new(None),
        index,
    };
    let result = leanhttp::serve(
        socket,
        async |exchange: &mut leanhttp::Exchange<'_, restore::Prefixed<H::Connection>>| {
            let selected = routes
                .route(
                    exchange.req.header.get("Host").unwrap_or(""),
                    &exchange.req.path,
                    &exchange.req.method,
                )
                .or_else(|error| error.response().map(Route::Response))
                .map_err(|_| leanhttp::Error::Io(leanhttp::IoError::Other))?;
            let mail = match selected {
                Route::Owner(mail) => mail,
                Route::Response(response) => {
                    let head_only = exchange.req.method == "HEAD";
                    let head = response_head(&response)?;
                    let mut raw = exchange.hijack()?;
                    leanhttp::AsyncWrite::set_write_timeout(
                        &mut raw,
                        Some(core::time::Duration::from_secs(20)),
                    )?;
                    leanhttp::write_all(&mut raw, head.as_bytes()).await?;
                    if !head_only {
                        leanhttp::write_all(&mut raw, &response.body).await?;
                    }
                    leanhttp::flush(&mut raw).await?;
                    return Ok(());
                }
            };
            if let Some(size) = restore_size {
                claim.set(mail);
                return restore::serve(exchange, size, mail, index, &peer, secure, clock).await;
            }
            if exchange.req.path == "/healthz"
                && matches!(exchange.req.method.as_str(), "GET" | "HEAD")
            {
                let cached = {
                    let health = mail.health.0.borrow();
                    health
                        .as_ref()
                        .map(|response| -> leanhttp::Result<Vec<u8>> {
                            let head = response_head(response)?;
                            let length = head.len()
                                + if exchange.req.method == "HEAD" {
                                    0
                                } else {
                                    response.body.len()
                                };
                            let mut bytes = Vec::new();
                            bytes
                                .try_reserve_exact(length)
                                .map_err(|_| leanhttp::Error::Alloc { bytes: length })?;
                            bytes.extend_from_slice(head.as_bytes());
                            if exchange.req.method != "HEAD" {
                                bytes.extend_from_slice(&response.body);
                            }
                            Ok(bytes)
                        })
                        .transpose()?
                };
                if let Some(bytes) = cached {
                    let mut raw = exchange.hijack()?;
                    leanhttp::AsyncWrite::set_write_timeout(
                        &mut raw,
                        Some(core::time::Duration::from_secs(20)),
                    )?;
                    leanhttp::write_all(&mut raw, &bytes).await?;
                    return Ok(());
                }
            }
            if assets::serve(exchange).await? {
                return Ok(());
            }
            let runner = exchange.req.path == "/api/runner/ws";
            let browser = exchange.req.path.ends_with("/terminal")
                || (exchange.req.path.starts_with("/api/sessions/")
                    && exchange.req.path.ends_with("/acp"));
            let accept = if exchange.req.path == "/api/state/ws" || runner || browser {
                match crate::websocket::handshake(&exchange.req) {
                    Ok(accept) => Some(accept),
                    Err(_) => {
                        exchange.error(400, "invalid WebSocket handshake").await?;
                        return Ok(());
                    }
                }
            } else {
                None
            };
            let body = exchange.read_body_to_end().await?;
            let mut headers = Vec::new();
            for (key, value) in exchange.req.header.iter() {
                headers
                    .try_reserve(1)
                    .map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
                headers.push((
                    try_string(key).map_err(http_alloc)?,
                    try_string(value).map_err(http_alloc)?,
                ));
            }
            let input = Input {
                method: try_string(&exchange.req.method).map_err(http_alloc)?,
                path: try_string(&exchange.req.path).map_err(http_alloc)?,
                raw_query: try_string(&exchange.req.raw_query).map_err(http_alloc)?,
                headers,
                body,
                peer: try_string(&peer).map_err(http_alloc)?,
                secure,
            };
            claim.set(mail);
            {
                let mut slots = mail.slots.0.borrow_mut();
                slots[index].routed = true;
                slots[index].queued_ms = clock.millis();
                slots[index].request = Some(input);
            }
            mail.nudge();
            let response = core::future::poll_fn(|_| {
                let mut slots = mail.slots.0.borrow_mut();
                if slots[index].abort {
                    return Poll::Ready(Err(leanhttp::Error::Io(leanhttp::IoError::Other)));
                }
                if slots[index].upgrade {
                    slots[index].upgrade = false;
                    return Poll::Ready(Ok(None));
                }
                match slots[index].response.take() {
                    Some(response) => Poll::Ready(Ok(Some(response))),
                    None => Poll::Pending,
                }
            })
            .await?;
            let Some(response) = response else {
                if let Some(accept) = accept {
                    return crate::websocket::serve(
                        exchange, mail, index, &accept, runner, browser, clock,
                    )
                    .await;
                }
                return Err(leanhttp::Error::Io(leanhttp::IoError::Other));
            };
            // leanhttp alpha's antwoordschrijver vouwt herhaalde headers samen,
            // maar Set-Cookie mag niet worden gevouwen. De parser blijft van lean;
            // deze afgebakende schrijver verstuurt beide cookies en sluit daarna.
            let head_only = exchange.req.method == "HEAD";
            let head = response_head(&response)?;
            let mut raw = exchange.hijack()?;
            leanhttp::AsyncWrite::set_write_timeout(
                &mut raw,
                Some(core::time::Duration::from_secs(20)),
            )?;
            leanhttp::write_all(&mut raw, head.as_bytes()).await?;
            if !head_only && response.status != 204 && response.status != 304 {
                leanhttp::write_all(&mut raw, &response.body).await?;
                let streaming = { mail.slots.0.borrow()[index].streaming };
                if streaming {
                    loop {
                        mail.slots.0.borrow_mut()[index].next_chunk = true;
                        mail.nudge();
                        let chunk = core::future::poll_fn(|_| {
                            let mut slots = mail.slots.0.borrow_mut();
                            if slots[index].abort {
                                return Poll::Ready(Err(leanhttp::Error::Io(
                                    leanhttp::IoError::Other,
                                )));
                            }
                            match slots[index].chunk.take() {
                                Some(bytes) => Poll::Ready(Ok(bytes)),
                                None => Poll::Pending,
                            }
                        })
                        .await?;
                        if chunk.is_empty() {
                            break;
                        }
                        leanhttp::write_all(&mut raw, &chunk).await?;
                    }
                }
            }
            leanhttp::flush(&mut raw).await?;
            Ok(())
        },
    )
    .await;
    if let Err(error) = result {
        H::log(format_args!("SPIN_HTTP_CLOSED slot={index} error={error}"));
    }
}
fn make_response<H: Platform>(result: spin_server::Result<Response>) -> Option<Response> {
    match result {
        Ok(response) => Some(response),
        Err(error) => {
            H::log(format_args!("SPIN_REQUEST_FAILED error={error}"));
            error.response().ok()
        }
    }
}
/// Draait de vaste pool op de unieke opslag-eigenaar; Drop sluit alle sockets.
/// De native schil parkeert deze eigenaar op zijn SQLite-stack tijdens I/O.
pub fn serve<P: Persistence, H: Platform>(
    platform: H,
    server: &mut Server<P>,
    runtime: &mut impl Runtime,
    secure: bool,
) -> Result {
    let mail = Mailbox::default();
    let mut transport = Transport::new(&mail, secure);
    serve_inner(platform, server, runtime, &mail, |platform, context| {
        transport.poll(platform, context)
    })
}
/// Run only the application owner. A separate transport must poll the same mailbox.
pub fn serve_owner<P: Persistence, H: Platform>(
    platform: H,
    server: &mut Server<P>,
    runtime: &mut impl Runtime,
    mail: &Mailbox,
) -> Result {
    serve_inner(platform, server, runtime, mail, |_, _| Ok(()))
}
fn serve_inner<P: Persistence, H: Platform>(
    mut platform: H,
    server: &mut Server<P>,
    runtime: &mut impl Runtime,
    mail: &Mailbox,
    mut pump: impl FnMut(&mut H, &mut Context<'_>) -> Result,
) -> Result {
    let clock = platform.clock();
    let waker = mail.waker();
    let mut meter = Meter::new(clock, &mail.phase, &mail.detail);
    let mut outgoing = outbound::Pool::new();
    mail.active.set(true);
    let mut passwords: Vec<Option<PasswordWork>> = Vec::new();
    passwords.try_reserve_exact(CONNECTIONS).map_err(boundary)?;
    passwords.resize_with(CONNECTIONS, || None);
    let mut watches: [Option<Watch>; CONNECTIONS] = core::array::from_fn(|_| None);
    let mut runners: [Option<RunnerLink>; CONNECTIONS] = core::array::from_fn(|_| None);
    let mut network: [Option<spin_server::NetworkWait>; CONNECTIONS] =
        core::array::from_fn(|_| None);
    let mut uploads: [Option<UploadWait>; CONNECTIONS] = core::array::from_fn(|_| None);
    let mut operations: [Option<spin_server::OperationWait>; CONNECTIONS] =
        core::array::from_fn(|_| None);
    let mut operation_routes: [Option<(String, String)>; CONNECTIONS] =
        core::array::from_fn(|_| None);
    let mut backups: [Option<spin_server::BackupWait>; CONNECTIONS] =
        core::array::from_fn(|_| None);
    let mut capsules: [Option<CapsuleWait>; CONNECTIONS] = core::array::from_fn(|_| None);
    let mut terminals: [Option<TerminalLink>; CONNECTIONS] = core::array::from_fn(|_| None);
    let mut chats: [Option<ChatLink>; CONNECTIONS] = core::array::from_fn(|_| None);
    let started = clock.millis();
    let mut maintained = clock.millis();
    // De interface gaat voor: wanneer een browser het laatst iets vroeg, en
    // wanneer het opslagonderhoud (met de Replica-capture) het laatst liep.
    let mut interface_at = 0_u64;
    let mut stored = clock.millis();
    let mut bulk_cursor = 0_usize;
    let mut bulk_left;
    let mut capsule_diagnosed = None;
    let mut health_updated = clock.millis();
    // De socketpomp en de providerpool pollen met de deurbel als waker: wat
    // hen wekt, wekt de eigenaar.
    let mut context = Context::from_waker(&waker);
    while !platform.stopped() {
        meter.round();
        let now = platform.timestamp()?;
        let refresh_health = mail.health.0.borrow().is_none()
            || clock.millis().saturating_sub(health_updated) >= 1000;
        if refresh_health
            && let Outcome::Response(response) = server.begin(
                Request {
                    method: "GET",
                    path: "/healthz",
                    raw_query: "",
                    headers: &[],
                    body: &[],
                    peer: "",
                    secure: false,
                },
                &now,
                runtime,
            )?
        {
            *mail.health.0.borrow_mut() = Some(response);
            health_updated = clock.millis();
        }
        meter.lap::<H>("health");
        if let Err(error) = server.maintain_restores(&now, runtime) {
            H::log(format_args!("SPIN_RESTORE_FAILED error={error}"));
        }
        meter.lap::<H>("maintain_restores");
        if !server.backup_active() {
            if let Err(error) = server.maintain_operations(&now, runtime) {
                H::log(format_args!("SPIN_OPERATION_FAILED error={error}"));
            }
            meter.lap::<H>("maintain_operations");
            outgoing.poll::<H, P>(&platform, server, &now, runtime, &waker)?;
            meter.lap::<H>("outgoing");
            if let Err(error) = server.maintain_agents(&now, runtime) {
                H::log(format_args!("SPIN_AGENT_MAINTENANCE_FAILED error={error}"));
            }
            meter.lap::<H>("maintain_agents");
            if let Err(error) = server.maintain_uploads(&now) {
                H::log(format_args!("SPIN_UPLOAD_MAINTENANCE_FAILED error={error}"));
            }
            meter.lap::<H>("maintain_uploads");
        }
        let now_ms = clock.millis().saturating_sub(started);
        pump(&mut platform, &mut context)?;
        for index in 0..CONNECTIONS {
            let finished = { mail.slots.0.borrow()[index].finished };
            if finished {
                if let Some(wait) = backups[index].take() {
                    server.finish_backup(&wait)?;
                }
                passwords[index] = None;
                uploads[index] = None;
                operations[index] = None;
                operation_routes[index] = None;
                network[index] = None;
                watches[index] = None;
                chats[index] = None;
                if let Some(link) = terminals[index].take()
                    && let Err(error) = server.terminal_disconnect(link)
                {
                    H::log(format_args!("SPIN_TERMINAL_CLOSE_FAILED error={error}"));
                }
                if let Some(wait) = capsules[index].take() {
                    server.detach_capsule(wait);
                }
                if let Some(link) = runners[index].take()
                    && let Err(error) = server.runner_disconnect(link, &now)
                {
                    H::log(format_args!("SPIN_RUNNER_DISCONNECT_FAILED error={error}"));
                }
                mail.slots.0.borrow_mut()[index] = Slot::default();
            }
        }
        let (order, browser, left) = {
            let slots = mail.slots.0.borrow();
            request_order(&slots, &mut bulk_cursor)
        };
        bulk_left = left;
        if browser {
            interface_at = clock.millis();
        }
        for index in order.into_iter().flatten() {
            let input = {
                let mut slots = mail.slots.0.borrow_mut();
                let queued_ms = slots[index].queued_ms;
                slots[index].request.take().map(|input| (input, queued_ms))
            };
            if let Some((input, queued_ms)) = input {
                meter.queued::<H>(clock.millis().saturating_sub(queued_ms));
                let mut headers = Vec::new();
                headers
                    .try_reserve_exact(input.headers.len())
                    .map_err(boundary)?;
                for (key, value) in &input.headers {
                    headers.push((key.as_str(), value.as_str()));
                }
                let request = Request {
                    method: &input.method,
                    path: &input.path,
                    raw_query: &input.raw_query,
                    headers: &headers,
                    body: &input.body,
                    peer: &input.peer,
                    secure: input.secure,
                };
                meter.mark_request(&route_label(&input.method, &input.path));
                let result = server.begin(request, &now, runtime);
                meter.lap::<H>("request");
                if result.is_err() {
                    // Never include query strings, credentials or public share tokens.
                    let path = if input.path.starts_with("/api/jobs/")
                        || input.path.starts_with("/api/sessions/")
                        || input.path.starts_with("/api/workflow/mcp/")
                    {
                        input.path.as_str()
                    } else {
                        "[other route]"
                    };
                    H::log(format_args!(
                        "SPIN_REQUEST_CONTEXT method={} path={path:?}",
                        input.method
                    ));
                }
                let response = match result {
                    Ok(Outcome::Download(download)) => {
                        backups[index] = Some(download.wait);
                        mail.slots.0.borrow_mut()[index].streaming = true;
                        Some(download.response)
                    }
                    Ok(Outcome::Operation(wait)) => {
                        operations[index] = Some(wait);
                        operation_routes[index] = Some((input.method, input.path));
                        continue;
                    }
                    Ok(Outcome::Network(wait)) => {
                        network[index] = Some(wait);
                        continue;
                    }
                    Ok(Outcome::Upload(wait)) => {
                        uploads[index] = Some(wait);
                        continue;
                    }
                    Ok(Outcome::Chat(link)) => {
                        chats[index] = Some(link);
                        mail.slots.0.borrow_mut()[index].upgrade = true;
                        continue;
                    }
                    Ok(Outcome::Terminal(link)) => {
                        terminals[index] = Some(link);
                        mail.slots.0.borrow_mut()[index].upgrade = true;
                        continue;
                    }
                    Ok(Outcome::Capsule(wait)) => {
                        capsules[index] = Some(wait);
                        continue;
                    }
                    Ok(Outcome::Password(work))
                        if passwords.iter().filter(|p| p.is_some()).count() < PASSWORD_TASKS =>
                    {
                        passwords[index] = Some(work);
                        continue;
                    }
                    Ok(Outcome::Password(_)) => make_response::<H>(Err(spin_server::Error::Http(
                        503,
                        "password worker capacity reached",
                    ))),
                    Ok(Outcome::State(mut watch)) => {
                        match server.state_for_watch(&mut watch, &now).and_then(|json| {
                            if json.len() > MAX_STATE_FRAME {
                                return Err(spin_server::Error::Http(
                                    503,
                                    "state frame exceeds budget",
                                ));
                            }
                            spin_core::websocket::encode(1, json.as_bytes(), None).map_err(|_| {
                                spin_server::Error::Http(500, "state frame unavailable")
                            })
                        }) {
                            Ok(frame) => {
                                watches[index] = Some(Watch {
                                    watch,
                                    version: server.version(),
                                    sent: clock.millis(),
                                    checked: clock.millis(),
                                });
                                let mut slots = mail.slots.0.borrow_mut();
                                slots[index].upgrade = true;
                                if !queue_frame(
                                    &mut slots,
                                    index,
                                    Frame {
                                        bytes: frame,
                                        ticket: None,
                                    },
                                ) {
                                    slots[index].upgrade = false;
                                    watches[index] = None;
                                    slots[index].response =
                                        make_response::<H>(Err(spin_server::Error::Http(
                                            503,
                                            "WebSocket buffer capacity reached",
                                        )));
                                }
                                continue;
                            }
                            Err(error) => make_response::<H>(Err(error)),
                        }
                    }
                    Ok(Outcome::Runner(link))
                        if runners.iter().filter(|r| r.is_some()).count() < 8 =>
                    {
                        runners[index] = Some(link);
                        mail.slots.0.borrow_mut()[index].upgrade = true;
                        continue;
                    }
                    Ok(Outcome::Runner(_)) => make_response::<H>(Err(spin_server::Error::Http(
                        503,
                        "runner connection capacity reached",
                    ))),
                    Ok(Outcome::Response(response)) => Some(response),
                    Err(error) => make_response::<H>(Err(error)),
                };
                if let Some(response) = response {
                    mail.slots.0.borrow_mut()[index].response = Some(response);
                } else {
                    mail.slots.0.borrow_mut()[index].abort = true;
                }
            }
        }
        for (index, backup) in backups.iter_mut().enumerate() {
            let Some(wait) = backup else {
                continue;
            };
            if !mail.slots.0.borrow()[index].next_chunk {
                continue;
            }
            mail.slots.0.borrow_mut()[index].next_chunk = false;
            meter.mark("backup_chunk");
            match server.backup_chunk(wait, &now) {
                Ok(Some(bytes)) => mail.slots.0.borrow_mut()[index].chunk = Some(bytes),
                Ok(None) => {
                    server.finish_backup(wait)?;
                    mail.slots.0.borrow_mut()[index].chunk = Some(Vec::new());
                    *backup = None;
                }
                Err(error) => {
                    H::log(format_args!("SPIN_BACKUP_FAILED error={error}"));
                    server.finish_backup(wait)?;
                    mail.slots.0.borrow_mut()[index].abort = true;
                    *backup = None;
                }
            }
            meter.lap::<H>("backup_chunk");
        }
        for (index, waiting) in uploads.iter_mut().enumerate() {
            let Some(wait) = waiting else {
                continue;
            };
            meter.mark("poll_upload");
            let polled = server.poll_upload(wait);
            meter.lap::<H>("poll_upload");
            let response = match polled {
                Ok(None) => continue,
                Ok(Some(response)) => Some(response),
                Err(error) => make_response::<H>(Err(error)),
            };
            *waiting = None;
            if let Some(response) = response {
                mail.slots.0.borrow_mut()[index].response = Some(response);
            } else {
                mail.slots.0.borrow_mut()[index].abort = true;
            }
        }
        if server.backup_active() {
            // De export stroomt per chunk op de deurbel van zijn socket; de
            // seconde is de bovengrens voor de health-cache.
            meter.finish::<H>();
            platform.idle(mail, Idle::Until(clock.millis().saturating_add(1000)))?;
            continue;
        }
        for (index, link) in terminals.iter().enumerate() {
            let Some(link) = link else {
                continue;
            };
            if mail.slots.0.borrow()[index].close.is_some() {
                continue;
            }
            let result = (|| -> spin_server::Result {
                server.validate_terminal(link, &now)?;
                let message = mail.slots.0.borrow_mut()[index].browser.take();
                if let Some(message) = message {
                    server.terminal_message(link, &message, runtime)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                H::log(format_args!(
                    "SPIN_TERMINAL_REJECTED slot={index} error={error}"
                ));
                if server.terminal_error(link, &error).is_err() {
                    H::log(format_args!(
                        "SPIN_WS_CLOSED kind=terminal code=1008 error={error}"
                    ));
                    mail.slots.0.borrow_mut()[index].close = Some(1008);
                    continue;
                }
            }
            if mail.slots.0.borrow()[index].frame_bytes == 0 {
                if let Some(json) = server.terminal_next(link) {
                    let bytes =
                        spin_core::websocket::encode(1, json.as_bytes(), None).map_err(boundary)?;
                    if queue_frame(
                        &mut mail.slots.0.borrow_mut(),
                        index,
                        Frame {
                            bytes,
                            ticket: None,
                        },
                    ) {
                        server.terminal_acknowledge(link);
                    }
                } else if server.terminal_done(link) {
                    mail.slots.0.borrow_mut()[index].close = Some(1000);
                }
            }
        }
        for (index, link) in chats.iter_mut().enumerate() {
            let Some(link) = link else {
                continue;
            };
            if mail.slots.0.borrow()[index].close.is_some() {
                continue;
            }
            if let Err(error) = server.validate_chat(link, &now) {
                H::log(format_args!(
                    "SPIN_WS_CLOSED kind=chat code=1008 error={error}"
                ));
                mail.slots.0.borrow_mut()[index].close = Some(1008);
                continue;
            }
            let message = mail.slots.0.borrow_mut()[index].browser.take();
            if let Some(message) = message
                && let Err(error) = server.chat_message(link, &message, &now)
            {
                H::log(format_args!(
                    "SPIN_CHAT_REJECTED slot={index} error={error}"
                ));
                if server.chat_error(link, &error).is_err() {
                    H::log(format_args!(
                        "SPIN_WS_CLOSED kind=chat code=1008 error={error}"
                    ));
                    mail.slots.0.borrow_mut()[index].close = Some(1008);
                    continue;
                }
            }
            if mail.slots.0.borrow()[index].frame_bytes == 0 {
                match server.chat_next(link) {
                    Ok(Some((sequence, json))) => {
                        let bytes = spin_core::websocket::encode(1, json.as_bytes(), None)
                            .map_err(boundary)?;
                        if queue_frame(
                            &mut mail.slots.0.borrow_mut(),
                            index,
                            Frame {
                                bytes,
                                ticket: None,
                            },
                        ) {
                            server.chat_acknowledge(link, sequence);
                        }
                    }
                    Ok(None) if server.chat_done(link) => {
                        mail.slots.0.borrow_mut()[index].close = Some(1000)
                    }
                    Err(error) => {
                        H::log(format_args!(
                            "SPIN_WS_CLOSED kind=chat code=1011 error={error}"
                        ));
                        mail.slots.0.borrow_mut()[index].close = Some(1011);
                    }
                    _ => {}
                }
            }
        }
        for (index, waiting) in network.iter_mut().enumerate() {
            let Some(wait) = waiting else { continue };
            let response = match server.poll_network(wait) {
                Ok(None) => continue,
                Ok(Some(response)) => Some(response),
                Err(error) => make_response::<H>(Err(error)),
            };
            *waiting = None;
            if let Some(response) = response {
                mail.slots.0.borrow_mut()[index].response = Some(response);
            } else {
                mail.slots.0.borrow_mut()[index].abort = true;
            }
        }
        for (index, waiting) in operations.iter_mut().enumerate() {
            let Some(wait) = waiting else { continue };
            let response = match server.poll_operation(wait) {
                Ok(None) => continue,
                Ok(Some(response)) => Some(response),
                Err(error) => make_response::<H>(Err(error)),
            };
            *waiting = None;
            if let Some((method, path)) = operation_routes[index].take() {
                H::log(format_args!(
                    "SPIN_OPERATION_RESPONSE method={method} path={path:?} status={}",
                    response.as_ref().map_or(500, |response| response.status)
                ));
            }
            if let Some(response) = response {
                mail.slots.0.borrow_mut()[index].response = Some(response);
            } else {
                mail.slots.0.borrow_mut()[index].abort = true;
            }
        }
        for (index, waiting) in capsules.iter_mut().enumerate() {
            let Some(wait) = waiting else { continue };
            let response = match server.poll_capsule(wait, &now) {
                Ok(None) => continue,
                Ok(Some(response)) => Some(response),
                Err(error) => make_response::<H>(Err(error)),
            };
            *waiting = None;
            if let Some(response) = response {
                mail.slots.0.borrow_mut()[index].response = Some(response);
            } else {
                mail.slots.0.borrow_mut()[index].abort = true;
            }
        }
        for (index, link) in runners.iter_mut().enumerate() {
            let Some(link) = link else { continue };
            if mail.slots.0.borrow()[index].close.is_some() {
                continue;
            }
            let result = (|| -> spin_server::Result {
                server.validate_runner(link, &now)?;
                let (message, acknowledged, touched) = {
                    let mut slots = mail.slots.0.borrow_mut();
                    let slot = &mut slots[index];
                    (
                        slot.incoming.take(),
                        slot.acknowledged.take(),
                        core::mem::take(&mut slot.touched),
                    )
                };
                if touched {
                    server.runner_touch(link, now_ms)?;
                }
                if let Some(ticket) = acknowledged {
                    server.runner_acknowledge(link, ticket);
                }
                if let Some(message) = message {
                    match server.runner_message(link, message, &now, now_ms, runtime)? {
                        RunnerEvent::Goodbye => mail.slots.0.borrow_mut()[index].close = Some(1000),
                        RunnerEvent::Attached { client, .. } => {
                            H::log(format_args!("SPIN_RUNNER_ATTACHED client={client}"))
                        }
                        RunnerEvent::Response { request, response }
                            if !response.error.is_empty() =>
                        {
                            H::log(format_args!(
                                "SPIN_RUNNER_REQUEST_FAILED id={} method={} error={}",
                                request.id, request.method, response.error
                            ));
                        }
                        _ => {}
                    }
                }
                if mail.slots.0.borrow()[index].frame_bytes == 0
                    && let Some((ticket, message)) = server.runner_next(link)?
                {
                    let json = spin_domain::Wire::to_json(message)?;
                    if json.len() > spin_domain::protocol::MAX_MESSAGE_BYTES {
                        return Err(spin_server::Error::Http(503, "runner frame too large"));
                    }
                    let bytes = spin_core::websocket::encode(1, json.as_bytes(), None)
                        .map_err(|_| spin_server::Error::Http(503, "runner frame unavailable"))?;
                    queue_frame(
                        &mut mail.slots.0.borrow_mut(),
                        index,
                        Frame {
                            bytes,
                            ticket: Some(ticket),
                        },
                    );
                }
                Ok(())
            })();
            if let Err(error) = result {
                // 1008 alleen voor een geweigerde identiteit, 1012 als een
                // nieuwere verbinding deze verving, 1011 voor al het andere.
                let code = match error {
                    spin_server::Error::Http(401 | 403, _) => 1008,
                    spin_server::Error::Http(
                        409,
                        "runner identity is already connected from another process",
                    ) => 1008,
                    spin_server::Error::Http(409, "runner connection was replaced") => 1012,
                    _ => 1011,
                };
                H::log(format_args!("SPIN_RUNNER_CLOSED code={code} error={error}"));
                mail.slots.0.borrow_mut()[index].close = Some(code);
            }
        }
        // Hoogstens 4096 PBKDF2-rondes per actorronde, over alle logins samen.
        let pending = passwords.iter().filter(|p| p.is_some()).count().max(1);
        let rounds = 4096 / u32::try_from(pending).map_err(boundary)?;
        for (index, password) in passwords.iter_mut().enumerate() {
            if password.as_mut().is_some_and(|work| work.step(rounds))
                && let Some(work) = password.take()
            {
                meter.mark("finish_password");
                let finished = server.finish_password(work, &now, runtime);
                meter.lap::<H>("finish_password");
                if let Some(response) = make_response::<H>(finished) {
                    mail.slots.0.borrow_mut()[index].response = Some(response);
                } else {
                    mail.slots.0.borrow_mut()[index].abort = true;
                }
            }
        }
        for (index, watch) in watches.iter_mut().enumerate() {
            let Some(watch) = watch else {
                continue;
            };
            if clock.millis().saturating_sub(watch.checked) >= 3000 {
                watch.checked = clock.millis();
                if let Err(error) = server.validate_watch(&watch.watch, &now) {
                    H::log(format_args!(
                        "SPIN_WS_CLOSED kind=watch code=1008 error={error}"
                    ));
                    mail.slots.0.borrow_mut()[index].close = Some(1008);
                    continue;
                }
            }
            if watch.version != server.version()
                && clock.millis().saturating_sub(watch.sent) >= 150
                && mail.slots.0.borrow()[index].frame_bytes == 0
                && mail.slots.0.borrow()[index].close.is_none()
            {
                meter.mark("watch_push");
                let (code, error) = match server.state_for_watch(&mut watch.watch, &now) {
                    Ok(json) if json.len() > MAX_STATE_FRAME => {
                        (1008, "state frame exceeds budget")
                    }
                    Ok(json) => match spin_core::websocket::encode(1, json.as_bytes(), None) {
                        Ok(bytes) => {
                            if queue_frame(
                                &mut mail.slots.0.borrow_mut(),
                                index,
                                Frame {
                                    bytes,
                                    ticket: None,
                                },
                            ) {
                                watch.version = server.version();
                                watch.sent = clock.millis();
                                meter.pushes += 1;
                            }
                            (0, "")
                        }
                        Err(_) => (1011, "state frame unavailable"),
                    },
                    Err(error) => {
                        H::log(format_args!(
                            "SPIN_WS_CLOSED kind=watch code=1008 error={error}"
                        ));
                        (1008, "")
                    }
                };
                meter.lap::<H>("watch_push");
                if code != 0 {
                    if !error.is_empty() {
                        H::log(format_args!(
                            "SPIN_WS_CLOSED kind=watch code={code} error={error}"
                        ));
                    }
                    mail.slots.0.borrow_mut()[index].close = Some(code);
                }
            }
        }
        // Het onderhoud van elke seconde komt ná de verzoeken van deze ronde: een
        // verzoek dat de eigenaar uit zijn rust belt, wacht niet op de opslagtelling
        // of de Replica-capture.
        if !server.backup_active() && clock.millis().saturating_sub(maintained) >= 1000 {
            meter.mark("maintain_storage");
            // Een capture houdt de eigenaar seconden vast; terwijl een browser
            // bezig is, wacht hij, maar nooit langer dan 30 s (de lease).
            let at = clock.millis();
            let defer =
                at.saturating_sub(interface_at) < 3_000 && at.saturating_sub(stored) < 30_000;
            if !defer {
                stored = at;
            }
            if let Err(error) = if defer {
                Ok(())
            } else {
                server.maintain_storage(&now)
            } {
                H::log(format_args!(
                    "SPIN_STORAGE_MAINTENANCE_FAILED error={error}"
                ));
                // Onzekere opslag (een verloren lease, een onbevestigde commit)
                // herstelt alleen door opnieuw te openen: de eigenaar stopt, de
                // boot-schil of Hop start hem opnieuw.
                if matches!(
                    error,
                    spin_server::Error::Store(spin_store::Error::StorageUncertain(_))
                ) {
                    H::log(format_args!("SPIN_OWNER_STOPPED reason=storage_uncertain"));
                    return Err(error);
                }
            }
            meter.lap::<H>("maintain_storage");
            if let Err(error) = server.maintain_capsules(&now, runtime) {
                H::log(format_args!(
                    "SPIN_CAPSULE_MAINTENANCE_FAILED error={error}"
                ));
                if capsule_diagnosed
                    .is_none_or(|last| clock.millis().saturating_sub(last) >= 30_000)
                {
                    server.capsule_diagnostics(
                        now.time().map_or(0, |time| time.0 / 1_000_000),
                        H::log,
                    );
                    capsule_diagnosed = Some(clock.millis());
                }
            }
            // Wachtende capsules, weigeringen en gefaalde agents: de server vraagt
            // zelf om zijn regels (hoogstens eens per minuut per geval).
            if server.take_diagnostics_due() {
                server.capsule_diagnostics(now.time().map_or(0, |time| time.0 / 1_000_000), H::log);
                capsule_diagnosed = Some(clock.millis());
            }
            meter.lap::<H>("maintain_capsules");
            // A slow maintenance round must leave an interval for queued requests.
            maintained = clock.millis();
        }
        // Alleen gesneden CPU-werk (een wachtwoordafleiding) vraagt meteen de
        // volgende ronde; verder slaapt de eigenaar tot de deurbel of zijn
        // vroegste echte deadline, hooguit een seconde verder: het onderhoud,
        // de health-cache, en per watch de controle en de uitgestelde push.
        let at = clock.millis();
        let mut until = at
            .saturating_add(1000)
            .min(maintained.saturating_add(1000))
            .min(health_updated.saturating_add(1000));
        for watch in watches.iter().flatten() {
            until = until.min(watch.checked.saturating_add(3000));
            if watch.version != server.version() {
                until = until.min(watch.sent.saturating_add(150));
            }
        }
        let next = if passwords.iter().any(Option::is_some) || server.restore_active() || bulk_left
        {
            Idle::Yield
        } else {
            Idle::Until(until)
        };
        meter.finish::<H>();
        platform.idle(mail, next)?;
    }
    Ok(())
}
/// Methode en pad voor de log, zonder query en met elk id- of tokenachtig
/// segment als `:id`: alleen korte segmenten van kleine letters en `-` blijven.
fn route_label(method: &str, path: &str) -> String {
    let mut out = String::new();
    let _ = out.try_reserve(method.len() + path.len().min(96) + 1);
    out.push_str(method);
    out.push(' ');
    for segment in path.split('/').skip(1).take(8) {
        out.push('/');
        if !segment.is_empty()
            && segment.len() <= 24
            && segment.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
        {
            out.push_str(segment);
        } else if !segment.is_empty() {
            out.push_str(":id");
        }
    }
    out
}
/// Laagdata en uploads: bulk, die wacht op de interface.
fn bulk(path: &str) -> bool {
    path == "/api/uploads"
        || path.starts_with("/api/uploads/")
        || path.starts_with("/api/snapshots/")
        || path.starts_with("/api/blobs/")
}
/// De volgorde van de verzoeken deze ronde: eerst alle gewone, daarna
/// hoogstens één bulkverzoek (beurtelings over de slots). Ook: of er een
/// browserverzoek (met cookie) bij zat, en of er bulk blijft liggen.
fn request_order(
    slots: &[Slot; CONNECTIONS],
    cursor: &mut usize,
) -> ([Option<usize>; CONNECTIONS], bool, bool) {
    let mut order = [None; CONNECTIONS];
    let mut next = 0;
    let mut browser = false;
    for (index, slot) in slots.iter().enumerate() {
        if let Some(input) = &slot.request
            && !bulk(&input.path)
        {
            browser |= input
                .headers
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("cookie"));
            order[next] = Some(index);
            next += 1;
        }
    }
    let mut waiting = 0;
    let start = *cursor;
    for offset in 0..CONNECTIONS {
        let index = (start + offset) % CONNECTIONS;
        if slots[index].request.as_ref().is_some_and(|i| bulk(&i.path)) {
            if waiting == 0 && next < CONNECTIONS {
                order[next] = Some(index);
                *cursor = (index + 1) % CONNECTIONS;
            }
            waiting += 1;
        }
    }
    (order, browser, waiting > 1)
}
/// Een stap of wachttijd vanaf deze duur krijgt zijn eigen regel.
const SLOW_MS: u64 = 200;
/// Meting van de eigenaar per taak (apps.md, "Meet per taak"): bezette tijd,
/// de langste stap met naam en de langste wachttijd in de rij, elke 30 s één
/// regel. Niets hierin alloceert.
struct Meter<'m, K: Clock> {
    clock: K,
    phase: &'m Cell<(&'static str, u64, bool)>,
    detail: &'m RefCell<String>,
    since: u64,
    round_at: u64,
    lap_at: u64,
    busy_ms: u64,
    rounds: u64,
    requests: u64,
    pushes: u64,
    longest: (&'static str, u64),
    queue_max_ms: u64,
}
impl<'m, K: Clock> Meter<'m, K> {
    fn new(
        clock: K,
        phase: &'m Cell<(&'static str, u64, bool)>,
        detail: &'m RefCell<String>,
    ) -> Self {
        let now = clock.millis();
        Self {
            clock,
            phase,
            detail,
            since: now,
            round_at: now,
            lap_at: now,
            busy_ms: 0,
            rounds: 0,
            requests: 0,
            pushes: 0,
            longest: ("", 0),
            queue_max_ms: 0,
        }
    }
    /// Begin van een ronde, direct na de idle.
    fn round(&mut self) {
        self.round_at = self.clock.millis();
        self.lap_at = self.round_at;
        self.phase.set(("woke", self.round_at, true));
    }
    /// Zet het beginpunt van de volgende stap en noemt hem.
    fn mark(&mut self, step: &'static str) {
        self.lap_at = self.clock.millis();
        self.phase.set((step, self.lap_at, true));
        self.detail.borrow_mut().clear();
    }
    /// Het beginpunt van een verzoek, met zijn label voor de fasregel.
    fn mark_request(&mut self, label: &str) {
        self.mark("request");
        let mut detail = self.detail.borrow_mut();
        if detail.try_reserve(label.len()).is_ok() {
            detail.push_str(label);
        }
    }
    /// Sluit de stap af die bij de laatste `mark` of `lap` begon.
    fn lap<H: Platform>(&mut self, step: &'static str) {
        let now = self.clock.millis();
        let ms = now.saturating_sub(self.lap_at);
        self.lap_at = now;
        self.phase.set((step, now, false));
        if ms > self.longest.1 {
            self.longest = (step, ms);
        }
        if ms >= SLOW_MS {
            let detail = self.detail.borrow();
            if detail.is_empty() {
                H::log(format_args!("SPIN_OWNER_SLOW step={step} ms={ms}"));
            } else {
                H::log(format_args!(
                    "SPIN_OWNER_SLOW step={step} route={detail} ms={ms}"
                ));
            }
        }
        self.detail.borrow_mut().clear();
    }
    /// Een verzoek lag `ms` in de rij voordat de eigenaar hem pakte.
    fn queued<H: Platform>(&mut self, ms: u64) {
        self.requests += 1;
        self.queue_max_ms = self.queue_max_ms.max(ms);
        if ms >= SLOW_MS {
            H::log(format_args!("SPIN_OWNER_QUEUE ms={ms}"));
        }
    }
    /// Einde van de ronde, vlak voor de idle: telt de bezette tijd en schrijft
    /// elke 30 s de samenvatting.
    fn finish<H: Platform>(&mut self) {
        let now = self.clock.millis();
        self.phase.set(("idle", now, true));
        self.busy_ms = self
            .busy_ms
            .saturating_add(now.saturating_sub(self.round_at));
        self.rounds += 1;
        if now.saturating_sub(self.since) < 30_000 {
            return;
        }
        H::log(format_args!(
            "SPIN_OWNER_LOAD rounds={} requests={} pushes={} busy_ms={} longest={}:{} queue_max_ms={}",
            self.rounds,
            self.requests,
            self.pushes,
            self.busy_ms,
            self.longest.0,
            self.longest.1,
            self.queue_max_ms
        ));
        self.since = now;
        self.busy_ms = 0;
        self.rounds = 0;
        self.requests = 0;
        self.pushes = 0;
        self.longest = ("", 0);
        self.queue_max_ms = 0;
    }
}

pub(crate) fn response_head(response: &Response) -> leanhttp::Result<String> {
    use core::fmt::Write;
    let mut out = String::new();
    out.try_reserve(100)
        .map_err(|_| leanhttp::Error::Alloc { bytes: 100 })?;
    write!(
        &mut out,
        "HTTP/1.1 {} Response\r\nConnection: {}\r\n",
        response.status,
        if response.status == 101 {
            "Upgrade"
        } else {
            "close"
        }
    )
    .map_err(|_| leanhttp::Error::Alloc { bytes: 100 })?;
    if response.status != 204
        && response.status != 101
        && !response.headers.iter().any(|(name, _)| {
            name.eq_ignore_ascii_case("Content-Length")
                || name.eq_ignore_ascii_case("Transfer-Encoding")
        })
    {
        write!(&mut out, "Content-Length: {}\r\n", response.body.len())
            .map_err(|_| leanhttp::Error::Alloc { bytes: 100 })?;
    }
    for (name, value) in response.headers.iter() {
        for part in [name.as_str(), ": ", value.as_str(), "\r\n"] {
            spin_domain::try_push_str(&mut out, part).map_err(http_alloc)?;
        }
    }
    spin_domain::try_push_str(&mut out, "\r\n").map_err(http_alloc)?;
    Ok(out)
}

// Een trage browser houdt één laatste snapshot vast, nooit een groeiende rij.
const MAX_STATE_FRAME: usize = 16 << 20;
struct Watch {
    watch: StateWatch,
    version: u64,
    sent: u64,
    checked: u64,
}

#[cfg(test)]
mod order_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn request(path: &str, cookie: bool) -> Option<Input> {
        let mut headers = Vec::new();
        if cookie {
            headers.push((String::from("Cookie"), String::from("spin_session=x")));
        }
        Some(Input {
            method: String::from("GET"),
            path: String::from(path),
            raw_query: String::new(),
            headers,
            body: Vec::new(),
            peer: String::new(),
            secure: false,
        })
    }
    #[test]
    fn the_interface_goes_first_and_bulk_one_per_round() {
        let mut slots: [Slot; CONNECTIONS] = core::array::from_fn(|_| Slot::default());
        slots[1].request = request("/api/uploads/u/chunk", false);
        slots[2].request = request("/api/state", true);
        slots[3].request = request("/api/uploads/v/chunk", false);
        slots[4].request = request("/api/workflow/mcp/s", false);
        let mut cursor = 0;
        let (order, browser, left) = request_order(&slots, &mut cursor);
        let order: Vec<usize> = order.into_iter().flatten().collect();
        assert_eq!(order, [2, 4, 1]);
        assert!(browser && left);
        // De volgende ronde is de andere bulk aan de beurt.
        slots[1].request = None;
        slots[2].request = None;
        slots[4].request = None;
        let (order, browser, left) = request_order(&slots, &mut cursor);
        assert_eq!(order.into_iter().flatten().collect::<Vec<_>>(), [3]);
        assert!(!browser, "browser");
        assert!(!left, "left");
    }
}
