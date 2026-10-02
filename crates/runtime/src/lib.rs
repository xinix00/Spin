//! Vaste verbindingspool en berichtentabel: alleen de app-taak raakt de Store aan.
#![no_std]
#![deny(unsafe_code)]
extern crate alloc;
use alloc::{string::String, vec::Vec};
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
    /// rusten tot [`Mailbox::nudge`] of een vloertimer; `busy` zegt dat er werk
    /// ligt dat alleen door pollen vordert (wachtwoorden, uitgaande HTTP, een
    /// staat-push), zodat het platform dan kort slaapt.
    fn idle(&mut self, mail: &Mailbox, busy: bool) -> Result;
    /// Schrijft één diagnostische regel zonder verzoekinhoud.
    fn log(message: core::fmt::Arguments<'_>);
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
    /// De deurbel van de eigenaar: level-triggered en samengevoegd, tien
    /// bellen in één idle zijn er één.
    nudged: Cell<bool>,
    bell: Local<RefCell<Option<core::task::Waker>>>,
}
impl Default for Mailbox {
    fn default() -> Self {
        Self {
            slots: Local(RefCell::new(core::array::from_fn(|_| Slot::default()))),
            health: Local(RefCell::new(None)),
            active: Cell::new(false),
            nudged: Cell::new(false),
            bell: Local(RefCell::new(None)),
        }
    }
}
impl Mailbox {
    /// Belt de eigenaar: een sockettaak legde iets in een slot, of een
    /// platformtaak rondde werk af. De eigenaar verlaat zijn idle in de
    /// volgende ronde.
    pub fn nudge(&self) {
        self.nudged.set(true);
        if let Some(waker) = self.bell.0.borrow_mut().take() {
            waker.wake();
        }
    }
    /// Wacht tot er gebeld is; een bel die tijdens het werk viel, wordt bij
    /// de volgende idle meteen gezien.
    pub fn nudged(&self) -> impl Future<Output = ()> + '_ {
        core::future::poll_fn(move |cx| {
            if self.nudged.replace(false) {
                Poll::Ready(())
            } else {
                *self.bell.0.borrow_mut() = Some(cx.waker().clone());
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
    let mut capsule_diagnosed = None;
    let mut health_updated = clock.millis();
    let mut context = Context::from_waker(Waker::noop());
    while !platform.stopped() {
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
        if let Err(error) = server.maintain_restores(&now, runtime) {
            H::log(format_args!("SPIN_RESTORE_FAILED error={error}"));
        }
        if !server.backup_active() {
            if let Err(error) = server.maintain_operations(&now, runtime) {
                H::log(format_args!("SPIN_OPERATION_FAILED error={error}"));
            }
            outgoing.poll::<H, P>(&platform, server, &now, runtime)?;
            if let Err(error) = server.maintain_agents(&now, runtime) {
                H::log(format_args!("SPIN_AGENT_MAINTENANCE_FAILED error={error}"));
            }
            if let Err(error) = server.maintain_uploads(&now) {
                H::log(format_args!("SPIN_UPLOAD_MAINTENANCE_FAILED error={error}"));
            }
            if clock.millis().saturating_sub(maintained) >= 1000 {
                if let Err(error) = server.maintain_storage(&now) {
                    H::log(format_args!(
                        "SPIN_STORAGE_MAINTENANCE_FAILED error={error}"
                    ));
                }
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
                // A slow maintenance round must leave an interval for queued requests.
                maintained = clock.millis();
            }
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
        for index in 0..CONNECTIONS {
            let input = { mail.slots.0.borrow_mut()[index].request.take() };
            if let Some(input) = input {
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
                let result = server.begin(request, &now, runtime);
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
                    Ok(Outcome::State(watch)) => {
                        match server.state_for_watch(&watch, &now).and_then(|json| {
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
        }
        for (index, waiting) in uploads.iter_mut().enumerate() {
            let Some(wait) = waiting else {
                continue;
            };
            let response = match server.poll_upload(wait) {
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
            // De export stroomt per chunk op de deurbel van zijn socket; kort slapen.
            platform.idle(mail, true)?;
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
            if let Err(error) = result
                && server.terminal_error(link, &error).is_err()
            {
                mail.slots.0.borrow_mut()[index].close = Some(1008);
                continue;
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
            if server.validate_chat(link, &now).is_err() {
                mail.slots.0.borrow_mut()[index].close = Some(1008);
                continue;
            }
            let message = mail.slots.0.borrow_mut()[index].browser.take();
            if let Some(message) = message
                && let Err(error) = server.chat_message(link, &message, &now)
                && server.chat_error(link, &error).is_err()
            {
                mail.slots.0.borrow_mut()[index].close = Some(1008);
                continue;
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
                    Err(_) => mail.slots.0.borrow_mut()[index].close = Some(1011),
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
                        RunnerEvent::Response { response, .. } if !response.error.is_empty() => {
                            H::log(format_args!("SPIN_RUNNER_REQUEST_FAILED"))
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
                H::log(format_args!("SPIN_RUNNER_CLOSED error={error}"));
                mail.slots.0.borrow_mut()[index].close = Some(1008);
            }
        }
        // Hoogstens 4096 PBKDF2-rondes per actorronde, over alle logins samen.
        let pending = passwords.iter().filter(|p| p.is_some()).count().max(1);
        let rounds = 4096 / u32::try_from(pending).map_err(boundary)?;
        for (index, password) in passwords.iter_mut().enumerate() {
            if password.as_mut().is_some_and(|work| work.step(rounds))
                && let Some(work) = password.take()
            {
                if let Some(response) =
                    make_response::<H>(server.finish_password(work, &now, runtime))
                {
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
                if server.validate_watch(&watch.watch, &now).is_err() {
                    mail.slots.0.borrow_mut()[index].close = Some(1008);
                    continue;
                }
            }
            if watch.version != server.version()
                && clock.millis().saturating_sub(watch.sent) >= 150
                && mail.slots.0.borrow()[index].frame_bytes == 0
                && mail.slots.0.borrow()[index].close.is_none()
            {
                match server.state_for_watch(&watch.watch, &now) {
                    Ok(json) if json.len() <= MAX_STATE_FRAME => {
                        match spin_core::websocket::encode(1, json.as_bytes(), None) {
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
                                }
                            }
                            Err(_) => mail.slots.0.borrow_mut()[index].close = Some(1011),
                        }
                    }
                    _ => {
                        mail.slots.0.borrow_mut()[index].close = Some(1008);
                    }
                }
            }
        }
        // Alleen werk dat door pollen vordert houdt de eigenaar wakker; de rest
        // komt met de deurbel of de vloertimer van het platform.
        let busy = passwords.iter().any(Option::is_some)
            || outgoing.active()
            || watches
                .iter()
                .flatten()
                .any(|watch| watch.version != server.version());
        platform.idle(mail, busy)?;
    }
    Ok(())
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
