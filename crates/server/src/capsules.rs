//! Runnerwerk verlaat de HTTP-taak; de app-eigenaar bevestigt pas daarna de graaf.
use super::*;
use d::{
    List, RawJson,
    protocol::{self as p, WireMessage},
    try_string,
};
use spin_core::validation::text;
use spin_store::Context;

const MAX_CALLS: usize = 64;
const CALL_LIFETIME_NS: u64 = 15 * 60 * 1_000_000_000;
fn maintenance_context(error: Error, context: &'static str) -> Error {
    match error {
        Error::Store(spin_store::Error::NotFound) => Error::Http(404, context),
        other => other,
    }
}
/// Een HTTP-verzoek wacht op precies één opdracht; verbreken vernietigt geen capsule-start.
pub struct CapsuleWait {
    pub(crate) id: String,
    response_session: Option<String>,
}
// Hoogstens 64 eigenaars in de faalbaar gereserveerde pool; geen tweede heapobject per stap.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Action {
    Start {
        recording: String,
        actor: String,
    },
    Execute {
        recording: String,
        actor: String,
    },
    Seal {
        recording: String,
        actor: String,
        snapshot: Option<d::CapsuleSnapshot>,
    },
    Cancel {
        recording: String,
        actor: String,
    },
    Rebase {
        recording: String,
        actor: String,
    },
    Materialize(crate::materialize::Placement),
    Workflow(crate::workflow_mcp::Acceptance),
    Delivery(crate::workflow_mcp::Delivery),
    Preserve(crate::workspace::Preservation),
    Browse(crate::code::Browse),
    Inspect(crate::code::Inspection),
    App(crate::apps::Work),
    Merge(crate::actions::Merge),
    ReadFile(crate::management::ReadFile),
    Probe(crate::management::Probe),
    RemoveSnapshot {
        artifact: String,
    },
    Login(crate::login_operations::Capture),
    Attachment {
        composition: String,
        key: String,
        stamp: String,
    },
    Watch {
        composition: String,
        stamp: String,
    },
    /// Geen opdracht aan een runner: een stap in de wachtrij waarvoor geen login
    /// van `layer` vrij is. Alleen voor de UI en de diagnose; verdwijnt zodra de
    /// stap start of niet meer in de wachtrij staat.
    WaitLogin {
        session: String,
        layer: String,
    },
}
impl Action {
    pub(crate) fn object(&self) -> &str {
        match self {
            Self::WaitLogin { session, .. } => session,
            Self::Start { recording, .. }
            | Self::Execute { recording, .. }
            | Self::Seal { recording, .. }
            | Self::Cancel { recording, .. }
            | Self::Rebase { recording, .. } => recording,
            Self::Materialize(work) => &work.id,
            Self::Workflow(work) => &work.composition,
            Self::Delivery(work) => &work.composition,
            Self::Preserve(work) => &work.composition,
            Self::Browse(work) => &work.repository,
            Self::Inspect(work) => &work.composition,
            Self::App(work) => &work.object,
            Self::Merge(work) => &work.session,
            Self::ReadFile(work) => &work.composition,
            Self::Probe(work) => &work.composition,
            Self::RemoveSnapshot { artifact } => artifact,
            Self::Login(work) => &work.composition,
            Self::Watch { composition, .. } | Self::Attachment { composition, .. } => composition,
        }
    }
    fn persistent(&self) -> bool {
        matches!(
            self,
            Self::Start { .. } | Self::Rebase { .. } | Self::Seal { .. }
        )
    }
}
pub(crate) struct Call {
    pub(crate) id: String,
    pub(crate) client: String,
    pub(crate) request_id: String,
    start: Option<WireMessage>,
    pub(crate) action: Action,
    pub(crate) started: Timestamp,
    pub(crate) updated: Timestamp,
    response: Option<Response>,
    pub(crate) finished: bool,
    pub(crate) error: bool,
    artifact: Option<d::Artifact>,
    waiting: Option<WireMessage>,
    detached: bool,
    /// De runner die deze opdracht als laatste weigerde (vol of accepts=false).
    refused_by: String,
    /// Wanneer de diagnose dit wachten voor het laatst meldde (eens per 60 s).
    reported_ms: u64,
    /// De laatste stand die de runner van dit verzoek meldde (stage, message, bytes).
    pub(crate) progress: d::SealStatus,
}
impl Call {
    /// Of de opdracht nog in de rij ligt en niet bij een runner.
    pub(crate) fn queued(&self) -> bool {
        !self.finished && self.waiting.is_some()
    }
}
/// Vanaf deze wachttijd meldt de diagnose een opdracht die nog op een runner wacht.
const WAIT_REPORT_AFTER_MS: u64 = 30_000;
/// Een wachtende opdracht of een gefaalde agent wordt hoogstens eens per minuut gemeld.
const WAIT_REPORT_EVERY_MS: u64 = 60_000;
impl<P: Persistence> Server<P> {
    fn session_capsule_route(
        &mut self,
        req: &Request<'_>,
        actor: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        if req.method != "POST" {
            return Ok(None);
        }
        if let Some(job) = req
            .path
            .strip_prefix("/api/jobs/")
            .and_then(|p| p.strip_suffix("/sessions"))
            .filter(|p| !p.is_empty() && !p.contains('/'))
        {
            let mut request: d::CreateJobSessionRequest = Self::decode(req)?;
            request.operator = try_string(actor)?;
            let run = request.run;
            let id = random.next("ses")?;
            let session = self
                .store
                .create_job_session(job, request, Context { now, id: &id })?;
            let mut created = d::CreateJobSessionResponse {
                session,
                ..Default::default()
            };
            if run {
                match self.begin_materialize(
                    d::UseRequest {
                        selector: text(format_args!("session:{id}"))?,
                        operator: try_string(actor)?,
                        ..Default::default()
                    },
                    now,
                    random,
                ) {
                    Ok(mut wait) => {
                        wait.response_session = Some(id);
                        return Ok(Some(Outcome::Capsule(wait)));
                    }
                    Err(error) => created.run_error = text(format_args!("{error}"))?,
                }
            }
            return Ok(Some(Outcome::Response(Response::json(201, &created)?)));
        }
        let Some(path) = req.path.strip_prefix("/api/sessions/") else {
            return Ok(None);
        };
        let (id, stop) = if let Some(id) = path.strip_suffix("/capsule/stop") {
            (id, true)
        } else if let Some(id) = path.strip_suffix("/capsule") {
            (id, false)
        } else {
            return Ok(None);
        };
        if id.is_empty() || id.contains('/') {
            return Ok(None);
        }
        let session = self.session_access(id, actor)?.try_clone()?;
        if self.calls.iter().any(|c| {
            !c.finished && matches!(&c.action, Action::Materialize(work) if work.session == id)
        }) {
            if stop {
                return Err(Error::Http(409, "capsule is still being prepared"));
            }
            return Ok(Some(Outcome::Response(Response::json(
                202,
                &http::object(&[
                    ("status", Value::string("starting")?),
                    ("session_id", Value::string(id)?),
                ])?,
            )?)));
        }
        if let Ok(composition) = self.store.composition(&session.prepared_composition_id) {
            if composition.operator != session.operator {
                return Err(Error::Http(409, "capsule belongs to another operator"));
            }
            let running = composition
                .runtime
                .as_ref()
                .is_some_and(|r| r.status != "stopped");
            if !stop && running {
                if composition.runtime.as_ref().is_some_and(|r| r.stop_pending) {
                    return Err(Error::Http(409, "previous capsule is still stopping"));
                }
                return Ok(Some(Outcome::Response(Response::json(200, composition)?)));
            }
            if stop {
                if !running {
                    return Ok(Some(Outcome::Response(Response::json(200, composition)?)));
                }
                let id = composition.id.try_clone()?;
                return Ok(Some(
                    match self.begin_stop(&id, "api_capsule_stop", now, random)? {
                        Some(wait) => Outcome::Capsule(wait),
                        None => {
                            Outcome::Response(Response::json(202, self.store.composition(&id)?)?)
                        }
                    },
                ));
            }
        } else if stop {
            return Err(Error::Http(409, "Session has no capsule"));
        }
        let response = Response::json(
            202,
            &http::object(&[
                ("status", Value::string("starting")?),
                ("session_id", Value::string(id)?),
            ])?,
        )?;
        if !self.calls.iter().any(|c| {
            !c.finished && matches!(&c.action,Action::Materialize(work) if work.session==id)
        }) {
            let wait = self.begin_materialize(
                d::UseRequest {
                    selector: text(format_args!("session:{id}"))?,
                    operator: session.operator,
                    ..Default::default()
                },
                now,
                random,
            )?;
            self.detach_capsule(wait);
        }
        Ok(Some(Outcome::Response(response)))
    }
    /// De plek van een call ná een stap die calls kan starten: `reserve_call`
    /// ruimt dan afgeronde calls op en schuift de vector (GEMETEN 08-10:
    /// index 43 bij lengte 43 na een workflow-acceptatie, de server viel om).
    pub(crate) fn call_index(&self, id: &str) -> Result<usize> {
        self.calls
            .iter()
            .position(|c| c.id == id)
            .ok_or(Error::Http(409, "capsule operation vanished"))
    }
    pub(crate) fn reserve_call(&mut self, now: &Timestamp) -> Result {
        let cutoff = now.time()?.0.saturating_sub(CALL_LIFETIME_NS);
        self.calls
            .retain(|c| !c.finished || c.updated.time().map_or(true, |t| t.0 >= cutoff));
        if self.calls.len() >= MAX_CALLS {
            if let Some(index) = self.calls.iter().position(|c| c.finished && c.detached) {
                self.calls.remove(index);
            } else {
                return Err(Error::Http(503, "capsule operation capacity reached"));
            }
        }
        self.calls
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        Ok(())
    }
    pub(crate) fn enqueue_call(
        &mut self,
        action: Action,
        client: &str,
        method: &str,
        payload: &impl Wire,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<CapsuleWait> {
        self.reserve_call(now)?;
        if self
            .calls
            .iter()
            .any(|c| !c.finished && c.action.object() == action.object())
        {
            return Err(Error::Http(409, "capsule operation already in progress"));
        }
        let id = runtime.next("req")?;
        let wait = CapsuleWait {
            id: id.try_clone()?,
            response_session: None,
        };
        let mut call = Call {
            id: id.try_clone()?,
            client: try_string(client)?,
            request_id: id.try_clone()?,
            start: None,
            action,
            started: now.try_clone()?,
            updated: now.try_clone()?,
            response: None,
            finished: false,
            error: false,
            artifact: None,
            waiting: None,
            detached: false,
            progress: d::SealStatus::default(),
            refused_by: String::new(),
            reported_ms: 0,
        };
        let message = WireMessage {
            r#type: try_string(p::MESSAGE_REQUEST)?,
            id,
            method: try_string(method)?,
            payload: RawJson(Some(payload.to_value()?)),
            ..Default::default()
        };
        if client.is_empty() {
            call.waiting = Some(message);
        } else {
            let peer = self
                .runners
                .iter_mut()
                .find(|p| p.client().id == client)
                .ok_or(Error::Http(503, "runner is not connected"))?;
            send_to_peer(peer, &mut call, message, runtime)?;
        }
        // De vectorruimte is gereserveerd vóór de aanvraag bij de peer terechtkwam.
        self.calls.push(call);
        self.display_changed();
        Ok(wait)
    }
    pub(crate) fn choose_runner(&mut self, now: &Timestamp) -> Result<String> {
        let now_ms = now.time()?.0 / 1_000_000;
        for offset in 0..self.runners.len() {
            let index = (self.runner_cursor + offset) % self.runners.len();
            let peer = &self.runners[index];
            if peer.is_available(now_ms) {
                let id = peer.client().id.try_clone()?;
                self.runner_cursor = (index + 1) % self.runners.len();
                return Ok(id);
            }
        }
        Err(Error::Http(503, "waiting for a connected Docker runner"))
    }
    /// Diagnose an interrupted maintenance pass without exposing prompts or credentials.
    pub fn capsule_diagnostics(&self, now_ms: u64, mut log: impl FnMut(core::fmt::Arguments<'_>)) {
        let mut remaining = 64;
        let mut log = |line: core::fmt::Arguments<'_>| {
            if remaining > 0 {
                remaining -= 1;
                log(line);
            }
        };
        for peer in &self.runners {
            let client = peer.client();
            log(format_args!(
                "SPIN_RUNNER_STATE client={} name={} connected={} available={} draining={} engine={}",
                client.id,
                client.name,
                peer.is_connected(),
                peer.is_available(now_ms),
                client.draining,
                client.capabilities.engine.available
            ));
        }
        for call in self.calls.iter().filter(|call| !call.finished) {
            if let Action::WaitLogin { session, layer } = &call.action {
                log(format_args!(
                    "SPIN_SESSION_WAITING_LOGIN session={session} layer={layer} age_s={}",
                    now_ms.saturating_sub(call.started.time().map_or(0, |t| t.0 / 1_000_000))
                        / 1000
                ));
                continue;
            }
            log(format_args!(
                "SPIN_CAPSULE_PENDING object={} client={} queued={} method={}",
                call.action.object(),
                call.client,
                call.waiting.is_some(),
                call.waiting
                    .as_ref()
                    .map_or("dispatched", |m| m.method.as_str())
            ));
            if let Some(message) = &call.waiting {
                log(format_args!(
                    "SPIN_CAPSULE_WAITING object={} client={} age_s={} reason={}",
                    call.action.object(),
                    call.client,
                    now_ms.saturating_sub(call.started.time().map_or(0, |t| t.0 / 1_000_000))
                        / 1000,
                    self.wait_reason(call)
                ));
                if !call.refused_by.is_empty() {
                    log(format_args!(
                        "SPIN_RUNNER_REFUSED client={} method={} retry_ms=5000",
                        call.refused_by, message.method
                    ));
                }
            }
        }
        for agent in self
            .agents
            .iter()
            .filter(|a| a.failed_ms != 0 && now_ms.saturating_sub(a.failed_ms) < 120_000)
        {
            log(format_args!(
                "SPIN_AGENT_FAILED session={} stream={} reason={}",
                agent.session_id, agent.stream, agent.failure
            ));
        }
        let Ok(snapshot) = self.store.snapshot() else {
            return;
        };
        for session in snapshot.sessions.iter().filter(|session| {
            session.status == d::SESSION_QUEUED && !session.phase_run_id.is_empty()
        }) {
            if let Err(error) = self.store.workflow_for_session(&session.id) {
                log(format_args!(
                    "SPIN_SESSION_WORKFLOW_FAILED session={} job={} run={} error={}",
                    session.id, session.job_id, session.phase_run_id, error
                ));
            }
        }
        for composition in snapshot.compositions.iter() {
            for layer in composition
                .layers
                .iter()
                .filter(|id| self.store.artifact(id).is_err())
            {
                log(format_args!(
                    "SPIN_CAPSULE_MISSING_LAYER composition={} layer={}",
                    composition.id, layer
                ));
            }
            if !composition.session_id.is_empty()
                && self.store.session(&composition.session_id).is_err()
            {
                log(format_args!(
                    "SPIN_CAPSULE_MISSING_SESSION composition={} session={}",
                    composition.id, composition.session_id
                ));
            }
        }
        for recording in snapshot
            .recordings
            .iter()
            .filter(|r| r.status == d::RECORDING_OPEN)
        {
            for layer in recording
                .parent_artifact_ids
                .iter()
                .filter(|id| self.store.artifact(id).is_err())
            {
                log(format_args!(
                    "SPIN_RECORDING_MISSING_LAYER recording={} layer={}",
                    recording.id, layer
                ));
            }
        }
    }
    /// Waarom een opdracht in de rij nog niet bij een runner ligt.
    pub(crate) fn wait_reason(&self, call: &Call) -> &'static str {
        if !call.refused_by.is_empty() {
            return "runner refused";
        }
        if call.client.is_empty() {
            return "no runner available";
        }
        match self.runners.iter().find(|p| p.client().id == call.client) {
            Some(peer) if !peer.is_connected() => "runner offline",
            Some(_) => "runner refused",
            None => "runner offline",
        }
    }
    /// Markeert wat de diagnose moet melden en zegt of er iets nieuws is: een
    /// opdracht die langer dan 30 s wacht of geweigerd is (eens per 60 s) en een
    /// agent die faalde (eenmalig). De server heeft geen eigen logregel; het
    /// resultaat van `maintain_capsules` laat de runtime `capsule_diagnostics` loggen.
    fn note_waiting(&mut self, now_ms: u64) -> bool {
        let mut due = false;
        for call in self.calls.iter_mut().filter(|c| !c.finished) {
            let waits = call.waiting.is_some() || matches!(call.action, Action::WaitLogin { .. });
            let age = now_ms.saturating_sub(call.started.time().map_or(0, |t| t.0 / 1_000_000));
            if waits
                && (age >= WAIT_REPORT_AFTER_MS || !call.refused_by.is_empty())
                && now_ms.saturating_sub(call.reported_ms) >= WAIT_REPORT_EVERY_MS
            {
                call.reported_ms = now_ms;
                due = true;
            }
        }
        for agent in self
            .agents
            .iter_mut()
            .filter(|a| a.failed_ms != 0 && !a.failure_reported)
        {
            agent.failure_reported = true;
            due = true;
        }
        due
    }
    /// Onthoudt dat een stap in de wachtrij op een login van `layer` wacht.
    pub(crate) fn note_login_wait(
        &mut self,
        session: &str,
        layer: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result {
        if session.is_empty() {
            return Ok(());
        }
        if let Some(call) = self.calls.iter_mut().find(
            |c| matches!(&c.action, Action::WaitLogin { session: s, layer: l } if s == session && l == layer),
        ) {
            call.updated = now.try_clone()?;
            return Ok(());
        }
        self.forget_login_wait(session);
        self.reserve_call(now)?;
        let id = runtime.next("req")?;
        self.calls.push(Call {
            id: id.try_clone()?,
            client: String::new(),
            request_id: id,
            start: None,
            action: Action::WaitLogin {
                session: try_string(session)?,
                layer: try_string(layer)?,
            },
            started: now.try_clone()?,
            updated: now.try_clone()?,
            response: None,
            finished: false,
            error: false,
            artifact: None,
            waiting: None,
            detached: false,
            progress: d::SealStatus::default(),
            refused_by: String::new(),
            reported_ms: 0,
        });
        self.display_changed();
        Ok(())
    }
    /// De stap start of staat niet meer in de wachtrij: de login-wachtregel vervalt.
    pub(crate) fn forget_login_wait(&mut self, session: &str) {
        let before = self.calls.len();
        self.calls
            .retain(|c| !matches!(&c.action, Action::WaitLogin { session: s, .. } if s == session));
        if self.calls.len() != before {
            self.display_changed();
        }
    }
    /// Herstelt starts na een serverherstart, kiest een runner en begrenst de levensduur.
    /// De host roept dit periodiek aan; geen netwerk- of klok-I/O vindt hier plaats.
    pub fn maintain_capsules(&mut self, now: &Timestamp, runtime: &mut impl Runtime) -> Result {
        let time = now.time()?.0;
        for index in 0..self.calls.len() {
            if self.calls[index].finished
                || matches!(self.calls[index].action, Action::WaitLogin { .. })
            {
                continue;
            }
            // Vanaf de laatste activiteit (uitdelen, voortgang, antwoord), niet vanaf
            // het aanmaken: een stap die eerst minuten op een login wachtte, kreeg
            // anders geen volle termijn meer voor het bouwen (05-10: drie keer
            // "runner operation timed out" na 60-120 s bouwen, stap geparkeerd).
            if time.saturating_sub(self.calls[index].updated.time()?.0) >= CALL_LIFETIME_NS {
                let id = self.calls[index].request_id.try_clone()?;
                let client = self.calls[index].client.try_clone()?;
                if let Some(peer) = self.runners.iter_mut().find(|p| p.client().id == client) {
                    peer.cancel(&id)?;
                }
                self.calls[index].waiting = None;
                self.finish_capsule(
                    &client,
                    None,
                    &WireMessage {
                        id,
                        error: try_string("runner operation timed out")?,
                        ..Default::default()
                    },
                    now,
                    runtime,
                )?;
                continue;
            }
            if self.calls[index].waiting.is_some() {
                let client = if self.calls[index].client.is_empty() {
                    match self.choose_runner(now) {
                        Ok(client) => client,
                        Err(Error::Http(503, _)) => break,
                        Err(error) => return Err(error),
                    }
                } else {
                    let id = &self.calls[index].client;
                    if !self
                        .runners
                        .iter()
                        .any(|p| p.client().id == *id && p.is_available(time / 1_000_000))
                    {
                        continue;
                    }
                    id.try_clone()?
                };
                let message = self.calls[index]
                    .waiting
                    .as_ref()
                    .ok_or(Error::Http(500, "missing queued request"))?
                    .try_clone()?;
                let peer = self
                    .runners
                    .iter_mut()
                    .find(|p| p.client().id == client)
                    .ok_or(Error::Http(503, "runner disappeared"))?;
                send_to_peer(peer, &mut self.calls[index], message, runtime)?;
                self.calls[index].client = client;
                self.calls[index].waiting = None;
                self.calls[index].refused_by.clear();
                self.display_changed();
            }
        }
        let diagnostics_due = self.note_waiting(time / 1_000_000);
        self.retry_pending_stops(now, runtime)
            .map_err(|e| maintenance_context(e, "pending stop references missing data"))?;
        self.maintain_watches(now, runtime)
            .map_err(|e| maintenance_context(e, "workspace watch references missing data"))?;
        self.maintain_attachments(now, runtime)
            .map_err(|e| maintenance_context(e, "attachment references missing data"))?;
        // Een opname zonder runtime is duurzaam gestart, ook wanneer de oude
        // HTTP-verbinding of de vorige server-incarnatie niet meer bestaat.
        for recording in self.store.starting_recordings()?.into_vec() {
            if self
                .calls
                .iter()
                .any(|c| !c.finished && c.action.object() == recording.id)
            {
                continue;
            }
            let wait = self
                .start_recording(recording, now, runtime)
                .map_err(|e| maintenance_context(e, "recording references missing data"))?;
            self.detach_capsule(wait);
            break; // Hoogstens één herstelstart per actorronde.
        }
        if self
            .last_launch_sweep
            .is_none_or(|last| time.saturating_sub(last.0) >= 30_000_000_000)
        {
            self.last_launch_sweep = Some(d::Time(time));
            self.sweep_idle_capsules(now, runtime)
                .map_err(|e| maintenance_context(e, "idle capsule references missing data"))?;
            let mut launch_error = None;
            let snapshot = self.store.snapshot()?;
            // Een login-wachtregel hoort bij een stap die nog in de wachtrij staat.
            let before = self.calls.len();
            self.calls.retain(|c| {
                !matches!(&c.action, Action::WaitLogin { session, .. } if !snapshot.sessions.iter().any(|s| s.id == *session && s.status == d::SESSION_QUEUED && !s.phase_run_id.is_empty()))
            });
            if self.calls.len() != before {
                self.display_changed();
            }
            for session in snapshot.sessions.iter() {
                if session.status == d::SESSION_QUEUED
                    && !session.phase_run_id.is_empty()
                    && let Err(error) = self.schedule_session(session, now, runtime)
                {
                    // A broken attempt must not starve unrelated Jobs. Keep
                    // reporting its failure after the other launches have run.
                    launch_error.get_or_insert(error);
                }
            }
            if let Some(error) = launch_error {
                return Err(maintenance_context(
                    error,
                    "queued session references missing data",
                ));
            }
        }
        if diagnostics_due {
            // Geen storing: de server kan zelf niet loggen. De runtime leest deze
            // vlag na elke onderhoudsronde en draait dan `capsule_diagnostics` met
            // de SPIN_CAPSULE_WAITING-, SPIN_RUNNER_REFUSED-, SPIN_AGENT_FAILED- en
            // SPIN_SESSION_WAITING_LOGIN-regels.
            self.diagnostics_due = true;
        }
        Ok(())
    }
    /// De consoleregels van deze ronde (capsule-stops, agents die opzij gaan).
    pub fn take_notes(&mut self) -> alloc::vec::Vec<String> {
        core::mem::take(&mut self.notes)
    }
    /// Of het onderhoud regels voor de console klaar heeft; één keer waar per aanvraag.
    pub fn take_diagnostics_due(&mut self) -> bool {
        core::mem::take(&mut self.diagnostics_due)
    }
    fn owner_recording(&self, id: &str, actor: &str) -> Result<d::Recording> {
        let recording = self.store.recording(id)?;
        if recording.actor != actor {
            return Err(Error::Http(404, "not found"));
        }
        if recording.status != d::RECORDING_OPEN {
            return Err(Error::Http(409, "recording is not open"));
        }
        if recording.runtime.as_ref().is_some_and(|r| r.stop_pending) {
            return Err(Error::Http(409, "recording capsule is still stopping"));
        }
        Ok(recording.try_clone()?)
    }
    fn start_recording(
        &mut self,
        recording: d::Recording,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<CapsuleWait> {
        if let Some(capsule) = recording.runtime.as_ref().filter(|r| r.stop_pending) {
            let client = capsule.client_id.try_clone()?;
            let action = Action::Rebase {
                recording: recording.id.try_clone()?,
                actor: recording.actor.try_clone()?,
            };
            return self.enqueue_call(
                action,
                &client,
                p::METHOD_CANCEL_RECORDING,
                &p::RecordingPayload { recording },
                now,
                runtime,
            );
        }
        let client = match self.choose_runner(now) {
            Ok(client) => client,
            Err(Error::Http(503, _)) => String::new(),
            Err(error) => return Err(error),
        };
        let mut parents = List::new();
        for id in recording.parent_artifact_ids.iter() {
            parents.push(self.store.artifact(id)?.try_clone()?)?;
        }
        let (layers, artifacts, lifted) = self.store.recording_stack(&recording)?;
        let stack = lifted.then_some(d::engine::RecordingStack { layers, artifacts });
        let action = Action::Start {
            recording: recording.id.try_clone()?,
            actor: recording.actor.try_clone()?,
        };
        self.enqueue_call(
            action,
            &client,
            p::METHOD_START_RECORDING,
            &p::StartRecordingPayload {
                recording,
                parents,
                stack,
            },
            now,
            runtime,
        )
    }
    pub(crate) fn capsule_route(
        &mut self,
        req: &Request<'_>,
        actor: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        if let Some(outcome) = self.session_capsule_route(req, actor, now, runtime)? {
            return Ok(Some(outcome));
        }
        if req.method == "POST" && req.path == "/api/use" {
            let mut request: d::UseRequest = Self::decode(req)?;
            request.operator = try_string(actor)?;
            return Ok(Some(Outcome::Capsule(
                self.begin_materialize(request, now, runtime)?,
            )));
        }
        if let Some(id) = req
            .path
            .strip_prefix("/api/artifacts/")
            .and_then(|p| p.strip_suffix("/edit"))
            .filter(|id| req.method == "POST" && !id.is_empty() && !id.contains('/'))
        {
            self.reserve_call(now)?;
            let current = self.store.artifact(id)?;
            if !current.superseded_by.is_empty() {
                return Err(Error::Http(409, "edit the current version of this layer"));
            }
            if current.scope == d::SCOPE_USER && current.subject != actor {
                return Err(Error::Http(403, "layer belongs to another user"));
            }
            let mut parents = List::new();
            parents.push(current.id.try_clone()?)?;
            let request = d::CreateRecordingRequest {
                actor: try_string(actor)?,
                kind: current.kind.try_clone()?,
                name: current.name.try_clone()?,
                scope: current.scope.try_clone()?,
                subject: current.subject.try_clone()?,
                profile: current.profile.try_clone()?,
                provides: current.provides.try_clone()?,
                requires: current.requires.try_clone()?,
                enables: current.enables.try_clone()?,
                slot: current.slot.try_clone()?,
                parent_artifact_ids: parents,
                compatibility_fingerprint: current.compatibility_fingerprint.try_clone()?,
                sensitivity: current.sensitivity.try_clone()?,
                replaces_artifact_id: current.id.try_clone()?,
            };
            let id = runtime.next("rec")?;
            let recording = self
                .store
                .create_recording(request, Context { now, id: &id })?;
            let result = self.start_recording(recording, now, runtime);
            if result.is_err() {
                self.store.cancel_recording(&id, actor, now)?;
            }
            return result.map(|wait| Some(Outcome::Capsule(wait)));
        }
        if req.method == "POST" && req.path == "/api/recordings" {
            self.reserve_call(now)?;
            let mut request: d::CreateRecordingRequest = Self::decode(req)?;
            request.actor = try_string(actor)?;
            let id = runtime.next("rec")?;
            let recording = self
                .store
                .create_recording(request, Context { now, id: &id })?;
            let result = self.start_recording(recording, now, runtime);
            if result.is_err() {
                self.store.cancel_recording(&id, actor, now)?;
            }
            return result.map(|wait| Some(Outcome::Capsule(wait)));
        }
        let mut path = req.path.trim_start_matches('/').split('/');
        let route = (
            path.next(),
            path.next(),
            path.next(),
            path.next(),
            path.next(),
        );
        match route {
            (Some("api"), Some("recordings"), Some(id), Some(kind @ ("start" | "seal")), None)
                if req.method == "GET" =>
            {
                let recording = self.store.recording(id)?;
                if recording.actor != actor {
                    return Err(Error::Http(404, "not found"));
                }
                let call = self
                    .calls
                    .iter()
                    .rev()
                    .find(|c| {
                        c.action.object() == id
                            && matches!(
                                (&c.action, kind),
                                (Action::Start { .. } | Action::Rebase { .. }, "start")
                                    | (Action::Seal { .. }, "seal")
                            )
                    })
                    .ok_or(Error::Http(404, "operation not found"))?;
                Ok(Some(Outcome::Response(self.call_status(call, 200)?)))
            }
            (Some("api"), Some("recordings"), Some(id), Some("parents"), None)
                if req.method == "POST" =>
            {
                self.owner_recording(id, actor)?;
                self.reserve_call(now)?;
                if self.recording_terminal_busy(id)
                    || self
                        .calls
                        .iter()
                        .any(|c| !c.finished && c.action.object() == id)
                {
                    return Err(Error::Http(409, "wait for the recording command to finish"));
                }
                let mut request: d::AttachRecordingParentRequest = Self::decode(req)?;
                request.actor = try_string(actor)?;
                let recording = self.store.prepare_recording_parent(id, &request)?;
                Ok(Some(Outcome::Capsule(
                    self.start_recording(recording, now, runtime)?,
                )))
            }
            (Some("api"), Some("recordings"), Some(id), Some("commands"), None)
                if req.method == "POST" =>
            {
                let recording = self.owner_recording(id, actor)?;
                let request: d::ExecuteRecordingCommandRequest = Self::decode(req)?;
                if request.input.trim().is_empty() {
                    return Err(Error::Http(400, "command is required"));
                }
                let client = recording
                    .runtime
                    .as_ref()
                    .map(|r| r.client_id.try_clone())
                    .transpose()?
                    .ok_or(Error::Http(409, "recording is still starting"))?;
                let wait = self.enqueue_call(
                    Action::Execute {
                        recording: try_string(id)?,
                        actor: try_string(actor)?,
                    },
                    &client,
                    p::METHOD_EXECUTE,
                    &p::ExecutePayload {
                        recording,
                        input: request.input,
                    },
                    now,
                    runtime,
                )?;
                Ok(Some(Outcome::Capsule(wait)))
            }
            (Some("api"), Some("recordings"), Some(id), Some("cancel" | "end"), None)
                if req.method == "POST" =>
            {
                // Beide requesttypen hebben een actor; end mag een legacy snapshot dragen,
                // maar het daadwerkelijke seal-resultaat blijft afkomstig van de runner.
                if !req.body.is_empty() {
                    let _: d::EndRecordingRequest = Self::decode(req)?;
                }
                let recording = self.owner_recording(id, actor)?;
                let seal = req.path.ends_with("/end");
                if !seal && let Some(index) = self.calls.iter().position(|c| {
                    !c.finished
                        && matches!(&c.action, Action::Start { recording, .. } if recording == id)
                }) {
                    let client = self.calls[index].client.try_clone()?;
                    let request = self.calls[index].request_id.try_clone()?;
                    let cancelled = self.store.cancel_recording(id, actor, now)?;
                    self.calls[index].response =
                        Some(Error::Http(409, "recording start cancelled").response()?);
                    self.calls[index].finished = true;
                    self.calls[index].error = true;
                    self.calls[index].waiting = None;
                    self.calls[index].updated = now.try_clone()?;
                    if client.is_empty() {
                        return Ok(Some(Outcome::Response(Response::json(200, &cancelled)?)));
                    }
                    if let Some(peer) = self.runners.iter_mut().find(|p| p.client().id == client) {
                        peer.cancel(&request)?;
                    }
                    let wait = self.enqueue_call(
                        Action::Cancel {
                            recording: try_string(id)?,
                            actor: try_string(actor)?,
                        },
                        &client,
                        p::METHOD_CANCEL_RECORDING,
                        &p::RecordingPayload { recording },
                        now,
                        runtime,
                    )?;
                    return Ok(Some(Outcome::Capsule(wait)));
                }
                if self
                    .calls
                    .iter()
                    .any(|c| !c.finished && c.action.object() == id)
                {
                    return Err(Error::Http(409, "capsule operation already in progress"));
                }
                let Some(client) = recording
                    .runtime
                    .as_ref()
                    .map(|r| r.client_id.try_clone())
                    .transpose()?
                else {
                    if seal {
                        return Err(Error::Http(409, "recording is still starting"));
                    }
                    return Ok(Some(Outcome::Response(Response::json(
                        200,
                        &self.store.cancel_recording(id, actor, now)?,
                    )?)));
                };
                let action = if seal {
                    Action::Seal {
                        snapshot: None,
                        recording: try_string(id)?,
                        actor: try_string(actor)?,
                    }
                } else {
                    Action::Cancel {
                        recording: try_string(id)?,
                        actor: try_string(actor)?,
                    }
                };
                let wait = self.enqueue_call(
                    action,
                    &client,
                    if seal {
                        p::METHOD_SEAL
                    } else {
                        p::METHOD_CANCEL_RECORDING
                    },
                    &p::RecordingPayload { recording },
                    now,
                    runtime,
                )?;
                Ok(Some(Outcome::Capsule(wait)))
            }
            (Some("api"), Some("compositions"), Some(id), Some("stop"), None)
                if req.method == "POST" =>
            {
                let composition = self.store.composition(id)?;
                if composition.operator != actor {
                    return Err(Error::Http(404, "not found"));
                }
                let capsule = composition
                    .runtime
                    .as_ref()
                    .ok_or(Error::Http(409, "composition has no capsule"))?
                    .try_clone()?;
                if capsule.status == "stopped" {
                    return Ok(Some(Outcome::Response(Response::json(200, composition)?)));
                }
                match self.begin_stop(id, "api_session_stop", now, runtime)? {
                    Some(wait) => Ok(Some(Outcome::Capsule(wait))),
                    None => Ok(Some(Outcome::Response(Response::json(
                        202,
                        self.store.composition(id)?,
                    )?))),
                }
            }
            _ => Ok(None),
        }
    }
    fn call_status(&self, call: &Call, code: u16) -> Result<Response> {
        let state = if !call.finished {
            "running"
        } else if call.error {
            "error"
        } else {
            "done"
        };
        let error = if call.error {
            "runner operation failed"
        } else {
            ""
        };
        if let Action::Seal { snapshot, .. } = &call.action {
            let progress = &call.progress;
            let stage = if call.finished {
                state
            } else if !progress.stage.is_empty() {
                &progress.stage
            } else if snapshot.is_some() {
                "archive"
            } else {
                "commit"
            };
            Response::json(
                code,
                &d::SealStatus {
                    recording_id: try_string(call.action.object())?,
                    status: try_string(state)?,
                    stage: try_string(stage)?,
                    message: try_string(if call.finished { "" } else { &progress.message })?,
                    current: if call.finished { 0 } else { progress.current },
                    total: if call.finished { 0 } else { progress.total },
                    error: try_string(error)?,
                    artifact: call.artifact.try_clone()?,
                    started_at: call.started.try_clone()?,
                    updated_at: call.updated.try_clone()?,
                },
            )
        } else {
            Response::json(
                code,
                &d::StartStatus {
                    recording_id: try_string(call.action.object())?,
                    status: try_string(state)?,
                    stage: try_string(if call.finished { state } else { "start" })?,
                    error: try_string(error)?,
                    recording: if call.finished && !call.error {
                        Some(self.store.recording(call.action.object())?.try_clone()?)
                    } else {
                        None
                    },
                    started_at: call.started.try_clone()?,
                    updated_at: call.updated.try_clone()?,
                    ..Default::default()
                },
            )
        }
    }
    /// Bewaart de laatste stand die een runner van een lopend verzoek meldt.
    pub(crate) fn capsule_progress(
        &mut self,
        client: &str,
        message: &WireMessage,
        now: &Timestamp,
    ) -> Result {
        let Some(call) = self
            .calls
            .iter_mut()
            .find(|c| c.request_id == message.id && c.client == client && !c.finished)
        else {
            return Ok(());
        };
        let Ok(progress) =
            d::SealStatus::from_value(message.payload.0.as_ref().unwrap_or(&Value::Null))
        else {
            return Ok(());
        };
        call.progress = progress;
        call.updated = now.try_clone()?;
        Ok(())
    }
    /// De host vraagt alleen of zijn antwoord klaar is; de Store-lening leeft niet over een await.
    pub fn poll_capsule(
        &mut self,
        wait: &CapsuleWait,
        now: &Timestamp,
    ) -> Result<Option<Response>> {
        let index = self
            .calls
            .iter()
            .position(|c| c.id == wait.id)
            .ok_or(Error::Http(404, "operation not found"))?;
        if self.calls[index].finished {
            let wrapped = if let Some(id) = &wait.response_session {
                let response = self.calls[index]
                    .response
                    .as_ref()
                    .ok_or(Error::Http(409, "operation already received"))?;
                let session = self.store.session(id)?.try_clone()?;
                let composition = if response.status < 400 {
                    Some(
                        self.store
                            .composition(&session.prepared_composition_id)?
                            .try_clone()?,
                    )
                } else {
                    None
                };
                Some(Response::json(
                    201,
                    &d::CreateJobSessionResponse {
                        session,
                        composition,
                        run_error: if response.status >= 400 {
                            try_string("capsule preparation failed")?
                        } else {
                            String::new()
                        },
                    },
                )?)
            } else {
                None
            };
            self.calls[index].detached = true;
            let response = self.calls[index]
                .response
                .take()
                .ok_or(Error::Http(409, "operation already received"))?;
            if !self.calls[index].action.persistent() {
                self.calls.remove(index);
            }
            return Ok(Some(wrapped.unwrap_or(response)));
        }
        let call = &self.calls[index];
        if call.action.persistent()
            && now.time()?.0.saturating_sub(call.started.time()?.0) >= 3_000_000_000
        {
            let mut response = self.call_status(call, 202)?;
            let route = if matches!(call.action, Action::Seal { .. }) {
                "seal"
            } else {
                "start"
            };
            response.header(
                "Location",
                &text(format_args!(
                    "/api/recordings/{}/{route}",
                    call.action.object()
                ))?,
            )?;
            self.calls[index].detached = true;
            return Ok(Some(response));
        }
        Ok(None)
    }
    /// Een verdwenen HTTP-caller geeft alleen zijn antwoordplek vrij; starts lopen door.
    pub fn detach_capsule(&mut self, wait: CapsuleWait) {
        if let Some(call) = self.calls.iter_mut().find(|c| c.id == wait.id) {
            call.detached = true;
        }
    }
    pub(crate) fn finish_capsule(
        &mut self,
        client: &str,
        request: Option<&WireMessage>,
        message: &WireMessage,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<bool> {
        let Some(index) = self
            .calls
            .iter()
            .position(|c| c.request_id == message.id && c.client == client && !c.finished)
        else {
            return Ok(false);
        };
        self.calls[index].updated = now.try_clone()?;
        let id = try_string(&self.calls[index].id)?;
        self.display_changed();
        let payload = message.payload.0.as_ref().unwrap_or(&Value::Null);
        if matches!(self.calls[index].action, Action::Rebase { .. }) {
            let result = (|| -> Result {
                if !message.error.is_empty() {
                    return Err(Error::Http(
                        502,
                        "recording parent change awaits container removal",
                    ));
                }
                let Action::Rebase { recording, actor } = &self.calls[index].action else {
                    return Err(Error::Http(500, "missing recording"));
                };
                let mut record = self.store.recording(recording)?.try_clone()?;
                record.runtime = None;
                let mut parents = List::new();
                for id in record.parent_artifact_ids.iter() {
                    parents.push(self.store.artifact(id)?.try_clone()?)?;
                }
                let (layers, artifacts, lifted) = self.store.recording_stack(&record)?;
                let stack = lifted.then_some(d::engine::RecordingStack { layers, artifacts });
                let action = Action::Start {
                    recording: recording.try_clone()?,
                    actor: actor.try_clone()?,
                };
                let payload = p::StartRecordingPayload {
                    recording: record,
                    parents,
                    stack,
                };
                self.store
                    .set_recording_runtime(recording, actor, d::CapsuleRuntime::default())?;
                self.calls[index].action = action;
                self.continue_capsule(index, client, p::METHOD_START_RECORDING, &payload, runtime)
            })();
            if let Err(error) = result {
                self.calls[index].response = Some(error.response()?);
                self.calls[index].error = true;
                self.calls[index].finished = true;
            }
            return Ok(true);
        }
        if matches!(self.calls[index].action, Action::Login(_)) {
            match self.advance_login_capture(index, client, message, now, runtime) {
                Ok(None) => return Ok(true),
                Ok(Some(response)) => self.calls[index].response = Some(response),
                Err(error) => {
                    self.calls[index].response = Some(error.response()?);
                    self.calls[index].error = true;
                }
            }
            self.calls[index].finished = true;
            return Ok(true);
        }
        if matches!(self.calls[index].action, Action::Merge(_)) {
            match self.advance_merge(index, client, message, now, runtime) {
                Ok(None) => return Ok(true),
                Ok(Some(response)) => self.calls[index].response = Some(response),
                Err(error) => {
                    self.fail_merge_reply(index, now, runtime)?;
                    self.calls[index].response = Some(error.response()?);
                    self.calls[index].error = true;
                }
            }
            self.calls[index].finished = true;
            return Ok(true);
        }
        if matches!(self.calls[index].action, Action::App(_)) {
            match self.finish_app(index, message, now, runtime) {
                Ok(response) => self.calls[index].response = Some(response),
                Err(error) => {
                    self.fail_app_reply(index)?;
                    self.calls[index].response = Some(error.response()?);
                    self.calls[index].error = true;
                }
            }
            self.calls[index].finished = true;
            return Ok(true);
        }
        if matches!(self.calls[index].action, Action::Inspect(_)) && message.error.is_empty() {
            match self.advance_inspection(index, client, payload, runtime, now) {
                Ok(None) => return Ok(true),
                Ok(Some(response)) => self.calls[index].response = Some(response),
                Err(error) => {
                    self.calls[index].response = Some(error.response()?);
                    self.calls[index].error = true;
                }
            }
            self.calls[index].finished = true;
            return Ok(true);
        }
        if matches!(self.calls[index].action, Action::Preserve(_)) {
            let outcome = self.advance_preservation(index, client, message, now, runtime);
            let index = self.call_index(&id)?;
            match outcome {
                Ok(None) => return Ok(true),
                Ok(Some(response)) => self.calls[index].response = Some(response),
                Err(error) => {
                    self.calls[index].response = Some(error.response()?);
                    self.calls[index].error = true;
                }
            }
            self.calls[index].finished = true;
            return Ok(true);
        }
        if matches!(self.calls[index].action, Action::Delivery(_)) {
            let response = self.finish_workflow_delivery(index, message, now, runtime)?;
            let index = self.call_index(&id)?;
            self.calls[index].response = Some(response);
            self.calls[index].finished = true;
            return Ok(true);
        }
        if matches!(self.calls[index].action, Action::Workflow(_)) {
            let outcome = self.advance_workflow_accept(index, client, message, now, runtime);
            let index = self.call_index(&id)?;
            match outcome {
                Ok(None) => return Ok(true),
                Ok(Some(response)) => self.calls[index].response = Some(response),
                Err(error) => self.calls[index].response = Some(error.response()?),
            }
            self.calls[index].finished = true;
            self.calls[index].updated = now.try_clone()?;
            return Ok(true);
        }
        if self.calls[index].start.is_some() && message.error.is_empty() {
            let accepts = p::AcceptsReply::from_value(payload)?;
            if !accepts.accepts {
                self.calls[index].waiting = self.calls[index].start.take();
                self.calls[index].refused_by = try_string(client)?;
                self.calls[index].reported_ms = 0;
                if !matches!(&self.calls[index].action, Action::Materialize(work) if work.pinned) {
                    self.calls[index].client.clear();
                }
                if let Some(peer) = self.runners.iter_mut().find(|p| p.client().id == client) {
                    peer.refuse_until(now.time()?.0 / 1_000_000 + 5000);
                }
                return Ok(true);
            }
            let start = self.calls[index]
                .start
                .as_ref()
                .ok_or(Error::Http(500, "missing capsule request"))?
                .try_clone()?;
            let id = start.id.try_clone()?;
            self.runners
                .iter_mut()
                .find(|p| p.client().id == client)
                .ok_or(Error::Http(503, "runner disappeared"))?
                .request(start)?;
            self.calls[index].request_id = id;
            self.calls[index].start = None;
            return Ok(true);
        }
        if matches!(
            self.calls[index].action,
            Action::Start { .. } | Action::Materialize(_)
        ) && p::is_runner_full(&message.error)
            && let Some(request) = request
        {
            // Ook een fout op de capaciteitsvraag bewaart de echte startopdracht.
            let mut retry = self.calls[index]
                .start
                .as_ref()
                .unwrap_or(request)
                .try_clone()?;
            retry.id = runtime.next("req")?;
            self.calls[index].start = None;
            self.calls[index].request_id = retry.id.try_clone()?;
            self.calls[index].waiting = Some(retry);
            self.calls[index].refused_by = try_string(client)?;
            self.calls[index].reported_ms = 0;
            if !matches!(&self.calls[index].action, Action::Materialize(work) if work.pinned) {
                self.calls[index].client.clear();
            }
            if let Some(peer) = self.runners.iter_mut().find(|p| p.client().id == client) {
                peer.refuse_until(now.time()?.0 / 1_000_000 + 5000);
            }
            return Ok(true);
        }
        // De Go-engine behandelt de watcher als best effort: geplaatste logins en
        // een bruikbare capsule blijven bruikbaar als deze optimalisatie faalt.
        if let Action::Materialize(work) = &self.calls[index].action
            && !message.error.is_empty()
            && work.reading()
        {
            crate::note(
                &mut self.notes,
                format_args!(
                    "SPIN_LOGIN_CAPTURE_FAILED composition={} session={} error={}",
                    work.id, work.session, message.error
                ),
            );
        }
        if matches!(&self.calls[index].action, Action::Materialize(work) if message.error.is_empty() || work.watching() || work.reading())
        {
            let outcome = self.advance_materialize(index, client, payload, now, runtime);
            let index = self.call_index(&id)?;
            match outcome {
                Ok(None) => return Ok(true),
                Ok(Some(response)) => {
                    // De opruiming na een mislukte voorbereiding antwoordt met haar reden.
                    self.calls[index].error = response.status >= 400;
                    self.calls[index].response = Some(response);
                    self.calls[index].finished = true;
                    self.calls[index].updated = now.try_clone()?;
                    self.last_launch_sweep = None;
                    return Ok(true);
                }
                Err(error) => {
                    let reason = public_reason(&error)?;
                    if self.fail_materialize(index, &reason, now, runtime)? {
                        return Ok(true);
                    }
                    self.calls[index].response = Some(error.response()?);
                    self.calls[index].finished = true;
                    self.calls[index].error = true;
                    self.calls[index].updated = now.try_clone()?;
                    return Ok(true);
                }
            }
        }
        if matches!(self.calls[index].action, Action::Materialize(_))
            && self.fail_materialize(index, &message.error, now, runtime)?
        {
            return Ok(true);
        }
        if message.error.is_empty()
            && matches!(
                &self.calls[index].action,
                Action::Seal { snapshot: None, .. }
            )
        {
            let mut snapshot = d::CapsuleSnapshot::from_value(payload)?;
            snapshot.client_id = try_string(client)?;
            let id = runtime.next("req")?;
            let request = WireMessage {
                id: id.try_clone()?,
                r#type: try_string(p::MESSAGE_REQUEST)?,
                method: try_string(p::METHOD_ARCHIVE_SNAPSHOT)?,
                payload: RawJson(Some(
                    p::SnapshotPayload {
                        snapshot: snapshot.try_clone()?,
                    }
                    .to_value()?,
                )),
                ..Default::default()
            };
            self.runners
                .iter_mut()
                .find(|p| p.client().id == client)
                .ok_or(Error::Http(503, "runner disappeared"))?
                .request(request)?;
            self.calls[index].request_id = id;
            self.calls[index].progress = d::SealStatus::default();
            if let Action::Seal {
                snapshot: saved, ..
            } = &mut self.calls[index].action
            {
                *saved = Some(snapshot);
            }
            self.calls[index].updated = now.try_clone()?;
            return Ok(true);
        }
        let mut artifact = None;
        let result = (|| -> Result<Response> {
            if !message.error.is_empty() {
                if let Action::Start { recording, actor } = &self.calls[index].action {
                    self.store.cancel_recording(recording, actor, now)?;
                }
                return Err(Error::Http(502, "runner operation failed"));
            }
            match &self.calls[index].action {
                Action::Rebase { .. } => Err(Error::Http(502, "recording rebase failed")),
                Action::Login(_) => Err(Error::Http(502, "login operation failed")),
                Action::Merge(_) => Err(Error::Http(502, "merge operation failed")),
                Action::App(_) => Err(Error::Http(502, "app operation failed")),
                Action::Inspect(_) => Err(Error::Http(502, "workspace inspection failed")),
                Action::Browse(work) => crate::code::browse_response(work, payload),
                Action::ReadFile(work) => crate::management::file_response(work, payload),
                Action::Probe(work) => crate::management::probe_response(work, payload),
                Action::RemoveSnapshot { .. } => Response::empty(204),
                Action::Attachment { key, stamp, .. } => {
                    let response = Response::empty(204)?;
                    self.attachment_stamps
                        .insert(key.try_clone()?, stamp.try_clone()?)?;
                    Ok(response)
                }
                Action::Watch { composition, stamp } => {
                    let response = Response::empty(204)?;
                    self.watch_stamps
                        .insert(composition.try_clone()?, stamp.try_clone()?)?;
                    Ok(response)
                }
                Action::Materialize(_) => Err(Error::Http(502, "capsule preparation failed")),
                Action::WaitLogin { .. } => Err(Error::Http(500, "login wait has no runner call")),
                Action::Workflow(_) => Err(Error::Http(502, "workflow operation failed")),
                Action::Delivery(_) => Err(Error::Http(502, "deliverable operation failed")),
                Action::Preserve(_) => Err(Error::Http(502, "workspace preservation failed")),
                Action::Start { recording, actor } => {
                    let mut capsule = d::CapsuleRuntime::from_value(payload)?;
                    capsule.client_id = try_string(client)?;
                    if capsule.container_id.is_empty() {
                        return Err(Error::Http(502, "runner returned no capsule"));
                    }
                    Response::json(
                        201,
                        &self
                            .store
                            .set_recording_runtime(recording, actor, capsule)?,
                    )
                }
                Action::Execute { recording, actor } => {
                    let execution = d::engine::Execution::from_value(payload)?;
                    Response::json(
                        200,
                        &self.store.record_execution(
                            recording,
                            actor,
                            Some(execution.exit_code),
                            now,
                        )?,
                    )
                }
                Action::Seal {
                    recording,
                    actor,
                    snapshot,
                } => {
                    let snapshot = snapshot
                        .as_ref()
                        .ok_or(Error::Http(502, "missing sealed snapshot"))?
                        .try_clone()?;
                    let archived = p::ArchiveResult::from_value(payload)?;
                    if archived.size <= 0
                        || archived.r#ref != text(format_args!("snapshot:{}", snapshot.digest))?
                    {
                        return Err(Error::Http(502, "runner returned an invalid archive"));
                    }
                    let id = runtime.next("art")?;
                    let saved = self.store.end_recording(
                        recording,
                        d::EndRecordingRequest {
                            actor: actor.try_clone()?,
                            snapshot,
                            ..Default::default()
                        },
                        Context { now, id: &id },
                    )?;
                    let response = Response::json(201, &saved)?;
                    artifact = Some(saved);
                    Ok(response)
                }
                Action::Cancel { recording, actor } => {
                    if self.store.recording(recording)?.status == d::RECORDING_CANCELLED {
                        Response::json(200, self.store.recording(recording)?)
                    } else {
                        Response::json(200, &self.store.cancel_recording(recording, actor, now)?)
                    }
                }
            }
        })();
        let failed = result.is_err();
        let response = match result {
            Ok(response) => response,
            // De runner kent de echte reden; die gaat mee in plaats van een vaste tekst.
            Err(_) if !message.error.is_empty() => Response::json(
                502,
                &http::object(&[("error", Value::string(&message.error)?)])?,
            )?,
            Err(error) => error.response()?,
        };
        self.calls[index].updated = now.try_clone()?;
        self.calls[index].response = Some(response);
        self.calls[index].finished = true;
        self.calls[index].error = failed;
        self.calls[index].artifact = artifact;
        if !failed
            && matches!(
                self.calls[index].action,
                Action::Seal { .. } | Action::Cancel { .. }
            )
            && let Some(peer) = self.runners.iter_mut().find(|p| p.client().id == client)
        {
            peer.freed();
        }
        Ok(true)
    }
}

// Een positieve capaciteitsvraag is geen reservering: een latere "runner full"
// doorloopt opnieuw de vloot met een verse RPC-ID, zodat de runnercache niet
// voor altijd hetzelfde eerdere volle-antwoord teruggeeft.
fn send_to_peer(
    peer: &mut spin_core::runner::Peer,
    call: &mut Call,
    message: WireMessage,
    runtime: &mut impl Runtime,
) -> Result {
    if matches!(call.action, Action::Start { .. } | Action::Materialize(_)) {
        let id = runtime.next("req")?;
        let request_id = id.try_clone()?;
        peer.request(WireMessage {
            r#type: try_string(p::MESSAGE_REQUEST)?,
            id,
            method: try_string(p::METHOD_ACCEPTS)?,
            ..Default::default()
        })?;
        call.request_id = request_id;
        call.start = Some(message);
    } else {
        peer.request(message)?;
    }
    Ok(())
}
/// De publieke reden van een fout, zoals de API hem ook zou tonen.
fn public_reason(error: &Error) -> Result<String> {
    let response = error.response()?;
    let body = Value::from_json(&response.body).ok();
    try_string(
        body.as_ref()
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("error"))
            .and_then(|e| e.as_str())
            .unwrap_or(""),
    )
    .map_err(Into::into)
}
