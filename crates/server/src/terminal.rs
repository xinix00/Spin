//! Een browserterminal bezit één runnerstream; verbreken stopt alleen die stream.
use super::*;
use alloc::{collections::VecDeque, vec::Vec};
use d::{
    protocol::{self as p, WireMessage},
    try_string,
};
/// Een intrekbaar browserhandvat zonder directe toegang tot de capsule of runner.
pub struct TerminalLink {
    id: String,
    token_hash: String,
}
pub(crate) struct Terminal {
    id: String,
    target: d::Recording,
    record: bool,
    client: String,
    request: String,
    started: bool,
    done: bool,
    ended: bool,
    events: VecDeque<String>,
    bytes: usize,
}
impl Terminal {
    fn event(&mut self, value: Value) -> Result {
        let json = value.to_json()?;
        if self.events.len() >= 256 || json.len() > (1_usize << 20).saturating_sub(self.bytes) {
            return Err(Error::Http(503, "terminal browser is too slow"));
        }
        self.events
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        self.bytes += json.len();
        self.events.push_back(json);
        Ok(())
    }
}
fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(bytes.len())
        .map_err(|_| d::Error::OutOfMemory)?;
    output.extend_from_slice(bytes);
    Ok(output)
}
fn value_text<'a>(value: &'a Value, key: &str) -> &'a str {
    value
        .as_object()
        .and_then(|v| v.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
}
fn dimension(value: &Value, key: &str, default: u16) -> u16 {
    let number = value
        .as_object()
        .and_then(|v| v.get(key))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if number <= 0 {
        default
    } else {
        u16::try_from(number.min(500)).unwrap_or(default)
    }
}
impl<P: Persistence> Server<P> {
    pub(crate) fn recording_terminal_busy(&self, id: &str) -> bool {
        self.terminals.iter().any(|t| t.target.id == id && !t.done)
    }

