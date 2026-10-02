//! Appservices blijven eigendom van de runner; de server bewaart alleen de startstatus.
use super::*;
use crate::capsules::{Action, CapsuleWait};
use d::{List, protocol as p, try_string};
use spin_store::Mutation;

pub(crate) struct Start {
    status: &'static str,
    error: String,
    at: Timestamp,
}
pub(crate) struct Work {
    pub(crate) object: String,
    session: String,
    method: &'static str,
    service: String,
    expose: bool,
}
impl<P: Persistence> Server<P> {
    fn app_target(
        &self,
        id: &str,
        actor: &str,
    ) -> Result<(d::Session, d::Composition, d::GitRepository)> {
        let session = self.session_access(id, actor)?;
        let composition = self.store.composition(&session.prepared_composition_id)?;
        if composition.operator != session.operator {
            return Err(Error::Http(404, "not found"));
        }
        let repository = self
            .store
            .snapshot()?
            .git_repositories
            .into_vec()
            .into_iter()
            .find(|r| r.id == session.git_repository_id)
            .ok_or(Error::Http(409, "session has no Git repository"))?;
        Ok((session.try_clone()?, composition.try_clone()?, repository))
    }
    pub(crate) fn start_app(
        &mut self,
        id: &str,
        actor: &str,
        expose: bool,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        let (_, composition, repository) = self.app_target(id, actor)?;
        if self.calls.iter().any(|c| !c.finished && matches!(&c.action, Action::App(w) if w.session == id && w.method == p::METHOD_START_APP)) {
            return Ok(());
        }
        let capsule = composition
            .runtime
            .filter(|c| c.status == "ready" && !c.stop_pending)
            .ok_or(Error::Http(409, "session workspace is not running"))?;
        if repository.services.is_empty() {
            return Err(Error::Http(409, "repository has no app services"));
        }
        let snapshot = self.store.snapshot()?;
        self.app_starts.retain(|session, start| {
            snapshot.sessions.iter().any(|s| s.id == session)
                && (start.status == "running"
                    || now
                        .time()
                        .map_or(0, |t| t.0)
                        .saturating_sub(start.at.time().map_or(0, |t| t.0))
                        < 1_800_000_000_000)
        });
        if self.app_starts.len() >= 1024 && !self.app_starts.contains_key(id) {
            return Err(Error::Http(503, "app start capacity reached"));
        }
        let work = Work {
            object: composition.id,
            session: try_string(id)?,
            method: p::METHOD_START_APP,
            service: String::new(),
            expose,
        };
        let started = Start {
            status: "running",
            error: String::new(),
            at: now.try_clone()?,
        };
        // De startstatus is gereserveerd vóór een runner een mutatie ontvangt.
        self.app_starts.insert(try_string(id)?, started)?;
        let client = capsule.client_id.try_clone()?;
        let result = self.enqueue_call(
            Action::App(work),
            &client,
            p::METHOD_START_APP,
            &p::AppPayload {
                runtime: capsule,
                session_id: try_string(id)?,
                services: repository.services,
                hosts: repository.service_hosts,
                ..Default::default()
            },
            now,
            random,
        );
        match result {
            Ok(wait) => {
                self.detach_capsule(wait);
                Ok(())
            }
            Err(error) => {
                self.app_starts.remove(id);
                Err(error)
            }
        }
    }
    pub(crate) fn launch_expose(
        &mut self,
        id: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        let view = self.store.workflow_for_session(id)?;
        if !matches!(
            view.run.status.as_str(),
            d::PHASE_RUN_QUEUED | d::PHASE_RUN_RUNNING
        ) || view.job.current_phase_run_id != view.run.id
        {
            return Ok(());
        }
        let actor = self.store.session(id)?.operator.try_clone()?;
        self.store.mark_workflow_phase_running(id, now)?;
        if let Err(error) = self.start_app(id, &actor, true, now, random) {
            self.store.requeue_workflow_phase(id)?;
            return Err(error);
        }
        Ok(())
    }
    fn app_status(
        &self,
        id: &str,
        services: &List<d::AppService>,
        running: &List<d::AppServiceRuntime>,
        error: &str,
    ) -> Result<Response> {
        let start = self.app_starts.get(id);
        Response::json(
            200,
            &http::object(&[
                ("session_id", Value::string(id)?),
                ("services", services.to_value()?),
                ("running", running.to_value()?),
                ("start", Value::string(start.map_or("", |s| s.status))?),
                (
                    "error",
                    Value::string(if error.is_empty() {
                        start.map_or("", |s| s.error.as_str())
                    } else {
                        error
                    })?,
                ),
                (
                    "started_at",
                    start.map_or(Ok(Value::Null), |s| s.at.to_value())?,
                ),
            ])?,
        )
    }
    pub(crate) fn app_route(
        &mut self,
        req: &Request<'_>,
        actor: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        let mut path = req.path.trim_start_matches('/').split('/');
        let (Some("api"), Some("sessions"), Some(id), Some("app")) =
            (path.next(), path.next(), path.next(), path.next())
        else {
            return Ok(None);
        };
        let kind = path.next();
        let tail = path.next();
        if path.next().is_some() {
            return Ok(None);
        }
        let method = match (req.method, kind, tail) {
            ("POST", Some("start"), None) => p::METHOD_START_APP,
            ("POST", Some("stop"), None) => p::METHOD_STOP_APP,
            ("GET", None, None) => p::METHOD_APP_STATUS,
            ("GET", Some(_), Some("logs")) => p::METHOD_APP_LOGS,
            _ => return Ok(None),
        };
        if method == p::METHOD_START_APP {
            self.start_app(id, actor, false, now, random)?;
            return Ok(Some(Outcome::Response(Response::json(
                202,
                &http::object(&[
                    ("status", Value::string("running")?),
                    ("error", Value::string("")?),
                ])?,
            )?)));
        }
        let (_, composition, repository) = self.app_target(id, actor)?;
        let Some(capsule) = composition
            .runtime
            .filter(|r| r.status == "ready" && !r.stop_pending)
        else {
            if method == p::METHOD_APP_STATUS {
                return Ok(Some(Outcome::Response(self.app_status(
                    id,
                    &repository.services,
                    &List::new(),
                    "",
                )?)));
            }
            if method == p::METHOD_STOP_APP {
                self.app_starts.remove(id);
                return Ok(Some(Outcome::Response(stopped()?)));
            }
            return Err(Error::Http(409, "session has no runner"));
        };
        let service = if method == p::METHOD_APP_LOGS {
            kind.unwrap_or("")
        } else {
            ""
        };
        let client = capsule.client_id.try_clone()?;
        let work = Work {
            object: if method == p::METHOD_STOP_APP {
                composition.id
            } else {
                spin_core::validation::text(format_args!("app:{method}:{id}:{service}"))?
            },
            session: try_string(id)?,
            method,
            service: try_string(service)?,
            expose: false,
        };
        let wait: CapsuleWait = self.enqueue_call(
            Action::App(work),
            &client,
            method,
            &p::AppPayload {
                runtime: capsule,
                session_id: try_string(id)?,
                service: try_string(service)?,
                tail: req.query("tail")?.parse().ok().unwrap_or(200),
                ..Default::default()
            },
            now,
            random,
        )?;
        Ok(Some(Outcome::Capsule(wait)))
    }
    pub(crate) fn fail_app_reply(&mut self, index: usize) -> Result {
        let Action::App(work) = &self.calls[index].action else {
            return Ok(());
        };
        if work.method != p::METHOD_START_APP {
            return Ok(());
        }
        if let Some(start) = self
            .app_starts
            .get_mut(&work.session)
            .filter(|s| s.status == "running")
        {
            start.error = try_string("runner app response could not be processed")?;
            start.status = "error";
        }
        if work.expose
            && self
                .store
                .workflow_for_session(&work.session)
                .is_ok_and(|v| {
                    v.run.status == d::PHASE_RUN_RUNNING && v.job.current_phase_run_id == v.run.id
                })
        {
            self.store.requeue_workflow_phase(&work.session)?;
        }
        Ok(())
    }
    pub(crate) fn finish_app(
        &mut self,
        index: usize,
        message: &p::WireMessage,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Response> {
        let Action::App(work) = &self.calls[index].action else {
            return Err(Error::Http(500, "missing app operation"));
        };
        let id = work.session.try_clone()?;
        let method = work.method;
        let expose = work.expose;
        let payload = message.payload.0.as_ref().unwrap_or(&Value::Null);
        if method == p::METHOD_APP_LOGS {
            if !message.error.is_empty() {
                return Err(Error::Http(502, "app logs unavailable"));
            }
            let logs = p::AppLogsResult::from_value(payload)?;
            return Response::json(
                200,
                &http::object(&[
                    ("service", Value::string(&work.service)?),
                    ("output", Value::string(&logs.output)?),
                ])?,
            );
        }
        if method == p::METHOD_STOP_APP {
            if !message.error.is_empty() {
                return Err(Error::Http(502, "app stop failed"));
            }
            self.app_starts.remove(&id);
            return stopped();
        }
        let mut error = if message.error.is_empty() {
            String::new()
        } else {
            try_string("runner app operation failed")?
        };
        let running = if error.is_empty() {
            p::AppStatusResult::from_value(payload)?.services
        } else {
            List::new()
        };
        if method == p::METHOD_START_APP {
            for service in running.iter().filter(|s| !s.error.is_empty()) {
                if error.len() >= 8192 {
                    break;
                }
                d::try_push_str(
                    &mut error,
                    &spin_core::validation::text(format_args!(
                        "{}: {}; ",
                        service.service, service.error
                    ))?,
                )?;
            }
            if let Some(start) = self.app_starts.get_mut(&id) {
                start.status = if error.is_empty() { "done" } else { "error" };
                start.error = error.try_clone()?;
            }
            if expose {
                if error.is_empty() {
                    let mut detail = String::new();
                    for service in running.iter() {
                        for (port, host) in service.ports.iter() {
                            d::try_push_str(
                                &mut detail,
                                &spin_core::validation::text(format_args!(
                                    "{}: http://{}:{} (poort {}) · ",
                                    service.service, service.host, host, port
                                ))?,
                            )?;
                        }
                    }
                    d::try_push_str(
                        &mut detail,
                        "Test de app en kies ACCEPT of REJECT met een reden.",
                    )?;
                    let advance = self.store.complete_workflow_phase(
                        &id,
                        "accept",
                        &detail,
                        true,
                        Mutation { now, ids: random },
                    )?;
                    if let Some(next) = advance.next_session {
                        self.schedule_session(&next, now, random)?;
                    }
                } else {
                    self.store.requeue_workflow_phase(&id)?;
                }
            }
        }
        let session = self.store.session(&id)?;
        let (_, _, repository) = self.app_target(&id, &session.operator)?;
        self.app_status(&id, &repository.services, &running, &error)
    }
}
fn stopped() -> Result<Response> {
    Response::json(
        200,
        &http::object(&[("status", Value::string("stopped")?)])?,
    )
}
