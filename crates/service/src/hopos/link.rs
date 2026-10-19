//! De verbinding met Spin: dezelfde socket, hello en replay als een runner.
//!
//! Eén taak bezit de socket. Een aanvraag van Spin wordt een eigen taak; zijn
//! antwoord gaat via [`OWNER`] en de [`BELL`] terug, zodat een trage Hop de
//! pings nooit ophoudt. Een herhaald verzoek na een reconnect krijgt zijn
//! bewaarde antwoord en wordt nooit een tweede herstart.
use super::{BELL, Config, DIRTY, FORCE, OWNER, hop::Hop};
use alloc::{string::String, vec::Vec};
use applib::{App, EXEC, log, rand::Rng, tcp::Dialer};
use core::time::Duration;
use hoplib::Backoff;
use leanhttp::{Dial, Target};
use spin_core::{
    validation::text,
    websocket::{self as ws, Decoder, Event, Role, Upgrade},
    worker::Admission,
};
use spin_domain::{
    self as d, Wire,
    json::Value,
    protocol::{self as p, WireMessage},
    try_string,
};
use sync::{Either, select};

/// Hoeveel aanvragen tegelijk lopen; de rest krijgt meteen "bezig".
const HANDLERS: usize = 4;
/// Zo vaak pingt de service; de server sluit na 90 s zonder pong.
const PING: Duration = Duration::from_secs(30);
/// Zo lang mag de server zwijgen voordat de service opnieuw verbindt.
const SILENCE: Duration = Duration::from_secs(90);
/// Zo lang mogen verbinden, TLS en de upgrade samen duren.
const HANDSHAKE: Duration = Duration::from_secs(20);

/// Waarom een sessie eindigde.
enum End {
    /// Na een welcome: de volgende poging begint weer snel.
    Lost(String),
    /// Vóór een welcome: de wachttijd loopt op.
    Failed(String),
    /// Het token is geweigerd; alleen een nieuwe config helpt.
    Denied,
}

fn unix_seconds() -> Option<u64> {
    Some(applib::app()?.wall_ns()? / 1_000_000_000)
}
fn entropy() -> Option<leantls::Entropy> {
    Some(leantls::Entropy::new(
        Rng::open(applib::app()?).array::<{ leantls::Entropy::LEN }>(),
    ))
}

/// Verbindt, en na elke breuk opnieuw; eindigt nooit.
pub(crate) async fn run(app: &'static App, config: Config, hop: &'static Hop) -> ! {
    let endpoint = match ws::endpoint(&config.server) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            log!("SPIN_SERVICE_CONFIG error=SPIN_SERVER: {error}");
            app.exit(2);
        }
    };
    let mut rng = Rng::open(app);
    let process = hex(&rng.array::<12>());
    let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
    loop {
        let wait = match session(app, &config, &endpoint, &process, &mut rng, hop).await {
            End::Lost(why) => {
                log!("SPIN_SERVICE_DISCONNECTED reason={why}");
                backoff.reset();
                backoff.next_delay()
            }
            End::Failed(why) => {
                let wait = backoff.next_delay();
                log!("SPIN_SERVICE_CONNECT_FAILED reason={why} retry_in={wait:?}");
                wait
            }
            End::Denied => {
                log!("SPIN_SERVICE_DENIED reason=SPIN_WORKER_TOKEN rejected by the server");
                Duration::from_secs(300)
            }
        };
        EXEC.get().after(wait).await;
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    for b in bytes {
        out.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
        out.push(char::from(b"0123456789abcdef"[usize::from(b & 15)]));
    }
    out
}