    pub(crate) fn terminal_route(
        &mut self,
        request: &Request<'_>,
        actor: &str,
        token_hash: &str,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        if request.method != "GET" || !request.path.ends_with("/terminal") {
            return Ok(None);
        }
        let mut parts = request.path.trim_start_matches('/').split('/');
        let (Some("api"), Some(kind), Some(id), Some("terminal"), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Ok(None);
        };
        if !matches!(kind, "recordings" | "compositions") {
            return Ok(None);
        }
        if !auth::origin(request) {
            return Err(Error::Http(403, "invalid request origin"));
        }
        let record = kind == "recordings";
        let target = if record {
            let recording = self.store.recording(id)?;
            if recording.actor != actor || self.store.open_recording(actor)?.id != recording.id {
                return Err(Error::Http(409, "recording is not open for this operator"));
            }
            d::Recording {
                id: recording.id.try_clone()?,
                actor: recording.actor.try_clone()?,
                runtime: recording.runtime.try_clone()?,
                ..Default::default()
            }
        } else {
            let composition = self.store.composition(id)?;
            if composition.operator != actor
                || composition
                    .runtime
                    .as_ref()
                    .is_none_or(|r| r.status != "ready" || r.stop_pending)
            {
                return Err(Error::Http(
                    409,
                    "composition is not running for this operator",
                ));
            }
            d::Recording {
                id: composition.id.try_clone()?,
                actor: composition.operator.try_clone()?,
                runtime: composition.runtime.try_clone()?,
                ..Default::default()
            }
        };
        let client = target
            .runtime
            .as_ref()
            .ok_or(Error::Http(409, "capsule has no runtime"))?
            .client_id
            .try_clone()?;
        if self.terminals.len() >= 64
            || self
                .terminals
                .iter()
                .filter(|t| !t.done && t.target.id == target.id)
                .count()
                >= 8
        {
            return Err(Error::Http(503, "terminal capacity reached"));
        }
        self.terminals
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        let id = runtime.next("term")?;
        let link = TerminalLink {
            id: id.try_clone()?,
            token_hash: try_string(token_hash)?,
        };
        self.terminals.push(Terminal {
            id,
            target,
            record,
            client,
            request: String::new(),
            started: false,
            done: false,
            ended: false,
            events: VecDeque::new(),
            bytes: 0,
        });
        Ok(Some(Outcome::Terminal(link)))
    }
    /// De browsercookie wordt ook na de upgrade opnieuw gecontroleerd.
    pub fn validate_terminal(&mut self, link: &TerminalLink, now: &Timestamp) -> Result {
        self.store.authenticate_session(&link.token_hash, now)?;
        Ok(())
    }
    /// Start, invoer, interrupt en resize lopen door de runner die deze capsule bezit.
    pub fn terminal_message(
        &mut self,
        link: &TerminalLink,
        message: &Value,
        runtime: &mut impl Runtime,
    ) -> Result {
        let terminal = self
            .terminals
            .iter_mut()
            .find(|t| t.id == link.id)
            .ok_or(Error::Http(404, "terminal closed"))?;
        if terminal.done {
            return Ok(());
        }
        let peer = self
            .runners
            .iter_mut()
            .find(|p| p.client().id == terminal.client)
            .ok_or(Error::Http(409, "capsule runner is offline"))?;
        if terminal.request.is_empty() {
            if value_text(message, "type") != "start"
                || value_text(message, "command").trim().is_empty()
            {
                return Err(Error::Http(
                    400,
                    "first terminal message must start a command",
                ));
            }
            // Een losse verbinding is geen fout: de peer bewaart de start in zijn
            // outbox en speelt die af zodra de runner terug is.
            let id = runtime.next("req")?;
            let request = WireMessage {
                version: p::PROTOCOL_VERSION,
                r#type: try_string(p::MESSAGE_REQUEST)?,
                id: id.try_clone()?,
                method: try_string(p::METHOD_START_INTERACTIVE)?,
                payload: d::RawJson(Some(
                    p::InteractivePayload {
                        recording: terminal.target.try_clone()?,
                        input: try_string(value_text(message, "command"))?,
                        rows: dimension(message, "rows", 30),
                        cols: dimension(message, "cols", 120),
                    }
                    .to_value()?,
                )),
                ..Default::default()
            };
            peer.adopt_stream(&id, false)?;
            if let Err(error) = peer.request(request) {
                peer.finish_stream(&id);
                return Err(error.into());
            }
            terminal.request = id;
            return Ok(());
        }
        let kind = value_text(message, "type");
        if !matches!(kind, "input" | "interrupt" | "resize") {
            return Ok(());
        }
        let mut request = WireMessage {
            version: p::PROTOCOL_VERSION,
            r#type: try_string(if kind == "resize" {
                p::MESSAGE_STREAM_RESIZE
            } else {
                p::MESSAGE_STREAM_INPUT
            })?,
            id: terminal.request.try_clone()?,
            rows: dimension(message, "rows", 30),
            cols: dimension(message, "cols", 120),
            ..Default::default()
        };
        if kind != "resize" {
            request.data = d::Bytes(Some(if kind == "interrupt" {
                copy_bytes(&[3])?
            } else {
                copy_bytes(value_text(message, "data").as_bytes())?
            }));
        }
        peer.enqueue(request)?;
        Ok(())
    }
    pub(crate) fn terminal_runner(
        &mut self,
        client: &str,
        message: &WireMessage,
        now: &Timestamp,
    ) -> Result<bool> {
        let Some(terminal) = self
            .terminals
            .iter_mut()
            .find(|t| t.client == client && t.request == message.id && !t.done)
        else {
            return Ok(false);
        };
        let event = match message.r#type.as_str() {
            p::MESSAGE_RESPONSE => {
                if !message.error.is_empty() {
                    terminal.done = true;
                    terminal.ended = true;
                    http::object(&[
                        ("type", Value::string("error")?),
                        ("error", Value::string(&message.error)?),
                    ])?
                } else {
                    terminal.started = true;
                    http::object(&[("type", Value::string("ready")?)])?
                }
            }
            p::MESSAGE_STREAM_DATA => {
                let text =
                    spin_core::docker::utf8(message.data.0.as_deref().unwrap_or_default(), false)?;
                http::object(&[
                    ("type", Value::string("output")?),
                    ("data", Value::String(text)),
                ])?
            }
            p::MESSAGE_STREAM_EXIT => {
                terminal.done = true;
                terminal.ended = true;
                let code = message.execution.as_ref().map_or(-1, |e| e.exit_code);
                let recorded = if terminal.record {
                    self.store
                        .record_execution(
                            &terminal.target.id,
                            &terminal.target.actor,
                            Some(code),
                            now,
                        )
                        .map(|_| ())
                } else {
                    Ok(())
                };
                if let Err(error) = recorded {
                    http::object(&[
                        ("type", Value::string("error")?),
                        (
                            "error",
                            Value::String(spin_core::validation::text(format_args!("{error}"))?),
                        ),
                    ])?
                } else if !message.error.is_empty() {
                    http::object(&[
                        ("type", Value::string("error")?),
                        ("error", Value::string(&message.error)?),
                    ])?
                } else {
                    http::object(&[
                        ("type", Value::string("exit")?),
                        ("exit_code", code.to_value()?),
                    ])?
                }
            }
            _ => return Ok(false),
        };
        if terminal.event(event).is_err() {
            terminal.done = true;
            terminal.events.clear();
            terminal.bytes = 0;
            terminal.event(http::object(&[
                ("type", Value::string("error")?),
                ("error", Value::string("terminal browser is too slow")?),
            ])?)?;
            if let Some(peer) = self.runners.iter_mut().find(|p| p.client().id == client) {
                peer.enqueue(WireMessage {
                    r#type: try_string(p::MESSAGE_STREAM_CLOSE)?,
                    id: message.id.try_clone()?,
                    ..Default::default()
                })?;
                peer.finish_stream(&message.id);
            }
        }
        Ok(true)
    }
    /// Een transportfout wordt als terminalbericht getoond en sluit daarna deze socket.
    pub fn terminal_error(&mut self, link: &TerminalLink, error: &Error) -> Result {
        if let Some(terminal) = self.terminals.iter_mut().find(|t| t.id == link.id) {
            if terminal.done {
                return Ok(());
            }
            terminal.done = true;
            terminal.events.clear();
            terminal.bytes = 0;
            terminal.event(http::object(&[
                ("type", Value::string("error")?),
                (
                    "error",
                    Value::String(spin_core::validation::text(format_args!("{error}"))?),
                ),
            ])?)?;
        }
        Ok(())
    }
    /// De socket vraagt één browserbericht; verwijderen gebeurt pas na overname door zijn schrijver.
    pub fn terminal_next(&self, link: &TerminalLink) -> Option<&str> {
        self.terminals
            .iter()
            .find(|t| t.id == link.id)?
            .events
            .front()
            .map(String::as_str)
    }
    /// Bevestigt dat de begrensde socketbuffer het bericht bezit.
    pub fn terminal_acknowledge(&mut self, link: &TerminalLink) {
        if let Some(terminal) = self.terminals.iter_mut().find(|t| t.id == link.id)
            && let Some(message) = terminal.events.pop_front()
        {
            terminal.bytes -= message.len();
        }
    }
    /// Na het laatste exit/error-bericht mag de browserverbinding sluiten.
    pub fn terminal_done(&self, link: &TerminalLink) -> bool {
        self.terminals
            .iter()
            .find(|t| t.id == link.id)
            .is_none_or(|t| t.done && t.events.is_empty())
    }
    /// Een verbroken browser maakt geen runner of andere terminal onbruikbaar.
    pub fn terminal_disconnect(&mut self, link: TerminalLink) -> Result {
        if let Some(index) = self.terminals.iter().position(|t| t.id == link.id) {
            let terminal = self.terminals.remove(index);
            if !terminal.request.is_empty()
                && let Some(peer) = self
                    .runners
                    .iter_mut()
                    .find(|p| p.client().id == terminal.client)
            {
                if !terminal.ended && !peer.cancel(&terminal.request)? {
                    peer.enqueue(WireMessage {
                        r#type: try_string(p::MESSAGE_STREAM_CLOSE)?,
                        id: terminal.request.try_clone()?,
                        ..Default::default()
                    })?;
                }
                peer.finish_stream(&terminal.request);
            }
        }
        Ok(())
    }
}
