//! Materialiseren, logins plaatsen en wijzigingen volgen vormen één uitvoerketen.
use super::*;
use crate::capsules::{Action, CapsuleWait};
use alloc::vec::Vec;
use d::{List, RawJson, WireMap, protocol as p, try_push, try_string};
use spin_core::validation::text;
use spin_store::Context;

pub(crate) struct Placement {
    pub(crate) id: String,
    pub(crate) session: String,
    pub(crate) pinned: bool,
    actor: String,
    runtime: Option<d::CapsuleRuntime>,
    targets: Vec<Target>,
    content: Vec<Content>,
    content_at: usize,
    at: usize,
    phase: Phase,
}
// De begrensde voorbereidingslijst bezit haar metadata zonder extra heapobject per item.
#[allow(clippy::large_enum_variant)]
enum Content {
    Attachment(d::JobAttachment),
    Delivery(d::Deliverable, String),
}
pub(crate) struct Target {
    pub(crate) key: String,
    pub(crate) paths: List<String>,
    pub(crate) excludes: List<String>,
    pub(crate) exclusive: bool,
}
#[derive(PartialEq)]
enum Phase {
    Build,
    Read,
    Write,
    Watch,
    Content,
    Cleanup,
}
impl Placement {
    pub(crate) fn watching(&self) -> bool {
        self.phase == Phase::Watch
    }
}
impl<P: Persistence> Server<P> {
    pub(crate) fn schedule_session(
        &mut self,
        session: &d::Session,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        if self.job_operation_pending(&session.job_id) {
            return Ok(());
        }
        if matches!(
            session.status.as_str(),
            d::SESSION_COMPLETED | d::SESSION_CANCELLED | d::SESSION_RUNNING | d::SESSION_CLAIMED
        ) {
            return Ok(());
        }
        // Historical attempts can outlive their Job or template. Only the Job's
        // current attempt may launch, as in the Go scheduler. Map lookups only:
        // the 30 s sweep passes every queued session of every old Job here.
        if !session.phase_run_id.is_empty() {
            if !self.store.is_current_phase_session(session) {
                return Ok(());
            }
            // Alleen voor de huidige poging de volledige weergave: een kapotte
            // poging (sjabloon weg) blijft zo een gemelde fout, geen stille stop.
            let view = self.store.workflow_for_session(&session.id)?;
            if !matches!(
                view.run.status.as_str(),
                d::PHASE_RUN_QUEUED | d::PHASE_RUN_RUNNING
            ) || view.job.current_phase_run_id != view.run.id
            {
                return Ok(());
            }
        }
        if session.executor == d::WORKFLOW_EXECUTOR_ACTION {
            return self.launch_workflow_action(session, now, random);
        }
        if self.calls.iter().any(|call| {
            !call.finished
                && matches!(&call.action, Action::Materialize(work) if work.session == session.id)
        }) {
            return Ok(());
        }
        if !session.prepared_composition_id.is_empty()
            && self
                .store
                .composition(&session.prepared_composition_id)
                .is_ok_and(|c| c.runtime.as_ref().is_some_and(|r| r.status != "stopped"))
        {
            if self
                .store
                .composition(&session.prepared_composition_id)?
                .runtime
                .as_ref()
                .is_some_and(|r| r.stop_pending)
            {
                return Ok(());
            }
            if !session.phase_run_id.is_empty() {
                if session.executor == d::WORKFLOW_EXECUTOR_EXPOSE {
                    self.launch_expose(&session.id, now, random)?;
                } else {
                    self.launch_workflow_agent(&session.id, now, random)?;
                }
            }
            return Ok(());
        }
        match self.begin_materialize(
            d::UseRequest {
                session_id: session.id.try_clone()?,
                operator: session.operator.try_clone()?,
                ..Default::default()
            },
            now,
            random,
        ) {
            Ok(wait) => self.detach_capsule(wait),
            // Geen vrije login is geen storing: `prepare_materialize` heeft de
            // wachtreden genoteerd en de volgende veegronde probeert het opnieuw.
            Err(Error::Store(spin_store::Error::LoginsBusy)) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }
    pub(crate) fn preparation_state(&self) -> Result<Value> {
        let mut items = List::new();
        for (index, call) in self.calls.iter().enumerate() {
            if let Action::WaitLogin { session, layer } = &call.action {
                if self.calls.iter().any(|other| matches!(&other.action, Action::Materialize(work) if !other.finished && work.session == *session)) {
                    continue;
                }
                items.push(http::object(&[
                    ("session_id", Value::string(session)?),
                    ("client_id", Value::string("")?),
                    ("started_at", call.started.to_value()?),
                    (
                        "progress",
                        http::object(&[
                            ("stage", Value::string("login")?),
                            ("message", Value::string("Wacht op een login")?),
                            ("updated_at", call.updated.to_value()?),
                        ])?,
                    ),
                    (
                        "waiting",
                        http::object(&[
                            (
                                "reason",
                                Value::string(&text(format_args!(
                                    "waiting for a login of {layer}"
                                ))?)?,
                            ),
                            ("layer", Value::string(layer)?),
                        ])?,
                    ),
                ])?)?;
                continue;
            }
            let Action::Materialize(work) = &call.action else {
                continue;
            };
            if (call.finished && !call.error) || work.session.is_empty()
                || self.calls.iter().skip(index + 1).any(|newer| matches!(&newer.action, Action::Materialize(other) if other.session == work.session)) {
                continue;
            }
            let status = if call.error {
                http::object(&[
                    ("error", Value::string("Werkomgeving voorbereiden mislukt")?),
                    ("at", call.updated.to_value()?),
                ])?
            } else {
                http::object(&[
                    ("stage", Value::string("prepare")?),
                    ("message", Value::string("Werkomgeving voorbereiden")?),
                    ("updated_at", call.updated.to_value()?),
                ])?
            };
            let mut fields = Vec::new();
            try_push(&mut fields, ("session_id", Value::string(&work.session)?))?;
            try_push(&mut fields, ("client_id", Value::string(&call.client)?))?;
            try_push(&mut fields, ("started_at", call.started.to_value()?))?;
            try_push(
                &mut fields,
                (if call.error { "failure" } else { "progress" }, status),
            )?;
            if call.queued() {
                // De opdracht ligt nog niet bij een runner; de UI toont waarom.
                try_push(
                    &mut fields,
                    (
                        "waiting",
                        http::object(&[
                            ("reason", Value::string(self.wait_reason(call))?),
                            ("client_id", Value::string(&call.client)?),
                        ])?,
                    ),
                )?;
            }
            items.push(http::object(&fields)?)?;
        }
        Ok(items.to_value()?)
    }
    fn placement_content(&self, composition: &d::Composition) -> Result<Vec<Content>> {
        let mut content = Vec::new();
        if composition.session_id.is_empty() || composition.for_login {
            return Ok(content);
        }
        let session = self.store.session(&composition.session_id)?;
        if session.job_id.is_empty() {
            return Ok(content);
        }
        let job = self.store.job(&session.job_id)?;
        let snapshot = self.store.snapshot()?;
        for (id, directory) in [
            (job.id.as_str(), d::DELIVERABLE_DIRECTORY),
            (
                job.forked_from_job_id.as_str(),
                d::PREVIOUS_JOB_DELIVERABLE_DIRECTORY,
            ),
        ] {
            if id.is_empty() {
                continue;
            }
            for attachment in self.store.job_attachments(id)?.into_vec() {
                try_push(&mut content, Content::Attachment(attachment))?;
            }
            for delivery in snapshot.deliverables.iter().filter(|d| d.job_id == id) {
                let name = spin_core::validation::normalized(&delivery.name)?;
                if snapshot.deliverables.iter().any(|other| {
                    other.job_id == id
                        && other.revision > delivery.revision
                        && other.name.eq_ignore_ascii_case(&name)
                }) {
                    continue;
                }
                if content.len() >= 1024 {
                    return Err(Error::Http(413, "too many capsule context files"));
                }
                try_push(
                    &mut content,
                    Content::Delivery(delivery.try_clone()?, try_string(directory)?),
                )?;
            }
        }
        Ok(content)
    }
    pub(crate) fn tracked_targets(&self, composition: &d::Composition) -> Result<Vec<Target>> {
        let mut targets: Vec<Target> = Vec::new();
        for id in composition.layers.iter() {
            let layer = self.store.artifact(id)?;
            if layer.tracked_paths.is_empty() {
                continue;
            }
            let target = Target {
                key: spin_store::layer_key(layer)?,
                paths: layer.tracked_paths.try_clone()?,
                excludes: layer.tracked_excludes.try_clone()?,
                exclusive: layer.kind == d::ARTIFACT_CREDENTIAL,
            };
            if let Some(old) = targets.iter_mut().find(|t| t.key == target.key) {
                *old = target;
            } else {
                try_push(&mut targets, target)?;
            }
        }
        Ok(targets)
    }
    /// Reconcile selections after edits, reconnects and server restarts; ACK is the publication point.
    pub(crate) fn maintain_watches(
        &mut self,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        let compositions = self.store.running_compositions()?;
        self.watch_stamps
            .retain(|id, _| compositions.iter().any(|c| c.id == id));
        for composition in compositions.iter() {
            let Some(capsule) = composition
                .runtime
                .as_ref()
                .filter(|r| r.status == "ready" && !r.stop_pending)
            else {
                continue;
            };
            if !self
                .runners
                .iter()
                .any(|p| p.client().id == capsule.client_id && p.is_connected())
                || self
                    .calls
                    .iter()
                    .any(|c| !c.finished && c.action.object() == composition.id)
            {
                continue;
            }
            let mut payload = p::TrackedFilesPayload {
                runtime: capsule.try_clone()?,
                ..Default::default()
            };
            for target in self.tracked_targets(composition)? {
                for path in target.paths.into_vec() {
                    if !payload.paths.contains(&path) {
                        payload.paths.push(path)?;
                    }
                }
                for path in target.excludes.into_vec() {
                    if !payload.excludes.contains(&path) {
                        payload.excludes.push(path)?;
                    }
                }
            }
            if payload.paths.is_empty() && self.watch_stamps.get(&composition.id).is_none() {
                continue;
            }
            let stamp = spin_security::digest_hex(payload.to_json()?.as_bytes())?;
            if self.watch_stamps.get(&composition.id) == Some(&stamp) {
                continue;
            }
            if self.watch_stamps.len() >= 1024 && self.watch_stamps.get(&composition.id).is_none() {
                return Err(Error::Http(503, "tracked capsule capacity reached"));
            }
            let wait = self.enqueue_call(
                Action::Watch {
                    composition: composition.id.try_clone()?,
                    stamp,
                },
                &capsule.client_id,
                p::METHOD_WATCH_TRACKED,
                &payload,
                now,
                random,
            )?;
            self.detach_capsule(wait);
            break;
        }
        Ok(())
    }
    pub(crate) fn begin_materialize(
        &mut self,
        request: d::UseRequest,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<CapsuleWait> {
        self.begin_materialize_probe(request, "", now, random)
    }
    pub(crate) fn begin_materialize_probe(
        &mut self,
        request: d::UseRequest,
        probe: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<CapsuleWait> {
        self.reserve_call(now)?;
        let id = random.next("cmp")?;
        let composition = self
            .store
            .use_environment(request, Context { now, id: &id })?;
        let result = (|| {
            // Persist cleanup identity before any command can create a remote capsule.
            if !probe.is_empty() {
                self.store.mark_options_probe(&composition.id, probe)?;
                // De gemarkeerde versie: prepare weet zo dat dit een probe is.
                let marked = self.store.composition(&composition.id)?.try_clone()?;
                return self.prepare_materialize(&marked, now, random);
            }
            self.prepare_materialize(&composition, now, random)
        })();
        if result.is_err() {
            self.store
                .discard_composition(&composition.id, &composition.operator, now)?;
        }
        result
    }
    fn prepare_materialize(
        &mut self,
        composition: &d::Composition,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<CapsuleWait> {
        let targets = self.tracked_targets(composition)?;
        if !composition.for_login {
            for target in &targets {
                match self
                    .store
                    .hand_out_login(&composition.id, &target.key, target.exclusive, now)
                {
                    // De opties van een credential-laag komen alleen met een
                    // login; anders bewaart de probe de opties van een
                    // niet-ingelogde agent.
                    Err(spin_store::Error::NotFound)
                        if target.exclusive && !composition.probe_artifact_id.is_empty() =>
                    {
                        return Err(Error::Http(
                            409,
                            "no login for this credential layer; capture a login first",
                        ));
                    }
                    Ok(_) | Err(spin_store::Error::NotFound) => {}
                    Err(spin_store::Error::LoginsBusy) => {
                        self.note_login_wait(&composition.session_id, &target.key, now, random)?;
                        return Err(spin_store::Error::LoginsBusy.into());
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            self.forget_login_wait(&composition.session_id);
        }
        let authentication = match composition
            .git
            .as_ref()
            .filter(|g| g.credential_scope != d::CREDENTIAL_SCOPE_PUBLIC)
        {
            Some(workspace) => {
                let account = self
                    .store
                    .resolve_git_workspace_account(workspace, &composition.operator)?;
                Some(d::engine::GitAuthentication {
                    username: if account.provider == "gitlab" {
                        try_string("oauth2")?
                    } else {
                        account.login.try_clone()?
                    },
                    password: account.access_token.try_clone()?,
                    author_name: account.name.try_clone()?,
                    author_email: account.email.try_clone()?,
                })
            }
            None => None,
        };
        let mut client = if composition.session_id.is_empty() {
            String::new()
        } else {
            self.store
                .session(&composition.session_id)?
                .client_id
                .try_clone()?
        };
        let pinned = !client.is_empty();
        if client.is_empty() {
            client = match self.choose_runner(now) {
                Ok(client) => client,
                Err(Error::Http(503, _)) => String::new(),
                Err(error) => return Err(error),
            };
        }
        let payload = p::MaterializePayload {
            composition: self.store.composition(&composition.id)?.try_clone()?,
            artifacts: self.store.snapshot()?.artifacts,
            authentication,
        };
        let action = Action::Materialize(Placement {
            id: composition.id.try_clone()?,
            session: composition.session_id.try_clone()?,
            pinned,
            actor: composition.operator.try_clone()?,
            runtime: None,
            content: self.placement_content(composition)?,
            content_at: 0,
            targets,
            at: 0,
            phase: Phase::Build,
        });
        self.enqueue_call(
            action,
            &client,
            p::METHOD_MATERIALIZE,
            &payload,
            now,
            random,
        )
    }
    pub(crate) fn advance_materialize(
        &mut self,
        index: usize,
        client: &str,
        payload: &Value,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let Action::Materialize(work) = &mut self.calls[index].action else {
            return Err(Error::Http(500, "invalid materialization task"));
        };
        match work.phase {
            Phase::Build => {
                let mut capsule = d::CapsuleRuntime::from_value(payload)?;
                capsule.client_id = try_string(client)?;
                if capsule.container_id.is_empty() {
                    return Err(Error::Http(502, "runner returned no capsule"));
                }
                // Bewaar de teruggekomen handle vóór de faalbare database-write:
                // ook bij een mislukte commit moet de eigenaar deze capsule opruimen.
                work.runtime = Some(capsule.try_clone()?);
                self.store
                    .finish_composition(&work.id, &work.actor, capsule, now)?;
            }
            Phase::Read => {
                let files = WireMap::<d::Bytes>::from_value(payload)?;
                if !files.is_empty() {
                    let target = work
                        .targets
                        .get(work.at)
                        .ok_or(Error::Http(500, "missing login target"))?;
                    let id = random.next("lgn")?;
                    self.store.create_login(
                        &work.id,
                        &target.key,
                        &files,
                        "",
                        Context { now, id: &id },
                    )?;
                }
                work.at += 1;
            }
            Phase::Write => work.at += 1,
            Phase::Content => work.content_at += 1,
            Phase::Watch => {
                return Ok(Some(Response::json(
                    201,
                    self.store.composition(&work.id)?,
                )?));
            }
            Phase::Cleanup => {
                if self.store.composition(&work.id)?.runtime.is_some() {
                    let mut capsule = work
                        .runtime
                        .as_ref()
                        .ok_or(Error::Http(500, "missing cleanup handle"))?
                        .try_clone()?;
                    capsule.status = try_string("stopped")?;
                    capsule.stop_pending = false;
                    self.store
                        .set_composition_runtime(&work.id, &work.actor, capsule)?;
                } else {
                    self.store.discard_composition(&work.id, &work.actor, now)?;
                }
                return Err(Error::Http(500, "capsule preparation failed"));
            }
        }
        let composition = self.store.composition(&work.id)?;
        if composition.for_login {
            return Ok(Some(Response::json(201, composition)?));
        }
        let capsule = work
            .runtime
            .as_ref()
            .ok_or(Error::Http(500, "missing capsule handle"))?
            .try_clone()?;
        if let Some(target) = work.targets.get(work.at) {
            let login = self
                .store
                .hand_out_login(&work.id, &target.key, target.exclusive, now);
            let (method, request) = match login {
                Ok(login) => {
                    work.phase = Phase::Write;
                    (
                        p::METHOD_WRITE_TRACKED,
                        p::TrackedFilesPayload {
                            runtime: capsule,
                            files: login.files,
                            ..Default::default()
                        },
                    )
                }
                Err(spin_store::Error::NotFound) => {
                    work.phase = Phase::Read;
                    (
                        p::METHOD_READ_TRACKED,
                        p::TrackedFilesPayload {
                            runtime: capsule,
                            paths: target.paths.try_clone()?,
                            excludes: target.excludes.try_clone()?,
                            ..Default::default()
                        },
                    )
                }
                Err(error) => return Err(error.into()),
            };
            self.continue_capsule(index, client, method, &request, random)?;
            return Ok(None);
        }
        if let Some(content) = work.content.get(work.content_at) {
            let (method, payload) = match content {
                Content::Attachment(attachment) => {
                    let reference =
                        spin_core::validation::text(format_args!("attachment:{}", attachment.id))?;
                    let bytes = self.store.read_blob(&reference, 15 << 20)?;
                    let mut attachments = List::new();
                    attachments.push(p::AttachmentPayload {
                        target_path: attachment.capsule_path.try_clone()?,
                        data: d::Bytes(Some(bytes)),
                    })?;
                    (
                        p::METHOD_INJECT_ATTACHMENTS,
                        p::InjectAttachmentsPayload {
                            runtime: capsule,
                            attachments,
                        }
                        .to_value()?,
                    )
                }
                Content::Delivery(delivery, directory) => {
                    let target = delivery.capsule_path_in(directory)?;
                    if let Some(bundle) = &delivery.bundle {
                        (
                            p::METHOD_PLACE_DELIVERABLE,
                            p::PlaceDeliverablePayload {
                                runtime: capsule,
                                target,
                                bundle: bundle.try_clone()?,
                            }
                            .to_value()?,
                        )
                    } else {
                        let mut files = WireMap::new();
                        files.insert(
                            target,
                            d::Bytes(Some(delivery.content.try_clone()?.into_bytes())),
                        )?;
                        (
                            p::METHOD_WRITE_TRACKED,
                            p::TrackedFilesPayload {
                                runtime: capsule,
                                files,
                                ..Default::default()
                            }
                            .to_value()?,
                        )
                    }
                }
            };
            work.phase = Phase::Content;
            self.continue_capsule(index, client, method, &payload, random)?;
            return Ok(None);
        }
        let mut paths = List::new();
        let mut excludes = List::new();
        for target in &work.targets {
            for path in target.paths.iter() {
                if !paths.contains(path) {
                    paths.push(path.try_clone()?)?;
                }
            }
            for path in target.excludes.iter() {
                if !excludes.contains(path) {
                    excludes.push(path.try_clone()?)?;
                }
            }
        }
        if paths.is_empty() {
            return Ok(Some(Response::json(
                201,
                self.store.composition(&work.id)?,
            )?));
        }
        work.phase = Phase::Watch;
        self.continue_capsule(
            index,
            client,
            p::METHOD_WATCH_TRACKED,
            &p::TrackedFilesPayload {
                runtime: capsule,
                paths,
                excludes,
                ..Default::default()
            },
            random,
        )?;
        Ok(None)
    }
    pub(crate) fn continue_capsule(
        &mut self,
        index: usize,
        client: &str,
        method: &str,
        payload: &impl Wire,
        random: &mut impl Runtime,
    ) -> Result {
        let id = random.next("req")?;
        let request_id = id.try_clone()?;
        self.runners
            .iter_mut()
            .find(|p| p.client().id == client)
            .ok_or(Error::Http(503, "runner disappeared"))?
            .request(p::WireMessage {
                id,
                r#type: try_string(p::MESSAGE_REQUEST)?,
                method: try_string(method)?,
                payload: RawJson(Some(payload.to_value()?)),
                ..Default::default()
            })?;
        self.calls[index].request_id = request_id;
        Ok(())
    }
    /// Opruimen wordt zelf een bevestigde runneropdracht. Logins blijven gereserveerd
    /// zolang de capsule niet aantoonbaar gestopt is.
    pub(crate) fn fail_materialize(
        &mut self,
        index: usize,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<bool> {
        let Action::Materialize(work) = &mut self.calls[index].action else {
            return Ok(false);
        };
        if work.phase == Phase::Cleanup {
            return Ok(false);
        }
        let Some(mut capsule) = work.runtime.as_ref().map(TryClone::try_clone).transpose()? else {
            self.store.discard_composition(&work.id, &work.actor, now)?;
            return Ok(false);
        };
        capsule.stop_pending = true;
        let client = capsule.client_id.try_clone()?;
        // Een onzekere opslag mag de netwerk-opruiming niet tegenhouden.
        let persisted =
            self.store
                .set_composition_runtime(&work.id, &work.actor, capsule.try_clone()?);
        work.phase = Phase::Cleanup;
        self.continue_capsule(
            index,
            &client,
            p::METHOD_STOP,
            &p::RuntimePayload { runtime: capsule },
            random,
        )?;
        // Geen vrijgave op deze writefout: de Stop-bevestiging volgt nog.
        let _ = persisted;
        Ok(true)
    }
    pub(crate) fn tracked_watcher_stopped(&mut self, client: &str, payload: &Value) -> Result {
        let report = p::TrackedFilesPayload::from_value(payload)?;
        for composition in self.store.running_compositions()?.iter() {
            if composition.runtime.as_ref().is_some_and(|r| {
                r.client_id == client && r.container_id == report.runtime.container_id
            }) {
                self.watch_stamps.remove(&composition.id);
            }
        }
        Ok(())
    }
    pub(crate) fn tracked_changed(
        &mut self,
        client: &str,
        payload: &Value,
        now: &Timestamp,
    ) -> Result {
        let report = p::TrackedFilesPayload::from_value(payload)?;
        for composition in self.store.running_compositions()?.iter() {
            let Some(capsule) = &composition.runtime else {
                continue;
            };
            if capsule.status == "installing_login"
                || capsule.client_id != client
                || capsule.container_id != report.runtime.container_id
            {
                continue;
            }
            for target in self.tracked_targets(composition)? {
                let Some(login) = composition.logins.get(&target.key) else {
                    continue;
                };
                let mut folders = List::new();
                for path in target.paths.iter() {
                    if d::tracked_folder(path) && report.paths.contains(path) {
                        folders.push(path.try_clone()?)?;
                    }
                }
                let mut covered = WireMap::new();
                for (path, bytes) in report.files.iter() {
                    if d::tracked_covers(path, target.paths.as_slice(), target.excludes.as_slice())
                    {
                        covered.insert(try_string(path)?, bytes.try_clone()?)?;
                    }
                }
                self.store
                    .save_login_files(login, &covered, folders.as_slice(), now)?;
            }
            break;
        }
        Ok(())
    }
}
