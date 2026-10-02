//! De agentsessie bezit handshake, prompts, toestemmingen en browserhistorie.
use super::*;
use settings::Setting;
const HISTORY_BYTES: usize = 4 << 20;
const PROMPT_BYTES: usize = 8 << 20;
/// Een reeds door de app voorbereide ACP-bijlage, met een duurzaam deduplicatie-ID.
pub struct Attachment {
    /// Dezelfde bijlage wordt maar eenmaal per levende agentsessie verstuurd.
    pub id: String,
    /// Een ACP content block, passend bij de onderhandelde prompt-capabilities.
    pub block: Value,
}
/// Eén operatorbericht, met eventuele nog niet verstuurde bijlagen.
pub struct Prompt {
    /// De tekst blijft samen met de bijlagen eigendom van de wachtrij.
    pub text: String,
    /// Alleen nieuwe IDs komen in de uiteindelijke prompt terecht.
    pub attachments: Vec<Attachment>,
}
struct Active {
    id: u64,
    attachments: Vec<String>,
    adopted: bool,
}
struct Steer {
    prompt: Prompt,
    attachments: Vec<String>,
}
struct Choice {
    method: String,
    params: Value,
    optional: bool,
}
struct History {
    sequence: u64,
    json: String,
}
struct Permission {
    params: Value,
    bytes: usize,
}
#[derive(PartialEq)]
enum Phase {
    Initialize,
    New,
    Configure,
    Ready,
    Failed,
}
/// De app verwerkt lifecyclewijzigingen onder dezelfde eigenaar als haar Store.
pub enum Signal {
    /// De agent heeft initialize, session/new en de gekozen instellingen bevestigd.
    Ready,
    /// De agent is klaar met zijn laatste beurt; er wacht geen volgende prompt.
    Idle,
    /// Een beurt faalde of eindigde zonder enige update.
    TurnFailed(String),
    /// De stream kan niet meer worden gebruikt.
    Failed(String),
}
/// Volledig transportonafhankelijke agentsessie; de runner draagt uitsluitend bytes.
pub struct Session {
    rpc: Rpc,
    phase: Phase,
    protocol: i64,
    agent_session: String,
    agent_name: String,
    servers: d::List<d::MCPServer>,
    defaults: d::AgentSettings,
    settings: Vec<Setting>,
    choices: VecDeque<Choice>,
    applying_optional: bool,
    prompt_caps: Value,
    steering: bool,
    auto_accept: bool,
    primed: bool,
    active: Option<Active>,
    queued: VecDeque<Prompt>,
    steers: Map<Steer>,
    permissions: Map<Permission>,
    permission_bytes: usize,
    sent: Map<bool>,
    received: usize,
    history: VecDeque<History>,
    history_bytes: usize,
    sequence: u64,
    signals: VecDeque<Signal>,
}
fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .as_object()
        .and_then(|object| object.get(name))
        .unwrap_or(&Value::Null)
}
fn string(value: &Value, name: &str) -> d::Fallible<String> {
    match field(value, name) {
        Value::Null => Ok(String::new()),
        value => String::from_value(value),
    }
}
fn boolean(value: &Value, name: &str) -> bool {
    matches!(field(value, name), Value::Bool(true))
}
fn prompt_size(prompt: &Prompt) -> d::Fallible<usize> {
    if prompt.attachments.len() > 128 {
        return Err(invalid("ACP", "too many prompt attachments"));
    }
    let mut bytes = prompt.text.len();
    for attachment in &prompt.attachments {
        if attachment.id.len() > 1024 {
            return Err(invalid("ACP", "attachment ID too long"));
        }
        bytes = bytes
            .saturating_add(attachment.block.to_json()?.len())
            .saturating_add(attachment.id.len());
    }
    if bytes > PROMPT_BYTES {
        return Err(invalid("ACP", "prompt exceeds byte budget"));
    }
    Ok(bytes)
}
impl Session {
    /// De eerste uitgaande regel is initialize; de app levert een monotone millisecondeklok.
    pub fn new(
        protocol: i64,
        servers: d::List<d::MCPServer>,
        defaults: d::AgentSettings,
        now_ms: u64,
    ) -> d::Fallible<Self> {
        let protocol = if protocol == 0 { 1 } else { protocol };
        let auto_accept = defaults.auto_accept.unwrap_or(true);
        let mut session = Self {
            rpc: Rpc::default(),
            phase: Phase::Initialize,
            protocol,
            agent_session: String::new(),
            agent_name: String::new(),
            servers,
            defaults,
            settings: Vec::new(),
            choices: VecDeque::new(),
            applying_optional: false,
            prompt_caps: Value::Null,
            steering: false,
            auto_accept,
            primed: false,
            active: None,
            queued: VecDeque::new(),
            steers: Map::new(),
            permissions: Map::new(),
            permission_bytes: 0,
            sent: Map::new(),
            received: 0,
            history: VecDeque::new(),
            history_bytes: 0,
            sequence: 0,
            signals: VecDeque::new(),
        };
        session.rpc.call(
            "initialize",
            initialize(protocol)?,
            Some(now_ms.saturating_add(15_000)),
        )?;
        Ok(session)
    }
    /// Een levende runnerstream hervat zonder initialize of een tweede agent te starten.
    pub fn adopt(record: &d::AgentProcess, last_id: u64) -> d::Fallible<Self> {
        let mut session = Self::new(
            record.protocol_version,
            d::List::new(),
            d::AgentSettings::default(),
            0,
        )?;
        session.rpc = Rpc::new(last_id);
        session.phase = Phase::Ready;
        session.agent_session = record.agent_session_id.try_clone()?;
        session.agent_name = record.agent_name.try_clone()?;
        session.steering = record.steering;
        session.auto_accept = record.auto_accept;
        session.primed = record.primed;
        session.prompt_caps = record.prompt_caps.0.try_clone()?.unwrap_or(Value::Null);
        session.settings =
            d::List::<Setting>::from_value(record.settings.0.as_ref().unwrap_or(&Value::Null))?
                .into_vec();
        if record.sent_attachments.len() > 4096
            || record.pending_permissions.len() > 64
            || session.settings.len() > 128
        {
            return Err(invalid("ACP", "adopted state exceeds session budget"));
        }
        for id in record.sent_attachments.iter() {
            if id.len() > 1024 {
                return Err(invalid("ACP", "attachment ID too long"));
            }
            session.sent.insert(id.try_clone()?, true)?;
        }
        for (id, params) in record.pending_permissions.iter() {
            session.keep_permission(id, params.0.as_ref().unwrap_or(&Value::Null))?;
        }
        if !record.prompt_id.is_empty() {
            let id = record
                .prompt_id
                .parse()
                .map_err(|_| invalid("ACP", "invalid adopted prompt ID"))?;
            session.rpc.adopt(id, "session/prompt")?;
            session.active = Some(Active {
                id,
                attachments: Vec::new(),
                adopted: true,
            });
            // Een vóór de herstart begonnen beurt kan zijn eerdere updates niet herhalen.
            session.received = 1;
        }
        Ok(session)
    }
    /// Alleen de bevestigde protocolfase bepaalt of prompts mogen beginnen.
    pub fn ready(&self) -> bool {
        self.phase == Phase::Ready
    }
    /// Een gefaalde sessie mag niet opnieuw als levende agent worden aangeboden.
    pub fn failed(&self) -> bool {
        self.phase == Phase::Failed
    }
    /// Agentnaam uit title, of name wanneer title ontbreekt.
    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }
    /// Het door session/new uitgegeven ID.
    pub fn agent_session_id(&self) -> &str {
        &self.agent_session
    }
    /// Een lopende prompt blijft busy over de overgang naar de volgende wachtende prompt.
    pub fn busy(&self) -> bool {
        self.active.is_some()
    }
    /// Aantal berichten dat op de lopende beurt wacht.
    pub fn queued(&self) -> usize {
        self.queued.len()
    }
    /// Vermijdt dat de app reeds verstuurde bijlagen opnieuw van opslag leest.
    pub fn attachment_sent(&self, id: &str) -> bool {
        self.sent.contains_key(id)
    }
    /// Capabilities voor het voorbereiden van rijke bijlagen aan de appgrens.
    pub fn prompt_capabilities(&self) -> &Value {
        &self.prompt_caps
    }
    /// De fase-instructie moet één keer door iedere nieuwe agentsessie worden gelezen.
    pub fn primed(&self) -> bool {
        self.primed
    }
    /// De app zet dit pas wanneer zij de volledige fase-instructie heeft aangeboden.
    pub fn mark_primed(&mut self) {
        self.primed = true;
    }
    /// Leent bytes tot de runner-outbox het complete bericht bezit.
    pub fn outbound(&self) -> Option<&[u8]> {
        self.rpc.outbound()
    }
    /// Bevestigt die overdracht.
    pub fn acknowledge(&mut self) {
        self.rpc.acknowledge();
    }
    /// Gedeeltelijke regels en late/duplicaatantwoorden worden door Rpc afgehandeld.
    pub fn receive(&mut self, bytes: &[u8], now: &d::Timestamp, now_ms: u64) -> d::Fallible {
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            self.rpc.receive(line)?;
            self.process(now, now_ms)?;
        }
        Ok(())
    }
    /// Deadlines blijven lopen zonder netwerkactiviteit.
    pub fn tick(&mut self, now: &d::Timestamp, now_ms: u64) -> d::Fallible {
        self.rpc.expire(now_ms)?;
        self.process(now, now_ms)
    }
    fn process(&mut self, now: &d::Timestamp, now_ms: u64) -> d::Fallible {
        while let Some(event) = self.rpc.event() {
            match event {
                Event::Method(value) => self.method(value, now)?,
                Event::Timeout { id, method } => self.reply(
                    id,
                    &method,
                    Err(try_string("ACP request timed out")?),
                    now,
                    now_ms,
                )?,
                Event::Reply { id, method, body } => {
                    let error = field(&body, "error");
                    let result = if matches!(error, Value::Null) {
                        Ok(field(&body, "result").try_clone()?)
                    } else {
                        Err(rpc_error(error)?)
                    };
                    self.reply(id, &method, result, now, now_ms)?;
                }
            }
        }
        Ok(())
    }
    fn reply(
        &mut self,
        id: u64,
        method: &str,
        result: Result<Value, String>,
        now: &d::Timestamp,
        now_ms: u64,
    ) -> d::Fallible {
        if self.phase == Phase::Failed {
            return Ok(());
        }
        if method == "session/prompt" {
            return self.end_turn(id, result, now, now_ms);
        }
        if method == "_session/steering" {
            return self.steered(id, result, now, now_ms);
        }
        let result = match result {
            Ok(result) => result,
            Err(error) if self.phase == Phase::Configure && self.applying_optional => {
                let _ = error;
                return self.configure(now_ms);
            }
            Err(error) => return self.fail(&error, now),
        };
        match self.phase {
            Phase::Initialize => {
                if field(&result, "protocolVersion").as_i64() != Some(self.protocol) {
                    return self.fail("ACP protocol negotiation failed", now);
                }
                let info = field(&result, "agentInfo");
                self.agent_name = string(info, "title")?;
                if self.agent_name.is_empty() {
                    self.agent_name = string(info, "name")?;
                }
                let capabilities = field(&result, "agentCapabilities");
                self.prompt_caps = field(capabilities, "promptCapabilities").try_clone()?;
                self.steering = boolean(field(field(&result, "_meta"), "steering"), "supported");
                let params = new_session(
                    &self.servers,
                    boolean(field(capabilities, "mcpCapabilities"), "http"),
                )?;
                self.rpc
                    .call("session/new", params, Some(now_ms.saturating_add(30_000)))?;
                self.phase = Phase::New;
            }
            Phase::New => {
                self.agent_session = string(&result, "sessionId")?;
                if self.agent_session.trim().is_empty() {
                    return self.fail("ACP session/new returned no sessionId", now);
                }
                self.settings = settings::normalize(&result)?;
                for category in ["mode", "model", "thought_level"] {
                    let Some(setting) = self.settings.iter().find(|s| s.category == category)
                    else {
                        continue;
                    };
                    let desired = match category {
                        "mode" => {
                            if self.defaults.mode.is_empty() {
                                setting.full_access()?
                            } else {
                                Some(self.defaults.mode.as_str())
                            }
                        }
                        "model" => Some(self.defaults.model.as_str()),
                        _ => Some(self.defaults.reasoning_effort.as_str()),
                    };
                    if let Some(value) = desired
                        .filter(|v| !v.is_empty() && (category != "mode" || *v != setting.current))
                    {
                        let choice = Choice {
                            method: setting.method.try_clone()?,
                            params: setting.params(&self.agent_session, value)?,
                            optional: category == "mode",
                        };
                        self.choices
                            .try_reserve(1)
                            .map_err(|_| d::Error::OutOfMemory)?;
                        self.choices.push_back(choice);
                    }
                }
                self.phase = Phase::Configure;
                self.configure(now_ms)?;
            }
            Phase::Configure => self.configure(now_ms)?,
            Phase::Ready | Phase::Failed => {}
        }
        Ok(())
    }
    fn configure(&mut self, now_ms: u64) -> d::Fallible {
        if let Some(choice) = self.choices.pop_front() {
            self.rpc.call(
                &choice.method,
                choice.params,
                Some(now_ms.saturating_add(30_000)),
            )?;
            self.applying_optional = choice.optional;
        } else {
            self.phase = Phase::Ready;
            self.signal(Signal::Ready)?;
        }
        Ok(())
    }
    fn method(&mut self, body: Value, now: &d::Timestamp) -> d::Fallible {
        if self.phase == Phase::Failed {
            return Ok(());
        }
        let params = field(&body, "params");
        match field(&body, "method").as_str().unwrap_or("") {
            "session/update" => {
                if self.agent_session.is_empty()
                    || field(params, "sessionId").as_str() == Some(self.agent_session.as_str())
                {
                    self.received = self.received.saturating_add(1);
                    self.broadcast(
                        "update",
                        &[("update", field(params, "update").try_clone()?)],
                        now,
                    )?;
                }
            }
            "session/request_permission" => {
                let id = field(&body, "id");
                if matches!(id, Value::Null) {
                    return Ok(());
                }
                if self.permissions.len() >= 64 {
                    return Err(invalid("ACP", "too many pending permissions"));
                }
                let key = id.to_json()?;
                self.keep_permission(&key, params)?;
                let option = if self.auto_accept {
                    settings::allow_option(params)?
                } else {
                    None
                };
                if let Some((option, name)) = option {
                    self.permission(&key, &option)?;
                    self.broadcast(
                        "permission",
                        &[
                            ("request_id", key.to_value()?),
                            ("params", params.try_clone()?),
                            ("auto", true.to_value()?),
                            ("choice", name.to_value()?),
                        ],
                        now,
                    )?;
                } else {
                    self.broadcast(
                        "permission",
                        &[
                            ("request_id", key.to_value()?),
                            ("params", params.try_clone()?),
                        ],
                        now,
                    )?;
                }
            }
            _ => {
                let id = field(&body, "id");
                if !matches!(id, Value::Null) {
                    self.rpc.unsupported(id.try_clone()?)?;
                }
            }
        }
        Ok(())
    }
    /// Nieuwe prompts volgen één lopende beurt, of de onderhandelde steering-extensie.
    pub fn prompt(&mut self, prompt: Prompt, now: &d::Timestamp, now_ms: u64) -> d::Fallible {
        if !self.ready() || prompt.text.trim().is_empty() {
            return Err(invalid("ACP", "session is not ready or prompt is empty"));
        }
        let mut queued_bytes = 0_usize;
        for queued in self
            .queued
            .iter()
            .chain(self.steers.iter().map(|(_, steer)| &steer.prompt))
        {
            queued_bytes = queued_bytes.saturating_add(prompt_size(queued)?);
        }
        if prompt_size(&prompt)? > PROMPT_BYTES.saturating_sub(queued_bytes)
            || self.queued.len() + self.steers.len() >= 16
        {
            return Err(invalid(
                "ACP",
                "too many messages are waiting for the running turn",
            ));
        }
        if self.active.is_none() {
            return self.start_prompt(prompt, now);
        }
        if self.steering {
            let (blocks, attachments) = self.blocks(&prompt)?;
            let id = self.rpc.call(
                "_session/steering",
                object(&[
                    ("sessionId", self.agent_session.to_value()?),
                    ("prompt", blocks),
                ])?,
                Some(now_ms.saturating_add(60_000)),
            )?;
            self.remember(&attachments)?;
            self.broadcast("user", &[("text", prompt.text.to_value()?)], now)?;
            self.steers.insert(
                text(format_args!("{id}"))?,
                Steer {
                    prompt,
                    attachments,
                },
            )?;
        } else {
            self.queue(prompt, now)?;
        }
        Ok(())
    }
    fn queue(&mut self, prompt: Prompt, now: &d::Timestamp) -> d::Fallible {
        if self.queued.len() >= 16 {
            return Err(invalid("ACP", "prompt queue is full"));
        }
        self.broadcast(
            "queued",
            &[
                ("text", prompt.text.to_value()?),
                (
                    "queued",
                    i64::try_from(self.queued.len() + 1)
                        .unwrap_or(16)
                        .to_value()?,
                ),
            ],
            now,
        )?;
        self.queued
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        self.queued.push_back(prompt);
        Ok(())
    }
    fn blocks(&self, prompt: &Prompt) -> d::Fallible<(Value, Vec<String>)> {
        let mut blocks = Vec::new();
        let mut ids = Vec::new();
        let mut bytes = prompt.text.len();
        d::try_push(
            &mut blocks,
            object(&[
                ("type", Value::string("text")?),
                ("text", Value::string(prompt.text.trim())?),
            ])?,
        )?;
        for attachment in &prompt.attachments {
            let id = attachment.id.trim();
            if id.is_empty() || self.sent.contains_key(id) || ids.iter().any(|seen| seen == id) {
                continue;
            }
            bytes = bytes.saturating_add(attachment.block.to_json()?.len());
            if bytes > PROMPT_BYTES || ids.len() >= 128 {
                return Err(invalid("ACP", "prompt attachment budget exceeded"));
            }
            d::try_push(&mut blocks, attachment.block.try_clone()?)?;
            d::try_push(&mut ids, try_string(id)?)?;
        }
        Ok((Value::Array(blocks), ids))
    }
    fn remember(&mut self, ids: &[String]) -> d::Fallible {
        if self.sent.len() + ids.len() > 4096 {
            return Err(invalid("ACP", "attachment history is full"));
        }
        for id in ids {
            self.sent.insert(id.try_clone()?, true)?;
        }
        Ok(())
    }
    fn start_prompt(&mut self, prompt: Prompt, now: &d::Timestamp) -> d::Fallible {
        let (blocks, attachments) = self.blocks(&prompt)?;
        let id = self.rpc.call(
            "session/prompt",
            object(&[
                ("sessionId", self.agent_session.to_value()?),
                ("prompt", blocks),
            ])?,
            None,
        )?;
        self.remember(&attachments)?;
        self.received = 0;
        self.active = Some(Active {
            id,
            attachments,
            adopted: false,
        });
        self.broadcast(
            "user",
            &[
                ("text", Value::string(prompt.text.trim())?),
                (
                    "queued",
                    i64::try_from(self.queued.len()).unwrap_or(16).to_value()?,
                ),
            ],
            now,
        )
    }
    fn steered(
        &mut self,
        id: u64,
        result: Result<Value, String>,
        now: &d::Timestamp,
        _: u64,
    ) -> d::Fallible {
        let Some(steer) = self.steers.remove(&text(format_args!("{id}"))?) else {
            return Ok(());
        };
        let outcome = result
            .as_ref()
            .ok()
            .and_then(|v| field(v, "outcome").as_str())
            .unwrap_or("");
        if matches!(outcome, "injected" | "startedNewTurn") {
            self.broadcast(
                "steered",
                &[(
                    "text",
                    Value::string(if outcome == "injected" {
                        "Ingestuurd in de lopende beurt"
                    } else {
                        "De beurt was net klaar; de agent is er een nieuwe mee begonnen"
                    })?,
                )],
                now,
            )?;
        } else {
            for id in steer.attachments {
                self.sent.remove(&id);
            }
            if self.active.is_some() {
                self.queue(steer.prompt, now)?;
            } else {
                self.start_prompt(steer.prompt, now)?;
            }
        }
        Ok(())
    }
    fn end_turn(
        &mut self,
        id: u64,
        result: Result<Value, String>,
        now: &d::Timestamp,
        _: u64,
    ) -> d::Fallible {
        if self.active.as_ref().is_none_or(|active| active.id != id) {
            return Ok(());
        }
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        let remaining = self.queued.len();
        match result {
            Err(error) => {
                for id in active.attachments {
                    self.sent.remove(&id);
                }
                self.queued.clear();
                self.discard_steers()?;
                self.broadcast(
                    "error",
                    &[(
                        "error",
                        Value::String(text(format_args!("ACP prompt: {error}"))?),
                    )],
                    now,
                )?;
                self.signal(Signal::Idle)?;
                self.signal(Signal::TurnFailed(error))?;
            }
            Ok(value) => {
                let stop = string(&value, "stopReason")?;
                if self.received == 0 {
                    let reason =
                        try_string("de agent beëindigde zijn beurt zonder iets te zeggen")?;
                    self.broadcast(
                        "error",
                        &[("error", Value::String(text(format_args!("ACP: {reason}"))?))],
                        now,
                    )?;
                    self.signal(Signal::TurnFailed(reason))?;
                }
                self.broadcast(
                    "turn_end",
                    &[
                        ("stop_reason", stop.to_value()?),
                        ("queued", i64::try_from(remaining).unwrap_or(16).to_value()?),
                    ],
                    now,
                )?;
                if let Some(next) = self.queued.pop_front() {
                    self.start_prompt(next, now)?;
                } else {
                    self.signal(Signal::Idle)?;
                }
            }
        }
        Ok(())
    }
    /// Annuleren gooit wachtende prompts weg en beantwoordt open permissions als cancelled.
    pub fn cancel(&mut self, now: &d::Timestamp) -> d::Fallible {
        if self.active.is_none() {
            return Err(invalid("ACP", "there is no running prompt"));
        }
        self.queued.clear();
        self.discard_steers()?;
        let mut permissions = Vec::new();
        for id in self.permissions.keys() {
            d::try_push(&mut permissions, try_string(id)?)?;
        }
        for id in permissions {
            self.permission_outcome(&id, object(&[("outcome", Value::string("cancelled")?)])?)?;
        }
        self.rpc.notify(
            "session/cancel",
            object(&[("sessionId", self.agent_session.to_value()?)])?,
        )?;
        if let Some(active) = &self.active
            && active.adopted
        {
            let id = active.id;
            self.rpc.forget(id)?;
            self.end_turn(
                id,
                Ok(object(&[("stopReason", Value::string("cancelled")?)])?),
                now,
                0,
            )?;
        }
        Ok(())
    }
    fn discard_steers(&mut self) -> d::Fallible {
        for (key, steer) in self.steers.iter() {
            let id = key
                .parse()
                .map_err(|_| invalid("ACP", "invalid steering ID"))?;
            self.rpc.forget(id)?;
            for attachment in &steer.attachments {
                self.sent.remove(attachment);
            }
        }
        self.steers = Map::new();
        Ok(())
    }
    /// Alleen een nog open permission-ID kan eenmaal worden beantwoord.
    pub fn permission(&mut self, id: &str, option: &str) -> d::Fallible {
        if option.trim().is_empty() {
            return Err(invalid("ACP", "permission option is required"));
        }
        self.permission_outcome(
            id.trim(),
            object(&[
                ("outcome", Value::string("selected")?),
                ("optionId", Value::string(option.trim())?),
            ])?,
        )
    }
    fn permission_outcome(&mut self, id: &str, outcome: Value) -> d::Fallible {
        if !self.permissions.contains_key(id) {
            return Err(invalid("ACP", "permission request is no longer pending"));
        }
        self.rpc.respond(
            Value::from_json(id.as_bytes())?,
            object(&[("outcome", outcome)])?,
        )?;
        if let Some(permission) = self.permissions.remove(id) {
            self.permission_bytes -= permission.bytes;
        }
        Ok(())
    }
    fn keep_permission(&mut self, id: &str, params: &Value) -> d::Fallible {
        let bytes = params.to_json()?.len().saturating_add(id.len());
        let replacing = self.permissions.get(id).map_or(0, |old| old.bytes);
        if id.len() > 1024 || bytes > PROMPT_BYTES.saturating_sub(self.permission_bytes - replacing)
        {
            return Err(invalid("ACP", "permission data exceeds budget"));
        }
        self.permissions.insert(
            try_string(id)?,
            Permission {
                params: params.try_clone()?,
                bytes,
            },
        )?;
        self.permission_bytes = self.permission_bytes - replacing + bytes;
        Ok(())
    }
    /// De bestaande per-sessieschakelaar wordt naar alle browserabonnees gepubliceerd.
    pub fn set_auto_accept(&mut self, enabled: bool, now: &d::Timestamp) -> d::Fallible {
        self.auto_accept = enabled;
        self.broadcast("auto_accept", &[("enabled", enabled.to_value()?)], now)
    }
    /// Een gestopte stream sluit de sessie en haar wachtrij expliciet.
    pub fn fail(&mut self, reason: &str, now: &d::Timestamp) -> d::Fallible {
        if self.phase == Phase::Failed {
            return Ok(());
        }
        self.phase = Phase::Failed;
        self.active = None;
        self.queued.clear();
        self.steers = Map::new();
        self.rpc = Rpc::new(self.rpc.last_id());
        self.permissions = Map::new();
        self.permission_bytes = 0;
        self.broadcast(
            "error",
            &[
                ("error", Value::string(reason)?),
                ("fatal", true.to_value()?),
            ],
            now,
        )?;
        self.signal(Signal::Failed(try_string(reason)?))
    }
    fn broadcast(
        &mut self,
        kind: &str,
        fields: &[(&str, Value)],
        now: &d::Timestamp,
    ) -> d::Fallible {
        let mut event = Object::new();
        event.push("type", Value::string(kind)?)?;
        event.push("at", now.to_value()?)?;
        for (key, value) in fields {
            event.push(key, value.try_clone()?)?;
        }
        let json = Value::Object(event).to_json()?;
        if json.len() > HISTORY_BYTES {
            return Err(invalid("ACP", "browser event exceeds history budget"));
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("ACP", "history IDs exhausted"))?;
        self.history
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        while self.history.len() >= 500
            || json.len() > HISTORY_BYTES.saturating_sub(self.history_bytes)
        {
            if let Some(old) = self.history.pop_front() {
                self.history_bytes -= old.json.len();
            } else {
                break;
            }
        }
        self.history_bytes += json.len();
        self.sequence = sequence;
        self.history.push_back(History { sequence, json });
        Ok(())
    }
    /// Iedere browser bewaart zijn eigen cursor; trage lezers laten de agent niet vastlopen.
    pub fn next_event(&self, after: u64) -> Option<(u64, &str)> {
        self.history
            .iter()
            .find(|event| event.sequence > after)
            .map(|event| (event.sequence, event.json.as_str()))
    }
    fn signal(&mut self, signal: Signal) -> d::Fallible {
        if self.signals.len() >= 64 {
            return Err(invalid("ACP", "session lifecycle queue is full"));
        }
        self.signals
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        self.signals.push_back(signal);
        Ok(())
    }
    /// De app verwerkt deze signalen voor zij meer runnerbytes aanbiedt.
    pub fn take_signal(&mut self) -> Option<Signal> {
        self.signals.pop_front()
    }
    /// Dezelfde duurzame AgentProcess als Go, zonder socket of proceshandvat.
    pub fn record(
        &self,
        session: &str,
        operator: &str,
        stream: &str,
    ) -> d::Fallible<d::AgentProcess> {
        let mut settings = Vec::new();
        for setting in &self.settings {
            d::try_push(&mut settings, setting.to_value()?)?;
        }
        let mut sent = d::List::new();
        for id in self.sent.keys() {
            sent.push(try_string(id)?)?;
        }
        let mut permissions = d::WireMap::new();
        for (id, permission) in self.permissions.iter() {
            permissions.insert(
                try_string(id)?,
                d::RawJson(Some(permission.params.try_clone()?)),
            )?;
        }
        Ok(d::AgentProcess {
            session_id: try_string(session)?,
            operator: try_string(operator)?,
            stream_id: try_string(stream)?,
            agent_session_id: self.agent_session.try_clone()?,
            agent_name: self.agent_name.try_clone()?,
            protocol_version: self.protocol,
            steering: self.steering,
            prompt_caps: d::RawJson(Some(self.prompt_caps.try_clone()?)),
            settings: d::RawJson(Some(Value::Array(settings))),
            auto_accept: self.auto_accept,
            primed: self.primed,
            prompt_id: self
                .active
                .as_ref()
                .map(|active| text(format_args!("{}", active.id)))
                .transpose()?
                .unwrap_or_default(),
            sent_attachments: sent,
            pending_permissions: permissions,
        })
    }
    /// Beschikbare modellen, modes en gedachte-instellingen voor de bestaande laag-UI.
    pub fn options(&self, now: &d::Timestamp) -> d::Fallible<d::AgentOptions> {
        settings::options(&self.settings, &self.agent_name, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> d::Timestamp {
        d::Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap()
    }
    fn json(text: &str) -> Value {
        Value::from_json(text.as_bytes()).unwrap()
    }
    fn take(session: &mut Session, method: &str) -> u64 {
        let request = Value::from_json(session.outbound().unwrap()).unwrap();
        assert_eq!(field(&request, "method").as_str(), Some(method));
        let id = field(&request, "id").as_i64().unwrap() as u64;
        session.acknowledge();
        id
    }
    fn answer(session: &mut Session, id: u64, result: &str) {
        session
            .receive(
                text(format_args!("{{\"id\":{id},\"result\":{result}}}\n"))
                    .unwrap()
                    .as_bytes(),
                &now(),
                50,
            )
            .unwrap();
    }
    fn ready(steering: bool) -> Session {
        let mut session = Session::new(1, d::List::new(), d::AgentSettings::default(), 0).unwrap();
        let id = take(&mut session, "initialize");
        answer(&mut session, id, &r#"{"protocolVersion":1,"agentInfo":{"title":"Test Agent"},"_meta":{"steering":{"supported":STEERING}}}"#.replace("STEERING", if steering { "true" } else { "false" }));
        let id = take(&mut session, "session/new");
        answer(&mut session, id, r#"{"sessionId":"agent-session"}"#);
        assert!(session.ready());
        assert!(matches!(session.take_signal(), Some(Signal::Ready)));
        session
    }
    fn prompt(value: &str, attachment: bool) -> Prompt {
        let mut attachments = Vec::new();
        if attachment {
            attachments.push(Attachment { id: "file-1".into(), block: json(r#"{"type":"resource_link","uri":"file:///workspace/reference.txt","name":"reference"}"#) });
        }
        Prompt {
            text: value.into(),
            attachments,
        }
    }
    fn update(session: &mut Session) {
        session.receive(b"{\"method\":\"session/update\",\"params\":{\"sessionId\":\"agent-session\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"working\"}}}}\n", &now(), 1).unwrap();
    }
    #[test]
    fn initialization_applies_negotiated_settings_before_ready_and_checks_protocol() {
        let defaults = d::AgentSettings {
            model: "model-b".into(),
            reasoning_effort: "high".into(),
            ..Default::default()
        };
        let mut session = Session::new(0, d::List::new(), defaults, 0).unwrap();
        let id = take(&mut session, "initialize");
        answer(&mut session, id, r#"{"protocolVersion":1}"#);
        let id = take(&mut session, "session/new");
        answer(
            &mut session,
            id,
            r#"{"sessionId":"agent-session","configOptions":[{"id":"mode","currentValue":"agent","options":[{"value":"agent-full-access"}]},{"id":"model","options":[{"value":"model-b"}]},{"id":"reasoning_effort","options":[{"value":"high"}]}]}"#,
        );
        for (config, expected) in [
            ("mode", "agent-full-access"),
            ("model", "model-b"),
            ("reasoning_effort", "high"),
        ] {
            assert!(!session.ready());
            let value = Value::from_json(session.outbound().unwrap()).unwrap();
            assert_eq!(
                field(field(&value, "params"), "configId").as_str(),
                Some(config)
            );
            assert_eq!(
                field(field(&value, "params"), "value").as_str(),
                Some(expected)
            );
            let id = take(&mut session, "session/set_config_option");
            answer(&mut session, id, "{}");
        }
        assert!(session.ready());
        let mut wrong = Session::new(1, d::List::new(), d::AgentSettings::default(), 0).unwrap();
        let id = take(&mut wrong, "initialize");
        answer(&mut wrong, id, r#"{"protocolVersion":2}"#);
        assert!(!wrong.ready());
        assert!(matches!(wrong.take_signal(), Some(Signal::Failed(_))));
        assert!(wrong.outbound().is_none());
    }
    #[test]
    fn queued_turns_attachment_deduplication_permissions_and_failures() {
        let mut session = ready(false);
        session.prompt(prompt("first", true), &now(), 0).unwrap();
        let request = Value::from_json(session.outbound().unwrap()).unwrap();
        assert_eq!(
            field(field(&request, "params"), "prompt")
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let first = take(&mut session, "session/prompt");
        session.prompt(prompt("second", true), &now(), 0).unwrap();
        assert_eq!(session.queued(), 1);
        session.receive(b"{\"id\":\"permission-1\",\"method\":\"session/request_permission\",\"params\":{\"options\":[{\"optionId\":\"yes\",\"kind\":\"allow_once\"}]}}\n", &now(), 0).unwrap();
        let permission = Value::from_json(session.outbound().unwrap()).unwrap();
        assert_eq!(field(&permission, "id").as_str(), Some("permission-1"));
        assert_eq!(
            field(field(field(&permission, "result"), "outcome"), "optionId").as_str(),
            Some("yes")
        );
        session.acknowledge();
        assert!(session.permission("\"permission-1\"", "yes").is_err());
        update(&mut session);
        answer(&mut session, first, r#"{"stopReason":"end_turn"}"#);
        assert!(session.busy());
        assert_eq!(session.queued(), 0);
        let request = Value::from_json(session.outbound().unwrap()).unwrap();
        assert_eq!(
            field(field(&request, "params"), "prompt")
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let second = take(&mut session, "session/prompt");
        session
            .prompt(prompt("discard this", false), &now(), 0)
            .unwrap();
        session.receive(text(format_args!(r#"{{"id":{second},"error":{{"code":-32603,"message":"Internal error","data":"token expired"}}}}
"#)).unwrap().as_bytes(), &now(), 0).unwrap();
        assert!(!session.busy());
        assert_eq!(session.queued(), 0);
        assert!(matches!(session.take_signal(), Some(Signal::Idle)));
        assert!(
            matches!(session.take_signal(), Some(Signal::TurnFailed(reason)) if reason.contains("token expired"))
        );
    }
    #[test]
    fn steering_fallback_adopted_cancel_and_late_reply_do_not_wedge_the_session() {
        let mut session = ready(true);
        session.prompt(prompt("first", false), &now(), 0).unwrap();
        let first = take(&mut session, "session/prompt");
        session.prompt(prompt("steer", false), &now(), 0).unwrap();
        let steer = take(&mut session, "_session/steering");
        answer(&mut session, steer, r#"{"outcome":"injected"}"#);
        assert_eq!(session.queued(), 0);
        session
            .prompt(prompt("fallback", false), &now(), 0)
            .unwrap();
        let steer = take(&mut session, "_session/steering");
        answer(&mut session, steer, r#"{"outcome":"unsupported"}"#);
        assert_eq!(session.queued(), 1);
        session.mark_primed();
        let record = session.record("spin-session", "derek", "stream-1").unwrap();
        let mut adopted = Session::adopt(&record, 900).unwrap();
        assert!(adopted.primed());
        assert!(adopted.busy());
        assert!(adopted.outbound().is_none());
        adopted.cancel(&now()).unwrap();
        assert!(!adopted.busy());
        let cancellation = Value::from_json(adopted.outbound().unwrap()).unwrap();
        assert_eq!(
            field(&cancellation, "method").as_str(),
            Some("session/cancel")
        );
        adopted.acknowledge();
        answer(&mut adopted, first, r#"{"stopReason":"cancelled"}"#);
        adopted
            .prompt(prompt("after restart", false), &now(), 10)
            .unwrap();
        assert_eq!(take(&mut adopted, "session/prompt"), 901);
    }
    #[test]
    fn cancelled_steering_cannot_restart_after_a_late_refusal() {
        let mut session = ready(true);
        session.prompt(prompt("first", false), &now(), 0).unwrap();
        let first = take(&mut session, "session/prompt");
        session.prompt(prompt("steer", true), &now(), 0).unwrap();
        let steer = take(&mut session, "_session/steering");
        session.cancel(&now()).unwrap();
        session.acknowledge();
        update(&mut session);
        answer(&mut session, first, r#"{"stopReason":"cancelled"}"#);
        answer(&mut session, steer, r#"{"outcome":"unsupported"}"#);
        assert!(!session.busy());
        assert_eq!(session.queued(), 0);
        assert!(session.outbound().is_none());
    }
}