async fn session(
    app: &'static App,
    config: &Config,
    endpoint: &ws::Endpoint,
    process: &str,
    rng: &mut Rng,
    hop: &'static Hop,
) -> End {
    let exec = EXEC.get();
    if endpoint.encrypted && app.wall_ns().is_none() {
        return End::Failed(String::from("UTC clock not synchronized yet"));
    }
    let deadline = applib::clock::now_ns().saturating_add(nanos(HANDSHAKE));
    let mut dialer = Dialer::new(exec, Duration::from_secs(10));
    let mut web =
        leanhttps::WebDial::new(&mut dialer, leantls::MOZILLA_ROOTS, unix_seconds, entropy);
    let target = Target {
        https: endpoint.encrypted,
        host: &endpoint.host,
        port: endpoint.port,
    };
    let mut conn = match select(web.dial(target), exec.until(deadline)).await {
        Either::Left(Ok(conn)) => conn,
        Either::Left(Err(error)) => return End::Failed(alloc::format!("dial: {error:?}")),
        Either::Right(()) => return End::Failed(String::from("dial timed out")),
    };
    let key = match spin_security_key(rng) {
        Ok(key) => key,
        Err(error) => return End::Failed(error),
    };
    let (Ok(expected), Ok(request)) = (
        ws::accept(&key),
        ws::upgrade_request(endpoint, &key, &config.token),
    ) else {
        return End::Failed(String::from("invalid endpoint or token"));
    };
    if let Err(error) = write(&mut conn, request.as_bytes()).await {
        return End::Failed(error);
    }
    // De kop tot en met de lege regel; wat erna komt zijn al frames.
    let mut pending = Vec::new();
    let mut buf = [0; 8192];
    let head_end = loop {
        if let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        if pending.len() > 32 << 10 {
            return End::Failed(String::from("upgrade headers too large"));
        }
        match select(leanhttp::read(&mut conn, &mut buf), exec.until(deadline)).await {
            Either::Left(Ok(0)) => return End::Failed(String::from("closed during upgrade")),
            Either::Left(Ok(n)) => pending.extend_from_slice(&buf[..n]),
            Either::Left(Err(error)) => return End::Failed(alloc::format!("read: {error:?}")),
            Either::Right(()) => return End::Failed(String::from("upgrade timed out")),
        }
    };
    match ws::check_upgrade(&pending[..head_end], &expected) {
        Ok(Upgrade::Accepted) => {}
        Ok(Upgrade::Denied) => return End::Denied,
        Ok(Upgrade::Busy) => return End::Failed(String::from("identity still connected")),
        Err(_) => return End::Failed(String::from("WebSocket upgrade rejected")),
    }
    pending.drain(..head_end);
    let hello = match hello(config, process) {
        Ok(hello) => hello,
        Err(error) => return End::Failed(alloc::format!("{error}")),
    };
    if let Err(error) = send(&mut conn, rng, 1, hello.as_bytes()).await {
        return End::Failed(error);
    }
    let mut socket = Session {
        decoder: Decoder::new(Role::Client, p::MAX_MESSAGE_BYTES),
        pending,
        welcomed: false,
        pong: None,
        last_read: applib::clock::now_ns(),
        next_ping: 0,
    };
    let why = socket.serve(&mut conn, &mut buf, rng, hop).await;
    if socket.welcomed {
        End::Lost(why)
    } else {
        End::Failed(why)
    }
}

