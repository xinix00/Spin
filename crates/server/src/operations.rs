//! Job-beheer wacht op capsulebevestigingen zonder een HTTP-taak of login vast te lenen.
use super::*;
mod artifacts;
use d::{List, try_string};

/// Verwijzing naar een lopende beheeropdracht; de opdracht overleeft de browserverbinding.
pub struct OperationWait {
    pub(crate) id: String,
}
pub(crate) struct Operation {
    id: String,
    job: String,
    actor: String,
    delete: bool,
    artifact: Option<artifacts::Deletion>,
    restart: Option<Restart>,
    session: String,
    preserved: bool,
    preserving: Option<CapsuleWait>,
    expires: u64,
    response: Option<Response>,
}
enum Restart {
    Retry {
        note: String,
        transcript: List<d::ChatLine>,
    },
    Adopt {
        phase: String,
    },
}
impl Operation {
    pub(crate) fn finished(&self) -> bool {
        self.response.is_some()
    }
}
impl<P: Persistence> Server<P> {
    pub(crate) fn job_operation_pending(&self, job: &str) -> bool {
        self.operations
            .iter()
            .any(|op| op.job == job && op.response.is_none())
    }
    pub(crate) fn operation_route(
        &mut self,
        req: &Request<'_>,
        actor: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        if let Some(outcome) = self.restart_operation_route(req, actor, now, random)? {
            return Ok(Some(outcome));
        }
        let Some(tail) = req.path.strip_prefix("/api/jobs/") else {
            return Ok(None);
        };
        let (job, delete) = match (req.method, tail.strip_suffix("/close")) {
            ("POST", Some(id)) => (id, false),
            ("DELETE", None) => (tail, true),
            _ => return Ok(None),
        };
        if job.is_empty() || job.contains('/') {
            return Ok(None);
        }
        self.store.prepare_job_deletion(job, actor)?;
        if self.job_operation_pending(job) {
            return Err(Error::Http(409, "Job operation already in progress"));
        }
        self.operations
            .retain(|op| op.response.is_none() || op.expires > now.time().map_or(0, |t| t.0));
        if self.operations.len() >= 32 {
            return Err(Error::Http(503, "Job operation capacity reached"));
        }
        self.operations
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        let id = random.next("op")?;
        let wait = OperationWait {
            id: id.try_clone()?,
        };
        self.operations.push(Operation {
            id,
            job: try_string(job)?,
            actor: try_string(actor)?,
            delete,
            artifact: None,
            restart: None,
            session: String::new(),
            preserved: true,
            preserving: None,
            expires: now.time()?.0.saturating_add(60_000_000_000),
            response: None,
        });
        Ok(Some(Outcome::Operation(wait)))
    }
    fn restart_operation_route(
        &mut self,
        req: &Request<'_>,
        actor: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        if req.method != "POST" {
            return Ok(None);
        }
        let retry = req
            .path
            .strip_prefix("/api/sessions/")
            .and_then(|p| p.strip_suffix("/retry"));
        let adopt = req
            .path
            .strip_prefix("/api/jobs/")
            .and_then(|p| p.strip_suffix("/template"));
        if retry
            .or(adopt)
            .is_none_or(|id| id.is_empty() || id.contains('/'))
        {
            return Ok(None);
        }
        let value = if req.body.is_empty() {
            Value::Null
        } else {
            Value::from_json(req.body)?
        };
        let field = |key| {
            value
                .as_object()
                .and_then(|o| o.get(key))
                .unwrap_or(&Value::Null)
        };
        let (job, session, restart) = if let Some(id) = retry {
            let view = self.store.workflow_for_session(id)?;
            if !view.job.allows_operator(actor)
                || view.job.current_phase_run_id != view.run.id
                || matches!(view.job.status.as_str(), d::JOB_DONE | d::JOB_CANCELLED)
                || !matches!(
                    view.run.status.as_str(),
                    d::PHASE_RUN_QUEUED | d::PHASE_RUN_RUNNING | d::PHASE_RUN_PENDING
                )
            {
                return Err(Error::Http(
                    409,
                    "only the active workflow Session can be retried",
                ));
            }
            let note = field("note").as_str().unwrap_or("").trim();
            if note.len() > 4000 {
                return Err(Error::Http(400, "restart note exceeds 4000 bytes"));
            }
            self.agent_start_failures.remove(id);
            self.prepare_failures.remove(id);
            (
                view.job.id,
                try_string(id)?,
                Restart::Retry {
                    note: try_string(note)?,
                    transcript: List::<d::ChatLine>::from_value(field("transcript"))?,
                },
            )
        } else {
            let id = adopt.ok_or(Error::Http(400, "Job is required"))?;
            let job = self.store.job(id)?;
            let phase = field("phase_id").as_str().unwrap_or("").trim();
            if !job.allows_operator(actor)
                || matches!(job.status.as_str(), d::JOB_DONE | d::JOB_CANCELLED)
                || phase.is_empty()
                || !self
                    .store
                    .snapshot()?
                    .workflow_templates
                    .iter()
                    .any(|t| t.id == job.template_id && t.phases.iter().any(|p| p.id == phase))
            {
                return Err(Error::Http(
                    409,
                    "choose a step of the Job's current Template",
                ));
            }
            let session = job
                .session_ids
                .iter()
                .find_map(|id| {
                    self.store
                        .session(id)
                        .ok()
                        .filter(|s| s.phase_run_id == job.current_phase_run_id)
                        .map(|s| s.id.as_str())
                })
                .unwrap_or("");
            (
                try_string(id)?,
                try_string(session)?,
                Restart::Adopt {
                    phase: try_string(phase)?,
                },
            )
        };
        if self.job_operation_pending(&job) {
            return Err(Error::Http(409, "Job operation already in progress"));
        }
        self.operations
            .retain(|op| !op.finished() || op.expires > now.time().map_or(0, |t| t.0));
        if self.operations.len() >= 32 {
            return Err(Error::Http(503, "Job operation capacity reached"));
        }
        self.operations
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        let id = random.next("op")?;
        let wait = OperationWait {
            id: id.try_clone()?,
        };
        self.operations.push(Operation {
            id,
            job,
            actor: try_string(actor)?,
            delete: false,
            artifact: None,
            restart: Some(restart),
            session,
            preserved: false,
            preserving: None,
            expires: now.time()?.0.saturating_add(300_000_000_000),
            response: None,
        });
        Ok(Some(Outcome::Operation(wait)))
    }
    /// Iedere ronde doet hoogstens één stop of definitieve mutatie.
    pub fn maintain_operations(&mut self, now: &Timestamp, random: &mut impl Runtime) -> Result {
        self.maintain_login_operations(now, random)?;
        for index in 0..self.operations.len() {
            if self.operations[index].response.is_some() {
                continue;
            }
            let result = self.advance_operation(index, now, random);
            let response = match result {
                Ok(None) => continue,
                Ok(Some(response)) => response,
                Err(error) => error.response()?,
            };
            self.operations[index].response = Some(response);
            self.operations[index].expires = now.time()?.0.saturating_add(300_000_000_000);
            self.last_launch_sweep = None;
            return Ok(());
        }
        Ok(())
    }
    fn advance_operation(
        &mut self,
        index: usize,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let op = &self.operations[index];
        if op.expires <= now.time()?.0 {
            return Err(Error::Http(
                503,
                "capsule stop is not confirmed; retry the Job operation",
            ));
        }
        if op.artifact.is_some() {
            return self.advance_artifact_deletion(index, now, random);
        }
        let (job, mut compositions) = self.store.prepare_job_deletion(&op.job, &op.actor)?;
        let delete = op.delete;
        let restart = op.restart.is_some();
        if restart {
            let session = op.session.try_clone()?;
            if session.is_empty() {
                compositions = List::new();
            } else {
                let selected = &self.store.session(&session)?.prepared_composition_id;
                compositions.retain(|c| c.id == *selected);
            }
            if !self.operations[index].preserved {
                if let Some(wait) = self.operations[index].preserving.take() {
                    match self.poll_capsule(&wait, now)? {
                        None => {
                            self.operations[index].preserving = Some(wait);
                            return Ok(None);
                        }
                        Some(response) if response.status >= 400 => {
                            return Err(Error::Http(
                                502,
                                "workspace could not be saved; retry was not applied",
                            ));
                        }
                        Some(_) => self.operations[index].preserved = true,
                    }
                } else {
                    if compositions.iter().any(|c| {
                        self.calls
                            .iter()
                            .any(|call| !call.finished && call.action.object() == c.id)
                    }) {
                        return Ok(None);
                    }
                    if let Some(composition) = compositions.iter().next() {
                        self.stop_composition_agents(&composition.id)?;
                        if let Some(wait) = self.preserve_for_restart(&session, now, random)? {
                            self.operations[index].preserving = Some(wait);
                            return Ok(None);
                        }
                    }
                    self.operations[index].preserved = true;
                }
            }
        }
        // A materialization already dispatched can still return a live container.
        // Await that result before removing the graph that must own its stop.
        if self.calls.iter().any(|c| !c.finished && matches!(&c.action, crate::capsules::Action::Materialize(w) if compositions.iter().any(|p| p.id == w.id))) {
            return Ok(None);
        }
        for composition in compositions.iter() {
            self.stop_composition_agents(&composition.id)?;
            let Some(capsule) = composition
                .runtime
                .as_ref()
                .filter(|r| r.status != "stopped")
            else {
                continue;
            };
            let online = self
                .runners
                .iter()
                .any(|r| r.client().id == capsule.client_id && r.is_connected());
            let pending = self
                .calls
                .iter()
                .any(|c| !c.finished && c.action.object() == composition.id);
            if pending {
                return Ok(None);
            }
            if (!capsule.stop_pending || online)
                && let Some(wait) =
                    self.begin_stop(&composition.id, "job_operation", now, random)?
            {
                self.detach_capsule(wait);
                return Ok(None);
            }
            if delete || restart {
                return Ok(None);
            }
            // An offline runner retains a durable stop obligation and its login.
        }
        let actor = self.operations[index].actor.try_clone()?;
        if let Some(restart) = &self.operations[index].restart {
            let (created, _) = match restart {
                Restart::Retry { note, transcript } => self.store.retry_workflow_session(
                    &self.operations[index].session,
                    &actor,
                    note,
                    transcript,
                    now,
                )?,
                Restart::Adopt { phase } => self.store.adopt_workflow_template(
                    &job.id,
                    &actor,
                    phase,
                    spin_store::Mutation { now, ids: random },
                )?,
            };
            return Ok(Some(Response::json(202, &created)?));
        }
        if !delete {
            let closed = self.store.close_job(&job.id, &actor, now)?;
            self.store
                .forget_workflow_tokens(closed.session_ids.as_slice())?;
            return Ok(Some(Response::json(200, &closed)?));
        }
        let mut garbage = List::<String>::new();
        let snapshot = self.store.snapshot()?;
        for attachment in job.attachment_ids.iter() {
            garbage.push(spin_core::validation::text(format_args!(
                "attachment:{attachment}"
            ))?)?;
        }
        for delivery in snapshot.deliverables.iter().filter(|d| d.job_id == job.id) {
            // Shared bundle references remain until the last remaining deliverable is gone.
            if let Some(bundle) = &delivery.bundle
                && !bundle.r#ref.is_empty()
                && !snapshot.deliverables.iter().any(|other| {
                    other.job_id != job.id
                        && other
                            .bundle
                            .as_ref()
                            .is_some_and(|b| b.r#ref == bundle.r#ref)
                })
            {
                garbage.push(bundle.r#ref.try_clone()?)?;
            }
        }
        let deleted = self
            .store
            .delete_job_with_blobs(&job.id, &actor, garbage.as_slice())?;
        Ok(Some(Response::json(200, &deleted)?))
    }
    /// Neemt het antwoord eenmaal over; een verbroken verbinding annuleert geen stop.
    pub fn poll_operation(&mut self, wait: &OperationWait) -> Result<Option<Response>> {
        if let Some(index) = self.login_operations.iter().position(|op| op.id == wait.id) {
            return Ok(if self.login_operations[index].response.is_some() {
                self.login_operations.remove(index).response
            } else {
                None
            });
        }
        let index = self
            .operations
            .iter()
            .position(|op| op.id == wait.id)
            .ok_or(Error::Http(404, "operation expired"))?;
        if self.operations[index].response.is_none() {
            return Ok(None);
        }
        Ok(self.operations.remove(index).response)
    }
}
