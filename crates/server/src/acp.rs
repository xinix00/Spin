//! De app bezit agentsessies; browserlinks zijn abonnees en runnerlinks dragen alleen bytes.
use super::*;
use alloc::{collections::VecDeque, vec::Vec};
use d::{
    protocol::{self as p, WireMessage},
    try_string,
};
use spin_core::{
    acp::session::{Prompt, Session, Signal},
    validation::text,
};
use spin_store::Mutation;
const MAX_AGENTS: usize = 16;
/// Zoveel mislukte starts achtereen zet een workflowstap opzij.
const AGENT_START_ATTEMPTS: i64 = 3;
/// Een ingelogde browser met een eigen historische cursor en schrijfrechten.
pub struct ChatLink {
    session: String,
    token_hash: String,
    actor: String,
    stream: String,
    viewer: bool,
    ready_sent: bool,
    fatal_sent: bool,
    cursor: u64,
    local: VecDeque<String>,
}
struct Initialize {
    protocol: i64,
    servers: d::List<d::MCPServer>,
    defaults: d::AgentSettings,
}
pub(crate) struct Agent {
    pub(super) options_probe: bool,
    pub(super) options_done: bool,
    pub(super) session_id: String,
    composition: String,
    operator: String,
    pub(super) client: String,
    pub(super) stream: String,
    artifact: String,
    initialize: Option<Initialize>,
    session: Option<Session>,
    pub(super) failure: String,
    saved: String,
    started_ms: u64,
    /// Sinds wanneer de runner van deze agent los is (0 = verbonden); de pauze
    /// telt niet mee voor de starttermijn van 45 s.
    detached_ms: u64,
    /// Wanneer `failure` voor het eerst werd gezet; de diagnose meldt het eenmalig.
    pub(super) failed_ms: u64,
    pub(super) failure_reported: bool,
    pub(super) closed: bool,
    launch: bool,
    /// De start kan nooit slagen (de prompt past niet): meteen parkeren.
    fatal: bool,
    queued: VecDeque<String>,
    after_turn: bool,
    last_preserve_ms: u64,
}
fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .as_object()
        .and_then(|value| value.get(key))
        .unwrap_or(&Value::Null)
}
fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    field(value, key).as_str().unwrap_or("")
}
fn now_ms(now: &Timestamp) -> Result<u64> {
    Ok(now.time()?.0 / 1_000_000)
}
impl<P: Persistence> Server<P> {
    pub(crate) fn stop_composition_agents(&mut self, composition: &str) -> Result {
        for agent in self
            .agents
            .iter_mut()
            .filter(|a| a.composition == composition && !a.closed)
        {
            agent.failure = try_string("agent workspace is stopping")?;
            if let Some(peer) = self
                .runners
                .iter_mut()
                .find(|p| p.client().id == agent.client)
            {
                if !peer.cancel(&agent.stream)? {
                    peer.enqueue(WireMessage {
                        r#type: try_string(p::MESSAGE_STREAM_CLOSE)?,
                        id: agent.stream.try_clone()?,
                        ..Default::default()
                    })?;
                }
                peer.finish_stream(&agent.stream);
            }
            self.store
                .set_composition_agent(composition, &agent.stream, None)?;
            agent.closed = true;
        }
        Ok(())
    }
    pub(crate) fn queue_workflow_prompt(
        &mut self,
        id: &str,
        message: String,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result {
        let index = self.ensure_agent(id, now, runtime)?;
        let agent = &mut self.agents[index];
        if agent.queued.len() >= 16
            || agent
                .queued
                .iter()
                .map(String::len)
                .sum::<usize>()
                .saturating_add(message.len())
                > 1 << 20
        {
            return Err(Error::Http(503, "workflow prompt queue is full"));
        }
        agent
            .queued
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        agent.queued.push_back(message);
        Ok(())
    }
    pub(crate) fn launch_workflow_agent(
        &mut self,
        id: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result {
        let view = self.store.workflow_for_session(id)?;
        if !matches!(
            view.run.status.as_str(),
            d::PHASE_RUN_QUEUED | d::PHASE_RUN_RUNNING
        ) || view.job.current_phase_run_id != view.run.id
            || matches!(
                view.phase.executor.as_str(),
                d::WORKFLOW_EXECUTOR_ACTION | d::WORKFLOW_EXECUTOR_EXPOSE
            )
        {
            return Ok(());
        }
        self.store.mark_workflow_phase_running(id, now)?;
        let index = match self.ensure_agent(id, now, runtime) {
            Ok(index) => index,
            Err(error) => {
                self.store.requeue_workflow_phase(id)?;
                return Err(error);
            }
        };
        if self.agents[index]
            .session
            .as_ref()
            .is_none_or(|s| !s.primed())
        {
            self.agents[index].launch = true;
        }
        Ok(())
    }
    pub(crate) fn restore_agents(&mut self, now: &Timestamp) -> Result {
        for composition in self.store.running_compositions()?.iter() {
            let Some(record) = &composition.agent else {
                continue;
            };
            let Some(capsule) = &composition.runtime else {
                continue;
            };
            if self.agents.iter().any(|a| a.stream == record.stream_id) {
                continue;
            }
            if !self
                .store
                .session(&record.session_id)
                .is_ok_and(|s| s.prepared_composition_id == composition.id)
            {
                self.store
                    .set_composition_agent(&composition.id, &record.stream_id, None)?;
                continue;
            }
            if self.agents.len() >= MAX_AGENTS {
                return Err(Error::Http(503, "agent capacity reached during recovery"));
            }
            let session = Session::adopt(record, now_ms(now)?)?;
            d::try_push(
                &mut self.agents,
                Agent {
                    options_probe: false,
                    options_done: false,
                    session_id: record.session_id.try_clone()?,
                    composition: composition.id.try_clone()?,
                    operator: record.operator.try_clone()?,
                    client: capsule.client_id.try_clone()?,
                    stream: record.stream_id.try_clone()?,
                    artifact: String::new(),
                    initialize: None,
                    session: Some(session),
                    failure: String::new(),
                    saved: record.to_json()?,
                    started_ms: 0,
                    closed: false,
                    launch: false,
                    fatal: false,
                    queued: VecDeque::new(),
                    after_turn: false,
                    last_preserve_ms: 0,
                    detached_ms: 0,
                    failed_ms: 0,
                    failure_reported: false,
                },
            )?;
        }
        Ok(())
    }
    /// Alleen expliciete bootconfiguratie bepaalt waar capsules de interne workflowtools bereiken.
    pub fn set_internal_url(&mut self, url: &str) -> Result {
        let url = url.trim().trim_end_matches('/');
        if !url.is_empty()
            && (!(url.starts_with("http://") || url.starts_with("https://"))
                || url.contains(['\r', '\n', '\0', '?', '#']))
        {
            return Err(Error::Http(400, "invalid internal URL"));
        }
        self.internal_url = try_string(url)?;
        Ok(())
    }
    pub(crate) fn chat_route(
        &mut self,
        request: &Request<'_>,
        actor: &str,
        token_hash: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        if request.method != "GET" {
            return Ok(None);
        }
        let Some(id) = request
            .path
            .strip_prefix("/api/sessions/")
            .and_then(|path| path.strip_suffix("/acp"))
            .filter(|id| !id.is_empty() && !id.contains('/'))
        else {
            return Ok(None);
        };
        if !auth::origin(request) {
            return Err(Error::Http(403, "invalid request origin"));
        }
        let session = self.store.session(id)?;
        let allowed = session.operator == actor
            || self
                .store
                .job(&session.job_id)
                .is_ok_and(|job| job.owner == actor || job.assignee == actor);
        self.maintain_agents(now, runtime)?;
        let index = self.ensure_agent(id, now, runtime)?;
        let link = ChatLink {
            session: try_string(id)?,
            token_hash: try_string(token_hash)?,
            actor: try_string(actor)?,
            stream: self.agents[index].stream.try_clone()?,
            viewer: !allowed,
            ready_sent: false,
            fatal_sent: false,
            cursor: 0,
            local: VecDeque::new(),
        };
        Ok(Some(Outcome::Chat(link)))
    }
    fn ensure_agent(
        &mut self,
        id: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<usize> {
        let record = self.store.session(id)?;
        if self
            .store
            .composition(&record.prepared_composition_id)?
            .runtime
            .as_ref()
            .is_none_or(|r| r.status == "stopped" || r.stop_pending)
        {
            return Err(Error::Http(
                409,
                "session composition is stopping or stopped",
            ));
        }
        if let Some(index) = self.agents.iter().position(|agent| agent.session_id == id) {
            if !self.agents[index].closed
                && self.agents[index].failure.is_empty()
                && self.agents[index]
                    .session
                    .as_ref()
                    .is_none_or(|session| !session.failed())
            {
                return Ok(index);
            }
            if !self.agents[index].closed {
                return Err(Error::Http(409, "previous agent is still closing"));
            }
            crate::note(
                &mut self.notes,
                format_args!(
                    "SPIN_AGENT_REPLACED session={id} stream={} failure={}",
                    self.agents[index].stream, self.agents[index].failure
                ),
            );
            self.agents.remove(index);
        }
        if self.agents.len() >= MAX_AGENTS {
            self.agents.retain(|agent| !agent.closed);
        }
        if self.agents.len() >= MAX_AGENTS {
            return Err(Error::Http(503, "agent capacity reached"));
        }
        self.agents
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        let session = self.store.session(id)?.try_clone()?;
        let composition = self
            .store
            .composition(&session.prepared_composition_id)?
            .try_clone()?;
        if composition.operator != session.operator {
            return Err(Error::Http(
                409,
                "session workspace belongs to another operator",
            ));
        }
        let capsule = composition
            .runtime
            .as_ref()
            .filter(|r| r.status != "stopped" && !r.stop_pending)
            .ok_or(Error::Http(409, "session composition is not running"))?;
        let enabled = composition
            .enabled
            .iter()
            .find(|enabled| enabled.name == "acp")
            .ok_or(Error::Http(409, "composition does not ENABLE acp"))?;
        let index = self
            .runners
            .iter()
            .position(|peer| peer.client().id == capsule.client_id)
            .ok_or(Error::Http(409, "capsule runner is offline"))?;
        // Een losse verbinding is geen fout: de peer bewaart de START_ENABLED-vraag
        // in zijn outbox en speelt die af zodra de runner terug is.
        let mut artifact = String::new();
        let mut defaults = d::AgentSettings::default();
        for (_, id) in composition.slot_bindings.iter() {
            if let Ok(layer) = self.store.artifact(id)
                && layer.enables.iter().any(|enabled| enabled.name == "acp")
            {
                artifact = layer.id.try_clone()?;
                defaults = layer.agent_settings.try_clone()?.unwrap_or_default();
                break;
            }
        }
        let (stream, initialize, active, saved) = if let Some(record) = composition
            .agent
            .as_ref()
            .filter(|agent| agent.session_id == session.id && !agent.stream_id.is_empty())
        {
            (
                record.stream_id.try_clone()?,
                None,
                Some(Session::adopt(record, now_ms(now)?)?),
                record.to_json()?,
            )
        } else {
            if !session.phase_run_id.is_empty() {
                let phase = self.store.workflow_for_session(&session.id)?.phase;
                if !phase.model.is_empty() {
                    defaults.model = phase.model;
                }
                if !phase.reasoning_effort.is_empty() {
                    defaults.reasoning_effort = phase.reasoning_effort;
                }
            }
            let mut servers = self
                .store
                .mcp_servers_for_operator(&session.operator, &session.mcp_server_ids)?;
            if !session.phase_run_id.is_empty() {
                if self.internal_url.is_empty() {
                    return Err(Error::Http(409, "SPIN_INTERNAL_URL is not configured"));
                }
                let token = auth::token(runtime)?;
                let hash = spin_security::digest_hex(token.as_bytes())?;
                self.store.set_workflow_token(&session.id, &hash)?;
                let mut headers = d::List::new();
                headers.push(d::MCPSecret {
                    name: try_string("Authorization")?,
                    value: text(format_args!("Bearer {token}"))?,
                })?;
                servers.push(d::MCPServer {
                    name: try_string("spin-workflow")?,
                    transport: try_string("http")?,
                    url: text(format_args!(
                        "{}/api/workflow/mcp/{}",
                        self.internal_url, session.id
                    ))?,
                    headers,
                    ..Default::default()
                })?;
            }
            (
                runtime.next("req")?,
                Some(Initialize {
                    protocol: enabled.protocol_version,
                    servers,
                    defaults,
                }),
                None,
                String::new(),
            )
        };
        let mut agent = Agent {
            options_probe: false,
            options_done: false,
            session_id: session.id,
            composition: composition.id.try_clone()?,
            operator: session.operator,
            client: capsule.client_id.try_clone()?,
            stream,
            artifact,
            initialize,
            session: active,
            failure: String::new(),
            saved,
            started_ms: now_ms(now)?,
            closed: false,
            launch: false,
            fatal: false,
            queued: VecDeque::new(),
            after_turn: false,
            last_preserve_ms: 0,
            detached_ms: 0,
            failed_ms: 0,
            failure_reported: false,
        };
        self.runners[index].adopt_stream(&agent.stream, false)?;
        if agent.initialize.is_some() {
            let request = WireMessage {
                version: p::PROTOCOL_VERSION,
                r#type: try_string(p::MESSAGE_REQUEST)?,
                id: agent.stream.try_clone()?,
                method: try_string(p::METHOD_START_ENABLED)?,
                payload: d::RawJson(Some(
                    p::EnabledPayload {
                        runtime: capsule.try_clone()?,
                        enablement: enabled.try_clone()?,
                        ..Default::default()
                    }
                    .to_value()?,
                )),
                ..Default::default()
            };
            if let Err(error) = self.runners[index].request(request) {
                self.runners[index].finish_stream(&agent.stream);
                return Err(error.into());
            }
        } else {
            agent.started_ms = 0;
        }
        self.agents.push(agent);
        Ok(self.agents.len() - 1)
    }
    pub(crate) fn start_options_agent(
        &mut self,
        composition: &str,
        artifact: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<String> {
        self.agents
            .retain(|agent| !agent.closed || !agent.options_probe);
        if self.agents.len() >= MAX_AGENTS {
            return Err(Error::Http(503, "agent capacity reached"));
        }
        self.agents
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        let composition = self.store.composition(composition)?;
        let capsule = composition
            .runtime
            .as_ref()
            .filter(|r| r.status == "ready" && !r.stop_pending)
            .ok_or(Error::Http(409, "probe capsule is not running"))?;
        let enabled = composition
            .enabled
            .iter()
            .find(|e| e.name == "acp")
            .ok_or(Error::Http(409, "composition does not ENABLE acp"))?;
        let peer = self
            .runners
            .iter_mut()
            .find(|p| p.client().id == capsule.client_id && p.is_connected())
            .ok_or(Error::Http(503, "probe runner is offline"))?;
        let stream = random.next("probe")?;
        let result = stream.try_clone()?;
        let request = WireMessage {
            r#type: try_string(p::MESSAGE_REQUEST)?,
            id: stream.try_clone()?,
            method: try_string(p::METHOD_START_ENABLED)?,
            payload: d::RawJson(Some(
                p::EnabledPayload {
                    runtime: capsule.try_clone()?,
                    enablement: enabled.try_clone()?,
                    ..Default::default()
                }
                .to_value()?,
            )),
            ..Default::default()
        };
        let agent = Agent {
            options_probe: true,
            options_done: false,
            session_id: String::new(),
            composition: composition.id.try_clone()?,
            operator: composition.operator.try_clone()?,
            client: capsule.client_id.try_clone()?,
            stream,
            artifact: try_string(artifact)?,
            initialize: Some(Initialize {
                protocol: enabled.protocol_version,
                servers: d::List::new(),
                defaults: d::AgentSettings::default(),
            }),
            session: None,
            failure: String::new(),
            saved: String::new(),
            started_ms: now_ms(now)?,
            closed: false,
            launch: false,
            fatal: false,
            queued: VecDeque::new(),
            after_turn: false,
            last_preserve_ms: 0,
            detached_ms: 0,
            failed_ms: 0,
            failure_reported: false,
        };
        peer.adopt_stream(&agent.stream, false)?;
        if let Err(error) = peer.request(request) {
            peer.finish_stream(&agent.stream);
            return Err(error.into());
        }
        self.agents.push(agent);
        Ok(result)
    }
    /// Een ingelogde kijker kan de sessie volgen; alleen de eigenaar/toegewezen collega schrijft.
    pub fn validate_chat(&mut self, link: &mut ChatLink, now: &Timestamp) -> Result {
        self.store.authenticate_session(&link.token_hash, now)?;
        let session = self.store.session(&link.session)?;
        link.viewer = session.operator != link.actor
            && !self
                .store
                .job(&session.job_id)
                .is_ok_and(|job| job.owner == link.actor || job.assignee == link.actor);
        Ok(())
    }
    /// Browseracties worden op de bestaande levende sessie uitgevoerd, zonder nieuw agentproces.
    pub fn chat_message(
        &mut self,
        link: &mut ChatLink,
        message: &Value,
        now: &Timestamp,
    ) -> Result {
        self.validate_chat(link, now)?;
        let prompt = if string(message, "type") == "prompt" {
            let primed = self
                .agents
                .iter()
                .find(|a| a.stream == link.stream)
                .and_then(|a| a.session.as_ref())
                .is_some_and(Session::primed);
            Some(if primed {
                try_string(string(message, "text"))?
            } else {
                primed_prompt(&self.store, &link.session, string(message, "text"))?
            })
        } else {
            None
        };
        let Some(agent) = self
            .agents
            .iter_mut()
            .find(|agent| agent.stream == link.stream)
        else {
            return Err(Error::Http(409, "agent is unavailable"));
        };
        if link.viewer {
            return Err(Error::Http(
                403,
                "only the session operator can send chat messages",
            ));
        }
        let session = agent
            .session
            .as_mut()
            .filter(|session| session.ready())
            .ok_or(Error::Http(409, "agent is not ready"))?;
        match string(message, "type") {
            "prompt" => {
                self.store.resume_workflow_phase_for_chat(
                    &agent.session_id,
                    &agent.operator,
                    now,
                )?;
                session.prompt(
                    Prompt {
                        text: prompt.ok_or(Error::Http(400, "missing chat prompt"))?,
                        attachments: prompt_attachments(&self.store, &agent.session_id, session)?,
                    },
                    now,
                    now_ms(now)?,
                )?;
                session.mark_primed();
            }
            "cancel" => session.cancel(now)?,
            "permission" => {
                session.permission(string(message, "request_id"), string(message, "option_id"))?
            }
            "auto_accept" => {
                if let Value::Bool(enabled) = field(message, "enabled") {
                    session.set_auto_accept(*enabled, now)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    /// Een geweigerde browseractie wordt alleen aan die browser teruggegeven.
    pub fn chat_error(&self, link: &mut ChatLink, error: &Error) -> Result {
        if link.local.len() >= 8 {
            return Err(Error::Http(503, "chat browser is too slow"));
        }
        let json = http::object(&[
            ("type", Value::string("error")?),
            ("error", Value::String(text(format_args!("{error}"))?)),
        ])?
        .to_json()?;
        link.local
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        link.local.push_back(json);
        Ok(())
    }
    /// Cursor 0 is ready, u64::MAX is een lokaal foutbericht; andere cursors zijn historie.
    pub fn chat_next(&self, link: &ChatLink) -> Result<Option<(u64, String)>> {
        if let Some(local) = link.local.front() {
            return Ok(Some((u64::MAX, local.try_clone()?)));
        }
        let Some(agent) = self.agents.iter().find(|agent| agent.stream == link.stream) else {
            return Ok(None);
        };
        if !agent.failure.is_empty() {
            return if link.fatal_sent {
                Ok(None)
            } else {
                Ok(Some((
                    u64::MAX - 1,
                    http::object(&[
                        ("type", Value::string("error")?),
                        ("error", agent.failure.to_value()?),
                        ("fatal", true.to_value()?),
                    ])?
                    .to_json()?,
                )))
            };
        }
        let Some(session) = &agent.session else {
            return Ok(None);
        };
        if !link.ready_sent && session.ready() {
            return Ok(Some((
                0,
                http::object(&[
                    ("type", Value::string("ready")?),
                    (
                        "agent_session_id",
                        Value::string(session.agent_session_id())?,
                    ),
                    ("agent_name", Value::string(session.agent_name())?),
                    ("busy", session.busy().to_value()?),
                    (
                        "queued",
                        i64::try_from(session.queued()).unwrap_or(16).to_value()?,
                    ),
                    ("viewer", link.viewer.to_value()?),
                    ("operator", agent.operator.to_value()?),
                ])?
                .to_json()?,
            )));
        }
        session
            .next_event(link.cursor)
            .map(|(sequence, event)| Ok((sequence, try_string(event)?)))
            .transpose()
    }
    /// Pas na overname door de socketbuffer schuift de cursor van deze browser op.
    pub fn chat_acknowledge(&self, link: &mut ChatLink, sequence: u64) {
        match sequence {
            0 => link.ready_sent = true,
            u64::MAX => {
                link.local.pop_front();
            }
            sequence if sequence == u64::MAX - 1 => link.fatal_sent = true,
            sequence => link.cursor = sequence,
        }
    }
    /// Het einde van de browser sluit de agent niet; een fatale agentfout sluit wel de browser.
    pub fn chat_done(&self, link: &ChatLink) -> bool {
        self.agents
            .iter()
            .find(|agent| agent.stream == link.stream)
            .is_none_or(|agent| {
                (!agent.failure.is_empty() && link.fatal_sent)
                    || agent.session.as_ref().is_some_and(|session| {
                        session.failed() && session.next_event(link.cursor).is_none()
                    })
            })
    }
    pub(crate) fn agent_runner(
        &mut self,
        client: &str,
        message: &WireMessage,
        now: &Timestamp,
    ) -> Result<bool> {
        let Some(agent) = self
            .agents
            .iter_mut()
            .find(|agent| agent.client == client && agent.stream == message.id)
        else {
            return Ok(false);
        };
        let result = (|| -> Result {
            match message.r#type.as_str() {
                p::MESSAGE_RESPONSE => {
                    if !message.error.is_empty() {
                        agent.failure = message.error.try_clone()?;
                        return Ok(());
                    }
                    if let Some(configuration) = agent.initialize.take() {
                        agent.session = Some(Session::new(
                            configuration.protocol,
                            configuration.servers,
                            configuration.defaults,
                            now_ms(now)?,
                        )?);
                    }
                }
                p::MESSAGE_STREAM_DATA => {
                    if let Some(session) = &mut agent.session {
                        session.receive(
                            message.data.0.as_deref().unwrap_or_default(),
                            now,
                            now_ms(now)?,
                        )?;
                    }
                }
                p::MESSAGE_STREAM_EXIT => {
                    let reason = if message.error.is_empty() {
                        text(format_args!(
                            "ACP process exited {}: {}",
                            message.execution.as_ref().map_or(-1, |e| e.exit_code),
                            message.execution.as_ref().map_or("", |e| e.output.as_str())
                        ))?
                    } else {
                        message.error.try_clone()?
                    };
                    if let Some(session) = &mut agent.session {
                        session.fail(&reason, now)?;
                    } else {
                        agent.failure = reason;
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = result {
            agent.failure = text(format_args!("{error}"))?;
        }
        Ok(true)
    }
    /// De app-poll draagt maximaal één ACP-regel per agent over en verwerkt daarna lifecycle.
    pub fn maintain_agents(&mut self, now: &Timestamp, runtime: &mut impl Runtime) -> Result {
        self.maintain_options(now, runtime)?;
        let now_ms = now_ms(now)?;
        for index in 0..self.agents.len() {
            let agent = &mut self.agents[index];
            if agent.closed {
                continue;
            }
            if !self.store.composition(&agent.composition).is_ok_and(|c| {
                c.runtime
                    .as_ref()
                    .is_some_and(|r| r.status != "stopped" && !r.stop_pending)
            }) {
                agent.failure = try_string("agent workspace was stopped or removed")?;
            }
            let connected = self
                .runners
                .iter()
                .any(|p| p.client().id == agent.client && p.is_connected());
            if !connected {
                if agent.detached_ms == 0 {
                    agent.detached_ms = now_ms;
                }
            } else if agent.detached_ms != 0 {
                // De tijd zonder runner telt niet mee: de start- en antwoordtermijnen
                // lopen pas weer vanaf de herverbinding.
                let paused = now_ms.saturating_sub(agent.detached_ms);
                if agent.started_ms != 0 {
                    agent.started_ms = agent.started_ms.saturating_add(paused);
                }
                agent.detached_ms = 0;
            }
            if connected
                && agent.initialize.is_some()
                && now_ms.saturating_sub(agent.started_ms) > 45_000
            {
                agent.failure = try_string("ACP entrypoint start timed out")?;
            }
            if let Some(session) = &mut agent.session {
                if connected && let Err(error) = session.tick(now, now_ms) {
                    agent.failure = text(format_args!("{error}"))?;
                }
                if agent.failure.is_empty()
                    && let Some(bytes) = session.outbound()
                {
                    let mut data = Vec::new();
                    data.try_reserve_exact(bytes.len())
                        .map_err(|_| d::Error::OutOfMemory)?;
                    data.extend_from_slice(bytes);
                    if let Some(peer) = self
                        .runners
                        .iter_mut()
                        .find(|p| p.client().id == agent.client)
                    {
                        match peer.enqueue(WireMessage {
                            r#type: try_string(p::MESSAGE_STREAM_INPUT)?,
                            id: agent.stream.try_clone()?,
                            data: d::Bytes(Some(data)),
                            ..Default::default()
                        }) {
                            Ok(()) => session.acknowledge(),
                            Err(spin_core::runner::Error::Full(_)) => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
                if agent.options_probe
                    && session.ready()
                    && !agent.options_done
                    && agent.failure.is_empty()
                {
                    self.store.set_artifact_agent_options(
                        &agent.artifact,
                        session.options(now)?,
                        now,
                    )?;
                    agent.options_done = true;
                }
                while let Some(signal) = session.take_signal() {
                    match signal {
                        Signal::Ready => {
                            // Alleen een agent die met een login draait, zet de
                            // opties neer: zonder login biedt Claude niets of
                            // alleen de API-variant.
                            if !agent.options_probe
                                && !agent.artifact.is_empty()
                                && self
                                    .store
                                    .composition(&agent.composition)
                                    .is_ok_and(|c| !c.logins.is_empty())
                            {
                                self.store.set_artifact_agent_options(
                                    &agent.artifact,
                                    session.options(now)?,
                                    now,
                                )?;
                            }
                        }
                        Signal::Idle if !agent.options_probe => {
                            agent.after_turn = true;
                            self.store.settle_workflow_chat_turn(
                                &agent.session_id,
                                Mutation { now, ids: runtime },
                            )?;
                        }
                        Signal::Idle => {}
                        Signal::TurnFailed(reason) => {
                            if self
                                .store
                                .workflow_for_session(&agent.session_id)
                                .is_ok_and(|view| view.run.status == d::PHASE_RUN_RUNNING)
                            {
                                self.store.requeue_workflow_phase(&agent.session_id)?;
                                session.fail(&reason, now)?;
                            }
                        }
                        Signal::Failed(reason) => agent.failure = reason,
                    }
                }
                if session.ready()
                    && agent.failure.is_empty()
                    && let Some(message) = agent.queued.front()
                {
                    let prompt = if session.primed() {
                        message.try_clone()?
                    } else {
                        primed_prompt(&self.store, &agent.session_id, message)?
                    };
                    match session.prompt(
                        Prompt {
                            text: prompt,
                            attachments: prompt_attachments(
                                &self.store,
                                &agent.session_id,
                                session,
                            )?,
                        },
                        now,
                        now_ms,
                    ) {
                        Ok(()) => {
                            session.mark_primed();
                            agent.queued.pop_front();
                        }
                        Err(error) => agent.failure = text(format_args!("{error}"))?,
                    }
                }
                if session.ready() && agent.launch && agent.failure.is_empty() {
                    let launch = (|| -> Result {
                        let view = self.store.workflow_for_session(&agent.session_id)?;
                        if view.phase.id != d::BRAINSTORM_PHASE_ID && !session.primed() {
                            let snapshot = self.store.snapshot()?;
                            let record = self.store.session(&agent.session_id)?;
                            let prompt = spin_core::prompts::workflow(
                                &snapshot,
                                &view.job,
                                record,
                                &view.run,
                                &view.phase,
                            )?;
                            session.prompt(
                                Prompt {
                                    text: prompt,
                                    attachments: prompt_attachments(
                                        &self.store,
                                        &agent.session_id,
                                        session,
                                    )?,
                                },
                                now,
                                now_ms,
                            )?;
                            session.mark_primed();
                        }
                        agent.launch = false;
                        Ok(())
                    })();
                    if let Err(error) = launch {
                        agent.failure = text(format_args!("{error}"))?;
                        agent.fatal = true;
                    }
                }
                if session.ready() && agent.failure.is_empty() && !agent.options_probe {
                    let record =
                        session.record(&agent.session_id, &agent.operator, &agent.stream)?;
                    let serialized = record.to_json()?;
                    if serialized != agent.saved {
                        self.store.set_composition_agent(
                            &agent.composition,
                            &agent.stream,
                            Some(record),
                        )?;
                        agent.saved = serialized;
                    }
                }
            }
            if !agent.failure.is_empty() {
                if agent.failed_ms == 0 {
                    agent.failed_ms = now_ms;
                }
                let stopping = self.store.composition(&agent.composition).is_ok_and(|c| {
                    c.runtime
                        .as_ref()
                        .is_some_and(|r| r.stop_pending || r.status == "stopped")
                });
                if !stopping
                    && self
                        .store
                        .workflow_for_session(&agent.session_id)
                        .is_ok_and(|view| view.run.status == d::PHASE_RUN_RUNNING)
                {
                    // Een agent die niet voorbij session/new komt, krijgt drie kansen;
                    // daarna gaat de stap opzij met de reden, in plaats van elke 30 s
                    // een nieuw proces op de runner (05-10: 152 starts in een uur).
                    let started = agent.session.as_ref().is_some_and(|s| s.primed());
                    let failures = if started {
                        0
                    } else {
                        self.agent_start_failures
                            .get(&agent.session_id)
                            .copied()
                            .unwrap_or(0)
                            .saturating_add(1)
                    };
                    if agent.fatal || failures >= AGENT_START_ATTEMPTS {
                        self.agent_start_failures.remove(&agent.session_id);
                        self.store
                            .park_workflow_phase(&agent.session_id, &agent.failure, now)?;
                        crate::note(
                            &mut self.notes,
                            format_args!(
                                "SPIN_AGENT_PARKED session={} attempts={failures} reason={}",
                                agent.session_id, agent.failure
                            ),
                        );
                    } else {
                        if started {
                            self.agent_start_failures.remove(&agent.session_id);
                        } else {
                            self.agent_start_failures
                                .insert(agent.session_id.try_clone()?, failures)?;
                        }
                        self.store.requeue_workflow_phase(&agent.session_id)?;
                    }
                }
                if self.store.composition(&agent.composition).is_ok() {
                    self.store
                        .set_composition_agent(&agent.composition, &agent.stream, None)?;
                }
                if let Some(peer) = self
                    .runners
                    .iter_mut()
                    .find(|p| p.client().id == agent.client)
                {
                    if !peer.cancel(&agent.stream)? {
                        peer.enqueue(WireMessage {
                            r#type: try_string(p::MESSAGE_STREAM_CLOSE)?,
                            id: agent.stream.try_clone()?,
                            ..Default::default()
                        })?;
                    }
                    peer.finish_stream(&agent.stream);
                }
                agent.closed = true;
            }
        }
        for index in 0..self.agents.len() {
            if !self.agents[index].after_turn
                || now_ms.saturating_sub(self.agents[index].last_preserve_ms) < 1000
            {
                continue;
            }
            self.agents[index].last_preserve_ms = now_ms;
            let id = self.agents[index].session_id.try_clone()?;
            match self.preserve_after_turn(&id, now, runtime) {
                Ok(Some(wait)) => {
                    self.detach_capsule(wait);
                    self.agents[index].after_turn = false;
                }
                Ok(None) | Err(Error::Store(spin_store::Error::NotFound)) => {
                    self.agents[index].after_turn = false
                }
                Err(_) => {} // Capaciteit of runner ontbreekt; de app blijft eigenaar van de retry.
            }
        }
        Ok(())
    }
}
fn primed_prompt<P: Persistence>(store: &Store<P>, id: &str, message: &str) -> Result<String> {
    if store.session(id)?.phase_run_id.is_empty() {
        return Ok(try_string(message)?);
    }
    let view = store.workflow_for_session(id)?;
    let base = spin_core::prompts::workflow(
        &store.snapshot()?,
        &view.job,
        store.session(id)?,
        &view.run,
        &view.phase,
    )?;
    if view.phase.id == d::BRAINSTORM_PHASE_ID {
        Ok(text(format_args!(
            "Hieronder staan de instructies en context van deze brainstorm; daarna het bericht van de gebruiker waarmee het gesprek begint.\n\n{base}\n\nBERICHT VAN DE GEBRUIKER\n{message}"
        ))?)
    } else {
        Ok(text(format_args!(
            "Deze agentsessie is opnieuw gestart. Hieronder staan eerst de volledige instructies, regels en context van de fase; daarna het bericht dat je nu oppakt. Werk niet verder zonder deze regels.\n\n{base}\n\n---\n\nHET BERICHT VAN NU\n{message}"
        ))?)
    }
}

fn prompt_attachments<P: Persistence>(
    store: &Store<P>,
    id: &str,
    session: &Session,
) -> Result<Vec<spin_core::acp::session::Attachment>> {
    let mut result = Vec::new();
    let record = store.session(id)?;
    if record.job_id.is_empty() {
        return Ok(result);
    }
    let job = match store.job(&record.job_id) {
        Ok(job) => job.try_clone()?,
        Err(spin_store::Error::NotFound) => return Ok(result),
        Err(error) => return Err(error.into()),
    };
    for job in [&job.id, &job.forked_from_job_id] {
        if job.is_empty() {
            continue;
        }
        for attachment in store.job_attachments(job)?.into_vec() {
            if session.attachment_sent(&attachment.id) {
                continue;
            }
            // Nooit ingebed: de bijlage staat op schijf in de capsule en de prompt
            // noemt haar pad; een agent leest en bekijkt haar daar zelf, ongeacht grootte.
            let uri = text(format_args!("file://{}", attachment.capsule_path))?;
            let block = http::object(&[
                ("type", Value::string("resource_link")?),
                ("uri", Value::string(&uri)?),
                ("name", Value::string(&attachment.name)?),
                ("mimeType", Value::string(&attachment.media_type)?),
                ("size", attachment.size.to_value()?),
                (
                    "description",
                    Value::string(&text(format_args!(
                        "Immutable Job attachment supplied by {}",
                        attachment.created_by
                    ))?)?,
                ),
            ])?;
            d::try_push(
                &mut result,
                spin_core::acp::session::Attachment {
                    id: attachment.id,
                    block,
                },
            )?;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Memory;
    impl Persistence for Memory {
        fn save(&mut self, _: &d::state::PersistedState) -> spin_store::Result {
            Ok(())
        }
    }
    struct Random(u64);
    impl Entropy for Random {
        fn fill(&mut self, bytes: &mut [u8]) -> spin_security::Result {
            for byte in bytes {
                self.0 += 1;
                *byte = self.0.to_le_bytes()[0];
            }
            Ok(())
        }
    }
    impl IdSource for Random {
        fn next(&mut self, prefix: &str) -> spin_store::Result<String> {
            self.0 += 1;
            Ok(text(format_args!("{prefix}_{}", self.0))?)
        }
    }
    fn take(server: &mut Server<Memory>, generation: u64) -> WireMessage {
        let (ticket, message) = server.runners[0].next(generation).unwrap().unwrap();
        let message = message.try_clone().unwrap();
        server.runners[0].acknowledge(ticket);
        message
    }
    fn answer(
        server: &mut Server<Memory>,
        stream: &str,
        request: &WireMessage,
        result: &str,
        now: &Timestamp,
    ) {
        let request = Value::from_json(request.data.0.as_deref().unwrap()).unwrap();
        let response = text(format_args!(
            "{{\"id\":{},\"result\":{result}}}\n",
            field(&request, "id").to_json().unwrap()
        ))
        .unwrap();
        server
            .agent_runner(
                "client",
                &WireMessage {
                    r#type: p::MESSAGE_STREAM_DATA.into(),
                    id: stream.into(),
                    data: d::Bytes(Some(response.into_bytes())),
                    ..Default::default()
                },
                now,
            )
            .unwrap();
    }
    #[test]
    fn phase_launch_sends_context_once_and_brainstorm_waits_for_the_person() {
        for brainstorm in [false, true] {
            let mut state = d::state::PersistedState::from_json(br#"{
              "jobs":{"job":{"id":"job","owner":"derek","current_phase_run_id":"run","objective":"Werkend","branch":"jobs/#1/main","template_snapshot":{"id":"tpl","phases":[{"id":"develop","instructions":"Volg deze regels","accept":{"target":"DONE"}}]}}},
              "sessions":{"ses":{"id":"ses","job_id":"job","phase_run_id":"run","operator":"derek","prepared_composition_id":"cmp","git_ref":"jobs/#1/sessions/one"}},
              "phase_runs":{"run":{"id":"run","session_id":"ses","job_id":"job","phase_id":"develop","status":"queued"}},
              "compositions":{"cmp":{"id":"cmp","operator":"derek","session_id":"ses","runtime":{"driver":"docker","client_id":"client","container_id":"container","status":"ready"},"enabled":[{"name":"acp","transport":"stdio","command":"agent","protocol_version":1}]}}
            }"#).unwrap();
            if brainstorm {
                state.phase_runs.get_mut("run").unwrap().phase_id = "brainstorm".into();
            }
            let mut server = Server::new(Store::new(state, Memory));
            server.set_internal_url("http://spin.internal").unwrap();
            let now = Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap();
            let mut random = Random(100);
            let mut peer = spin_core::runner::Peer::new(d::Client {
                id: "client".into(),
                ..Default::default()
            });
            let (generation, _) = peer.attach("runner", None, 0).unwrap();
            server.runners.push(peer);
            server
                .launch_workflow_agent("ses", &now, &mut random)
                .unwrap();
            assert_eq!(
                server.store.workflow_for_session("ses").unwrap().run.status,
                d::PHASE_RUN_RUNNING
            );
            let start = take(&mut server, generation);
            assert_eq!(start.method, p::METHOD_START_ENABLED);
            server.runners[0].response(&start.id);
            server
                .agent_runner(
                    "client",
                    &WireMessage {
                        r#type: p::MESSAGE_RESPONSE.into(),
                        id: start.id.try_clone().unwrap(),
                        ..Default::default()
                    },
                    &now,
                )
                .unwrap();
            server.maintain_agents(&now, &mut random).unwrap();
            let initialize = take(&mut server, generation);
            answer(
                &mut server,
                &start.id,
                &initialize,
                r#"{"protocolVersion":1,"agentCapabilities":{"mcpCapabilities":{"http":true}}}"#,
                &now,
            );
            server.maintain_agents(&now, &mut random).unwrap();
            let new = take(&mut server, generation);
            assert!(
                core::str::from_utf8(new.data.0.as_deref().unwrap())
                    .unwrap()
                    .contains("http://spin.internal/api/workflow/mcp/ses")
            );
            answer(
                &mut server,
                &start.id,
                &new,
                r#"{"sessionId":"agent-session"}"#,
                &now,
            );
            server.maintain_agents(&now, &mut random).unwrap();
            if brainstorm {
                assert!(server.runners[0].next(generation).unwrap().is_none());
                assert!(
                    !server
                        .store
                        .composition("cmp")
                        .unwrap()
                        .agent
                        .as_ref()
                        .unwrap()
                        .primed
                );
                server
                    .queue_workflow_prompt("ses", "Bespreek mijn idee".into(), &now, &mut random)
                    .unwrap();
                server.maintain_agents(&now, &mut random).unwrap();
            }
            server.maintain_agents(&now, &mut random).unwrap();
            let prompt = take(&mut server, generation);
            let json = Value::from_json(prompt.data.0.as_deref().unwrap()).unwrap();
            assert_eq!(string(&json, "method"), "session/prompt");
            let content = field(field(&json, "params"), "prompt").as_array().unwrap();
            let content = string(&content[0], "text");
            assert!(content.contains(if brainstorm {
                "BERICHT VAN DE GEBRUIKER\nBespreek mijn idee"
            } else {
                "Volg deze regels"
            }));
            assert!(
                server
                    .store
                    .composition("cmp")
                    .unwrap()
                    .agent
                    .as_ref()
                    .unwrap()
                    .primed
            );
            server
                .launch_workflow_agent("ses", &now, &mut random)
                .unwrap();
            server.maintain_agents(&now, &mut random).unwrap();
            assert!(server.runners[0].next(generation).unwrap().is_none());
        }
    }
}