fn spin_security_key(rng: &mut Rng) -> Result<String, String> {
    spin_security::encode_base64(&rng.array::<16>(), true).map_err(|e| alloc::format!("{e}"))
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn hello(config: &Config, process: &str) -> d::Fallible<String> {
    WireMessage {
        version: p::PROTOCOL_VERSION,
        r#type: try_string(p::MESSAGE_HELLO)?,
        instance_id: text(format_args!("service-{}", config.name))?,
        process: try_string(process)?,
        name: try_string(&config.name)?,
        capabilities: d::ClientCapabilities {
            os: try_string("hopos")?,
            arch: try_string(if cfg!(target_arch = "riscv64") {
                "riscv64"
            } else {
                "arm64"
            })?,
            tools: {
                let mut tools = d::List::new();
                tools.push(try_string(p::TOOL_SERVICE)?)?;
                tools
            },
            ..Default::default()
        },
        streams_reported: true,
        ..Default::default()
    }
    .to_json()
}

async fn write<C: leanhttp::AsyncWrite>(conn: &mut C, bytes: &[u8]) -> Result<(), String> {
    leanhttp::write_all(conn, bytes)
        .await
        .map_err(|e| alloc::format!("write: {e:?}"))?;
    leanhttp::flush(conn)
        .await
        .map_err(|e| alloc::format!("flush: {e:?}"))
}
async fn send<C: leanhttp::AsyncWrite>(
    conn: &mut C,
    rng: &mut Rng,
    opcode: u8,
    bytes: &[u8],
) -> Result<(), String> {
    let frame =
        ws::encode(opcode, bytes, Some(rng.array::<4>())).map_err(|e| alloc::format!("{e}"))?;
    write(conn, &frame).await
}

struct Session {
    decoder: Decoder,
    pending: Vec<u8>,
    welcomed: bool,
    pong: Option<Vec<u8>>,
    last_read: u64,
    next_ping: u64,
}
impl Session {
    /// Leest, beantwoordt en schrijft tot de verbinding breekt; geeft de reden.
    async fn serve<C: leanhttp::Conn + Unpin>(
        &mut self,
        conn: &mut C,
        buf: &mut [u8],
        rng: &mut Rng,
        hop: &'static Hop,
    ) -> String {
        let exec = EXEC.get();
        loop {
            if let Err(why) = self.drain(hop) {
                return why;
            }
            if let Some(data) = self.pong.take()
                && let Err(why) = send(conn, rng, 10, &data).await
            {
                return why;
            }
            let now = applib::clock::now_ns();
            if self.welcomed && now >= self.next_ping {
                if let Err(why) = send(conn, rng, 9, b"").await {
                    return why;
                }
                self.next_ping = now.saturating_add(nanos(PING));
            }
            // Pas na de welcome: de server kent de service dan, en een replay volgt de welcome.
            if self.welcomed {
                loop {
                    let frame = {
                        let owner = OWNER.borrow();
                        let Some((ticket, bytes)) = owner.outbound() else {
                            break;
                        };
                        match ws::encode(1, bytes, Some(rng.array::<4>())) {
                            Ok(frame) => (ticket, frame),
                            Err(error) => return alloc::format!("{error}"),
                        }
                    };
                    if let Err(why) = write(conn, &frame.1).await {
                        return why;
                    }
                    OWNER.borrow_mut().acknowledge(frame.0);
                }
            }
            let silent_until = self.last_read.saturating_add(nanos(SILENCE));
            if applib::clock::now_ns() >= silent_until {
                return String::from("server silent for 90 s");
            }
            let wake = if self.welcomed {
                silent_until.min(self.next_ping)
            } else {
                silent_until
            };
            match select(
                leanhttp::read(conn, buf),
                select(BELL.wait(), exec.until(wake)),
            )
            .await
            {
                Either::Left(Ok(0)) => return String::from("closed by the server"),
                Either::Left(Ok(n)) => {
                    self.last_read = applib::clock::now_ns();
                    if self.pending.try_reserve(n).is_err() {
                        return String::from("out of memory");
                    }
                    self.pending.extend_from_slice(&buf[..n]);
                }
                Either::Left(Err(error)) => return alloc::format!("read: {error:?}"),
                Either::Right(_) => {}
            }
        }
    }
    /// Alle hele frames uit wat er gelezen is.
    fn drain(&mut self, hop: &'static Hop) -> Result<(), String> {
        loop {
            if !self.pending.is_empty() {
                let used = self
                    .decoder
                    .push(&self.pending)
                    .map_err(|e| alloc::format!("{e}"))?;
                self.pending.drain(..used);
            }
            let Some(event) = self
                .decoder
                .next_event()
                .map_err(|e| alloc::format!("{e}"))?
            else {
                return Ok(());
            };
            match event {
                Event::Text(text) => self.message(text.as_bytes(), hop)?,
                Event::Binary(bytes) => self.message(&bytes, hop)?,
                Event::Ping(data) => self.pong = Some(data),
                Event::Pong(_) => {}
                Event::Close(_) => return Err(String::from("closed by the server")),
            }
        }
    }
    fn message(&mut self, bytes: &[u8], hop: &'static Hop) -> Result<(), String> {
        let message = WireMessage::decode(bytes).map_err(|e| alloc::format!("{e}"))?;
        if !self.welcomed {
            if message.r#type != p::MESSAGE_WELCOME || message.client.is_none() {
                return Err(alloc::format!("welcome rejected: {}", message.error));
            }
            self.welcomed = true;
            self.next_ping = applib::clock::now_ns().saturating_add(nanos(PING));
            log!(
                "SPIN_SERVICE_CONNECTED client={}",
                message.client.as_ref().map_or("", |c| c.id.as_str())
            );
            // Spin houdt de status alleen in het geheugen: na elke welcome opnieuw.
            FORCE.set();
            DIRTY.set();
            return Ok(());
        }
        if message.r#type != p::MESSAGE_REQUEST {
            return Ok(());
        }
        let busy = OWNER.borrow().active() >= HANDLERS;
        let admission = OWNER
            .borrow_mut()
            .begin(&message)
            .map_err(|e| alloc::format!("{e}"))?;
        match admission {
            Admission::Start if busy => {
                OWNER.borrow_mut().release_unstarted(&message.id);
                refuse(&message.id, "service is busy; try again")?;
            }
            Admission::Start => {
                let id = message.id.clone();
                if EXEC.get().spawn(handle(message, hop)).is_err() {
                    OWNER.borrow_mut().release_unstarted(&id);
                    refuse(&id, "service is busy; try again")?;
                }
            }
            Admission::Busy => refuse(&message.id, "service operation queue is full")?,
            Admission::InFlight | Admission::Replayed => {}
        }
        Ok(())
    }
}

