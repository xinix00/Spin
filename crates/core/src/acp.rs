//! ACP als begrensde berichteneigenaar. Geen socket, proces, thread of gedeelde lock.
use crate::validation::{invalid, text};
use alloc::{collections::VecDeque, string::String, vec::Vec};
use spin_domain::{
    self as d, Map, TryClone, Wire,
    json::{Object, Value},
    try_string,
};
pub mod session;
pub mod settings;
/// De bestaande ACP-scannergrens is zestien MiB per JSON-regel.
pub const LINE_LIMIT: usize = 16 << 20;
const QUEUE_LIMIT: usize = 128;
const PENDING_LIMIT: usize = 64;
struct Pending {
    method: String,
    deadline: Option<u64>,
}
struct Queued {
    bytes: usize,
    event: Event,
}
/// Een compleet ACP-bericht, of een deadline die de eigenaar zelf heeft laten verlopen.
pub enum Event {
    /// Een antwoord verwijdert de bijbehorende call precies eenmaal.
    Reply {
        /// Het numerieke ID dat deze eigenaar heeft uitgegeven.
        id: u64,
        /// De methode waarvoor het antwoord verwacht werd.
        method: String,
        /// De volledige JSON-envelop, inclusief eventuele foutdata.
        body: Value,
    },
    /// Een agent-notificatie of verzoek, zoals update of request_permission.
    Method(Value),
    /// Een antwoord kwam niet voor de expliciete deadline.
    Timeout {
        /// Het verlopen request-ID.
        id: u64,
        /// De oorspronkelijke methode.
        method: String,
    },
}
/// De verbinding mag worden vervangen; lopende ACP-IDs en gedeeltelijke regels blijven hier.
pub struct Rpc {
    next: u64,
    pending: Map<Pending>,
    line: Vec<u8>,
    outbox: VecDeque<Vec<u8>>,
    out_bytes: usize,
    events: VecDeque<Queued>,
    event_bytes: usize,
}
impl Default for Rpc {
    fn default() -> Self {
        Self::new(0)
    }
}
impl Rpc {
    /// Een geadopteerde agent begint boven alle eerder uitgegeven IDs.
    pub fn new(last_id: u64) -> Self {
        Self {
            next: last_id,
            pending: Map::new(),
            line: Vec::new(),
            outbox: VecDeque::new(),
            out_bytes: 0,
            events: VecDeque::new(),
            event_bytes: 0,
        }
    }
    /// Reserveert een call vóór de bytes zichtbaar worden voor het transport.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        deadline_ms: Option<u64>,
    ) -> d::Fallible<u64> {
        if self.pending.len() >= PENDING_LIMIT || method.is_empty() || method.len() > 256 {
            return Err(invalid("ACP", "pending call budget exceeded"));
        }
        let id = self
            .next
            .checked_add(1)
            .ok_or_else(|| invalid("ACP", "request IDs exhausted"))?;
        let value = object(&[
            ("jsonrpc", Value::string("2.0")?),
            (
                "id",
                i64::try_from(id)
                    .map_err(|_| invalid("ACP", "request IDs exhausted"))?
                    .to_value()?,
            ),
            ("method", Value::string(method)?),
            ("params", params),
        ])?;
        let bytes = self.prepare(&value)?;
        self.pending.insert(
            text(format_args!("{id}"))?,
            Pending {
                method: try_string(method)?,
                deadline: deadline_ms,
            },
        )?;
        self.next = id;
        self.out_bytes += bytes.len();
        self.outbox.push_back(bytes);
        Ok(id)
    }
    /// Een duurzame prompt van vóór de serverherstart krijgt zijn oorspronkelijke ID terug.
    pub fn adopt(&mut self, id: u64, method: &str) -> d::Fallible {
        let key = text(format_args!("{id}"))?;
        if id == 0
            || id > i64::MAX as u64
            || self.pending.contains_key(&key)
            || self.pending.len() >= PENDING_LIMIT
            || method.len() > 256
        {
            return Err(invalid("ACP", "invalid adopted call"));
        }
        self.pending.insert(
            key,
            Pending {
                method: try_string(method)?,
                deadline: None,
            },
        )?;
        self.next = self.next.max(id);
        Ok(())
    }
    /// Een notificatie verwacht geen antwoord en verandert de request-teller niet.
    pub fn notify(&mut self, method: &str, params: Value) -> d::Fallible {
        self.write(object(&[
            ("jsonrpc", Value::string("2.0")?),
            ("method", Value::string(method)?),
            ("params", params),
        ])?)
    }
    /// Het originele agent-ID blijft een JSON-getal of string, ook bij permissions.
    pub fn respond(&mut self, id: Value, result: Value) -> d::Fallible {
        if !matches!(id, Value::Number(_) | Value::String(_)) {
            return Err(invalid("ACP", "invalid response ID"));
        }
        self.write(object(&[
            ("jsonrpc", Value::string("2.0")?),
            ("id", id),
            ("result", result),
        ])?)
    }
    /// Methoden die Spin niet implementeert krijgen de bestaande JSON-RPC-fout.
    pub fn unsupported(&mut self, id: Value) -> d::Fallible {
        self.write(object(&[
            ("jsonrpc", Value::string("2.0")?),
            ("id", id),
            (
                "error",
                object(&[
                    ("code", (-32601_i64).to_value()?),
                    ("message", Value::string("client method not supported")?),
                ])?,
            ),
        ])?)
    }
    fn prepare(&mut self, value: &Value) -> d::Fallible<Vec<u8>> {
        let mut bytes = value.to_json()?.into_bytes();
        if bytes.len() >= LINE_LIMIT
            || self.outbox.len() >= QUEUE_LIMIT
            || bytes.len() + 1 > LINE_LIMIT.saturating_sub(self.out_bytes)
        {
            return Err(invalid("ACP", "outbox budget exceeded"));
        }
        self.outbox
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        d::try_push(&mut bytes, b'\n')?;
        Ok(bytes)
    }
    fn write(&mut self, value: Value) -> d::Fallible {
        let bytes = self.prepare(&value)?;
        self.out_bytes += bytes.len();
        self.outbox.push_back(bytes);
        Ok(())
    }
    /// Bytes worden pas verwijderd nadat de runner-outbox het hele bericht heeft overgenomen.
    pub fn outbound(&self) -> Option<&[u8]> {
        self.outbox.front().map(Vec::as_slice)
    }
    /// Draagt het oudste complete JSON-regelbericht over.
    pub fn acknowledge(&mut self) {
        if let Some(bytes) = self.outbox.pop_front() {
            self.out_bytes -= bytes.len();
        }
    }
    /// Fragmenten van de runner mogen op iedere bytegrens aankomen, ook midden in UTF-8.
    pub fn receive(&mut self, mut bytes: &[u8]) -> d::Fallible {
        while !bytes.is_empty() {
            let newline = bytes.iter().position(|b| *b == b'\n');
            let count = newline.unwrap_or(bytes.len());
            if count > LINE_LIMIT.saturating_sub(self.line.len()) {
                return Err(invalid("ACP", "agent line exceeds 16 MiB"));
            }
            self.line
                .try_reserve_exact(count)
                .map_err(|_| d::Error::OutOfMemory)?;
            self.line.extend_from_slice(&bytes[..count]);
            let Some(_) = newline else {
                break;
            };
            let value = Value::from_json_with_limit(&self.line, LINE_LIMIT)?;
            let object = value
                .as_object()
                .ok_or_else(|| invalid("ACP", "agent envelope must be an object"))?;
            let method = object.get("method").and_then(Value::as_str).unwrap_or("");
            if !method.is_empty() {
                self.push_event(self.line.len(), Event::Method(value))?;
            } else if let Some(id) = object
                .get("id")
                .and_then(Value::as_i64)
                .and_then(|id| u64::try_from(id).ok())
            {
                let key = text(format_args!("{id}"))?;
                if let Some(pending) = self.pending.get(&key) {
                    let method = pending.method.try_clone()?;
                    self.push_event(
                        self.line.len(),
                        Event::Reply {
                            id,
                            method,
                            body: value,
                        },
                    )?;
                    self.pending.remove(&key);
                }
            }
            self.line = Vec::new();
            bytes = &bytes[count + 1..];
        }
        Ok(())
    }
    fn push_event(&mut self, bytes: usize, event: Event) -> d::Fallible {
        if self.events.len() >= QUEUE_LIMIT || bytes > LINE_LIMIT.saturating_sub(self.event_bytes) {
            return Err(invalid("ACP", "event budget exceeded"));
        }
        self.events
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        self.event_bytes += bytes;
        self.events.push_back(Queued { bytes, event });
        Ok(())
    }
    /// Deadlines komen van de appklok; de codec leest zelf geen klok of timer.
    pub fn expire(&mut self, now_ms: u64) -> d::Fallible {
        let mut expired = Vec::new();
        for (id, pending) in self.pending.iter() {
            if pending.deadline.is_some_and(|deadline| now_ms >= deadline) {
                d::try_push(&mut expired, try_string(id)?)?;
            }
        }
        for key in expired {
            if let Some(pending) = self.pending.get(&key) {
                let id = key
                    .parse()
                    .map_err(|_| invalid("ACP", "invalid pending ID"))?;
                let method = pending.method.try_clone()?;
                self.push_event(method.len(), Event::Timeout { id, method })?;
                self.pending.remove(&key);
            }
        }
        Ok(())
    }
    /// Eén bericht gaat naar de sessie-eigenaar; verouderde antwoorden komen hier nooit terecht.
    pub fn event(&mut self) -> Option<Event> {
        let queued = self.events.pop_front()?;
        self.event_bytes -= queued.bytes;
        Some(queued.event)
    }
    /// Het hoogste uitgegeven ID hoort bij de duurzame agentverwijzing.
    pub fn last_id(&self) -> u64 {
        self.next
    }
    /// Een afgehandelde geadopteerde prompt mag nooit nog een tweede eindbericht geven.
    pub fn forget(&mut self, id: u64) -> d::Fallible<bool> {
        Ok(self.pending.remove(&text(format_args!("{id}"))?).is_some())
    }
}
/// Klein object zonder infallibele map- of stringallocaties.
pub fn object(fields: &[(&str, Value)]) -> d::Fallible<Value> {
    let mut object = Object::new();
    for (key, value) in fields {
        object.push(key, value.try_clone()?)?;
    }
    Ok(Value::Object(object))
}
/// Dezelfde initialize-capabilities als de Go-client: bestanden en terminals blijven in de capsule.
pub fn initialize(protocol: i64) -> d::Fallible<Value> {
    object(&[("protocolVersion", (if protocol == 0 {1} else {protocol}).to_value()?),
        ("clientCapabilities", Value::from_json(br#"{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false,"auth":{"terminal":false}}"#)?),
        ("clientInfo", Value::from_json(br#"{"name":"easyacp","title":"EasyACP Capsule Client","version":"0.2.0"}"#)?)])
}
/// HTTP-MCP wordt alleen aangeboden wanneer de concrete agent dat ondersteunt.
pub fn new_session(servers: &[d::MCPServer], http: bool) -> d::Fallible<Value> {
    let mut mcp = Vec::new();
    for server in servers {
        let value = match server.transport.as_str() {
            "stdio" => object(&[
                ("name", server.name.to_value()?),
                ("command", server.command.to_value()?),
                ("args", server.args.to_value()?),
                ("env", server.env.to_value()?),
            ])?,
            "http" if http => object(&[
                ("type", Value::string("http")?),
                ("name", server.name.to_value()?),
                ("url", server.url.to_value()?),
                ("headers", server.headers.to_value()?),
            ])?,
            _ => {
                return Err(invalid(
                    "MCP",
                    "agent does not support the requested MCP transport",
                ));
            }
        };
        d::try_push(&mut mcp, value)?;
    }
    object(&[
        ("cwd", Value::string("/workspace")?),
        ("additionalDirectories", Value::from_json(br#"["/root"]"#)?),
        ("mcpServers", Value::Array(mcp)),
    ])
}
/// Foutdata bevat vaak de echte oorzaak, terwijl het bericht alleen 'Internal error' zegt.
pub fn rpc_error(value: &Value) -> d::Fallible<String> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("ACP", "invalid RPC error"))?;
    let message = object.get("message").and_then(Value::as_str).unwrap_or("");
    let code = object.get("code").and_then(Value::as_i64).unwrap_or(0);
    let data = object.get("data").unwrap_or(&Value::Null);
    let detail = match data {
        Value::Null => String::new(),
        Value::String(value) => try_string(value.trim())?,
        value => value.to_json()?,
    };
    if detail.is_empty() {
        return text(format_args!("{message} ({code})"));
    }
    let mut end = detail.len().min(400);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    text(format_args!(
        "{message} ({code}): {}{}",
        &detail[..end],
        if end < detail.len() { "…" } else { "" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn json(text: &str) -> Value {
        Value::from_json(text.as_bytes()).unwrap()
    }
    #[test]
    fn fragmented_utf8_notifications_duplicate_responses_deadlines_and_adoption() {
        let mut rpc = Rpc::default();
        assert_eq!(
            rpc.call("initialize", initialize(1).unwrap(), Some(100))
                .unwrap(),
            1
        );
        let request = Value::from_json(rpc.outbound().unwrap()).unwrap();
        assert_eq!(
            request.as_object().unwrap().get("id").unwrap().as_i64(),
            Some(1)
        );
        rpc.acknowledge();
        let incoming = "{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"text\":\"hé🙂\"}}\n{\"id\":1,\"result\":{}}\n{\"id\":1,\"result\":{}}\n";
        for byte in incoming.as_bytes() {
            rpc.receive(core::slice::from_ref(byte)).unwrap();
        }
        assert!(matches!(rpc.event(), Some(Event::Method(_))));
        assert!(matches!(rpc.event(), Some(Event::Reply { id: 1, .. })));
        assert!(rpc.event().is_none());
        rpc.expire(100).unwrap();
        assert!(rpc.event().is_none());
        rpc.call("session/new", Value::Null, Some(200)).unwrap();
        rpc.expire(199).unwrap();
        assert!(rpc.event().is_none());
        rpc.expire(200).unwrap();
        assert!(matches!(rpc.event(), Some(Event::Timeout { id: 2, .. })));
        rpc.receive(b"{\"id\":2,\"result\":{}}\n").unwrap();
        assert!(rpc.event().is_none());
        rpc.adopt(400, "session/prompt").unwrap();
        assert_eq!(
            rpc.call("session/set_mode", Value::Null, None).unwrap(),
            401
        );
        assert!(rpc.forget(400).unwrap());
        rpc.receive(b"{\"id\":400,\"result\":{}}\n").unwrap();
        assert!(rpc.event().is_none());
        assert!(rpc.receive(b"not json\n").is_err());
    }
    #[test]
    fn three_settings_protocols_precedence_permission_choice_and_mcp_negotiation() {
        let created = json(
            r#"{"configOptions":[{"id":"mode","currentValue":"agent","options":[{"value":"agent"},{"value":"agent-full-access"}]},{"id":"reasoning_effort","currentValue":"medium","options":[{"value":"high","name":"High"}]}],"modes":{"availableModes":[{"id":"wrong"}]},"models":{"currentModelId":"model-a","availableModels":[{"value":"model-a","title":"Agent Model"}]}}"#,
        );
        let normalized = settings::normalize(&created).unwrap();
        assert_eq!(normalized.len(), 3);
        assert_eq!(
            normalized[0].full_access().unwrap(),
            Some("agent-full-access")
        );
        assert_eq!(normalized[0].method, "session/set_config_option");
        assert_eq!(normalized[1].category, "thought_level");
        assert_eq!(normalized[2].method, "session/set_model");
        assert_eq!(normalized[2].values[0].name, "Agent Model");
        let params = normalized[2].params("agent-session", "model-b").unwrap();
        assert_eq!(
            params.as_object().unwrap().get("modelId").unwrap().as_str(),
            Some("model-b")
        );
        let explicit = settings::normalize(&json(r#"{"modes":{"currentModeId":"default","availableModes":[{"id":"unusual","_meta":{"kind":"full_access"}}]}}"#)).unwrap();
        assert_eq!(explicit[0].full_access().unwrap(), Some("unusual"));
        let restored = settings::Setting::from_value(&normalized[0].to_value().unwrap()).unwrap();
        assert_eq!(restored.values.len(), 2);
        let params = json(
            r#"{"options":[{"optionId":"no","kind":"reject_once"},{"optionId":"once","kind":"allow_once"},{"optionId":"always","kind":"allow_always"}]}"#,
        );
        assert_eq!(
            settings::allow_option(&params).unwrap().unwrap().0,
            "always"
        );
        assert!(
            settings::allow_option(&json(
                r#"{"options":[{"optionId":"no","kind":"reject_once"}]}"#
            ))
            .unwrap()
            .is_none()
        );
        let server = d::MCPServer::from_value(&json(
            r#"{"name":"tools","transport":"http","url":"https://tools.test/mcp"}"#,
        ))
        .unwrap();
        assert!(new_session(core::slice::from_ref(&server), false).is_err());
        let params = new_session(&[server], true).unwrap();
        assert_eq!(
            params.as_object().unwrap().get("cwd").unwrap().as_str(),
            Some("/workspace")
        );
        assert_eq!(
            rpc_error(&json(
                r#"{"code":-32603,"message":"Internal error","data":"token expired"}"#
            ))
            .unwrap(),
            "Internal error (-32603): token expired"
        );
    }
}