fn response(id: &str, result: Result<Value, String>) -> d::Fallible<WireMessage> {
    let mut message = WireMessage {
        version: p::PROTOCOL_VERSION,
        r#type: try_string(p::MESSAGE_RESPONSE)?,
        id: try_string(id)?,
        ..Default::default()
    };
    match result {
        Ok(value) => message.payload = d::RawJson(Some(value)),
        Err(error) => message.error = error,
    }
    Ok(message)
}
/// Een weigering zonder operatie: niet bewaard, want er liep niets.
fn refuse(id: &str, why: &str) -> Result<(), String> {
    let reply = response(id, Err(String::from(why))).map_err(|e| alloc::format!("{e}"))?;
    OWNER
        .borrow_mut()
        .enqueue(&reply)
        .map_err(|e| alloc::format!("{e}"))?;
    BELL.set();
    Ok(())
}

/// Eén aanvraag: Hop vragen en het antwoord in de outbox leggen.
async fn handle(request: WireMessage, hop: &'static Hop) {
    let payload = request.payload.0.as_ref().unwrap_or(&Value::Null);
    let result = hop.answer(&request.method, payload).await;
    let reply = match response(&request.id, result) {
        Ok(reply) => reply,
        Err(error) => {
            log!("SPIN_SERVICE_REPLY_FAILED id={} error={error}", request.id);
            OWNER.borrow_mut().release_unstarted(&request.id);
            return;
        }
    };
    // Een volle outbox loopt leeg zodra de socket schrijft; tot dan wacht het antwoord.
    loop {
        let finished = OWNER.borrow_mut().finish(&reply);
        match finished {
            Ok(true) => break,
            Ok(false) => EXEC.get().after(Duration::from_secs(1)).await,
            Err(error) => {
                log!("SPIN_SERVICE_REPLY_FAILED id={} error={error}", request.id);
                OWNER.borrow_mut().release_unstarted(&request.id);
                return;
            }
        }
    }
    BELL.set();
}
