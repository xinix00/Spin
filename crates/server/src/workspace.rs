//! Na een agentbeurt worden Session-werk en capsulewijzigingen apart bevestigd.
use super::*;
use crate::capsules::{Action, CapsuleWait};
use alloc::vec::Vec;
use d::{protocol as p, try_string};
pub(crate) struct Preservation {
    pub(crate) composition: String,
    session: String,
    stop_actor: Option<String>,
    steps: Vec<Step>,
    at: usize,
    strict_sync: bool,
}
struct Step {
    method: &'static str,
    payload: Value,
}
impl<P: Persistence> Server<P> {
    pub(crate) fn preserve_after_turn(
        &mut self,
        id: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<CapsuleWait>> {
        self.preserve_workspace(id, false, now, random)
    }
    pub(crate) fn preserve_for_restart(
        &mut self,
        id: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<CapsuleWait>> {
        self.preserve_workspace(id, true, now, random)
    }
    fn preserve_workspace(
        &mut self,
        id: &str,
        force_sync: bool,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<CapsuleWait>> {
        let session = self.store.session(id)?;
        let composition = self.store.composition(&session.prepared_composition_id)?;
        let Some(capsule) = composition
            .runtime
            .as_ref()
            .filter(|r| r.status == "ready" && !r.stop_pending)
        else {
            return Ok(None);
        };
        let mut sync = !session.git_ref.is_empty();
        let mut force = force_sync;
        if !session.phase_run_id.is_empty() {
            let view = self.store.workflow_for_session(id)?;
            sync &= view.phase.allow_changes
                && matches!(
                    view.run.status.as_str(),
                    d::PHASE_RUN_RUNNING | d::PHASE_RUN_PENDING | d::PHASE_RUN_QUEUED
                );
            force |= view.run.status == d::PHASE_RUN_PENDING && view.run.pending_reason == "ask";
        }
        if !force
            && let Some(last) = &session.synced_at
            && now.time()?.0.saturating_sub(last.time()?.0) < 20_000_000_000
        {
            sync = false;
        }
        let mut steps = Vec::new();
        if sync {
            for workspace in composition.changed_workspaces() {
                if steps.len() >= 32 {
                    return Err(Error::Http(413, "too many workspace repositories"));
                }
                d::try_push(
                    &mut steps,
                    Step {
                        method: p::METHOD_SYNC_WORKSPACE,
                        payload: p::SyncPayload {
                            runtime: capsule.try_clone()?,
                            sync: d::engine::WorkspaceSync {
                                path: workspace.path.try_clone()?,
                                session_ref: session.git_ref.try_clone()?,
                                authentication: self
                                    .workspace_authentication(workspace, &composition.operator)?,
                            },
                        }
                        .to_value()?,
                    },
                )?;
            }
        }
        d::try_push(
            &mut steps,
            Step {
                method: p::METHOD_CAPSULE_CHANGES,
                payload: p::RuntimePayload {
                    runtime: capsule.try_clone()?,
                }
                .to_value()?,
            },
        )?;
        let method = steps[0].method;
        let payload = steps[0].payload.try_clone()?;
        let client = capsule.client_id.try_clone()?;
        let work = Preservation {
            composition: composition.id.try_clone()?,
            session: try_string(id)?,
            stop_actor: None,
            steps,
            at: 0,
            strict_sync: force_sync,
        };
        Ok(Some(self.enqueue_call(
            Action::Preserve(work),
            &client,
            method,
            &payload,
            now,
            random,
        )?))
    }
    pub(crate) fn begin_stop(
        &mut self,
        id: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<CapsuleWait>> {
        let composition = self.store.composition(id)?.try_clone()?;
        let mut capsule = composition
            .runtime
            .as_ref()
            .ok_or(Error::Http(409, "composition has no capsule"))?
            .try_clone()?;
        if capsule.status == "stopped" {
            return Ok(None);
        }
        if !capsule.stop_pending {
            capsule.stop_pending = true;
            self.store
                .set_composition_runtime(id, &composition.operator, capsule.try_clone()?)?;
        }
        self.stop_composition_agents(id)?;
        // Maak eerst de stop duurzaam; pas na een runnerbevestiging komen logins vrij.
        if !self
            .runners
            .iter()
            .any(|p| p.client().id == capsule.client_id && p.is_connected())
        {
            return Ok(None);
        }
        let mut steps = Vec::new();
        if !composition.session_id.is_empty() {
            d::try_push(
                &mut steps,
                Step {
                    method: p::METHOD_STOP_APP,
                    payload: p::AppPayload {
                        session_id: composition.session_id.try_clone()?,
                        ..Default::default()
                    }
                    .to_value()?,
                },
            )?;
        }

        for target in self
            .tracked_targets(&composition)?
            .into_iter()
            .filter(|_| capsule.status != "installing_login")
        {
            if steps.len() >= 128 {
                return Err(Error::Http(413, "too many tracked login targets"));
            }
            d::try_push(
                &mut steps,
                Step {
                    method: p::METHOD_READ_TRACKED,
                    payload: p::TrackedFilesPayload {
                        runtime: capsule.try_clone()?,
                        paths: target.paths,
                        excludes: target.excludes,
                        ..Default::default()
                    }
                    .to_value()?,
                },
            )?;
        }
        d::try_push(
            &mut steps,
            Step {
                method: p::METHOD_CAPSULE_CHANGES,
                payload: p::RuntimePayload {
                    runtime: capsule.try_clone()?,
                }
                .to_value()?,
            },
        )?;
        d::try_push(
            &mut steps,
            Step {
                method: p::METHOD_STOP,
                payload: p::RuntimePayload {
                    runtime: capsule.try_clone()?,
                }
                .to_value()?,
            },
        )?;
        let method = steps[0].method;
        let payload = steps[0].payload.try_clone()?;
        let work = Preservation {
            composition: composition.id,
            session: composition.session_id,
            stop_actor: Some(composition.operator),
            steps,
            at: 0,
            strict_sync: false,
        };
        match self.enqueue_call(
            Action::Preserve(work),
            &capsule.client_id,
            method,
            &payload,
            now,
            random,
        ) {
            Ok(wait) => Ok(Some(wait)),
            Err(Error::Http(409 | 503, _)) => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub(crate) fn sweep_idle_capsules(
        &mut self,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        let snapshot = self.store.snapshot()?;
        for composition in snapshot.compositions.iter() {
            if self
                .calls
                .iter()
                .any(|c| !c.finished && c.action.object() == composition.id)
            {
                continue;
            }
            let Some(capsule) = &composition.runtime else {
                if now
                    .time()?
                    .0
                    .saturating_sub(composition.created_at.time()?.0)
                    > 600_000_000_000
                {
                    self.store
                        .discard_composition(&composition.id, &composition.operator, now)?;
                }
                continue;
            };
            if capsule.status == "stopped"
                || capsule.stop_pending
                || composition.session_id.is_empty()
            {
                continue;
            }
            let session = snapshot
                .sessions
                .iter()
                .find(|s| s.id == composition.session_id);
            let obsolete = match session {
                None => true,
                Some(session) if session.phase_run_id.is_empty() => false,
                Some(session) if session.prepared_composition_id != composition.id => true,
                Some(session) => snapshot
                    .jobs
                    .iter()
                    .find(|j| j.id == session.job_id)
                    .is_none_or(|j| matches!(j.status.as_str(), d::JOB_DONE | d::JOB_CANCELLED)),
            };
            if obsolete && let Some(wait) = self.begin_stop(&composition.id, now, random)? {
                self.detach_capsule(wait);
            }
        }
        Ok(())
    }
    pub(crate) fn retry_pending_stops(
        &mut self,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        for composition in self.store.running_compositions()?.iter() {
            if !composition
                .runtime
                .as_ref()
                .is_some_and(|r| r.stop_pending || r.status == "installing_login")
                || self
                    .calls
                    .iter()
                    .any(|c| !c.finished && c.action.object() == composition.id)
            {
                continue;
            }
            if let Some(wait) = self.begin_stop(&composition.id, now, random)? {
                self.detach_capsule(wait);
                break;
            }
        }
        Ok(())
    }
    pub(crate) fn advance_preservation(
        &mut self,
        index: usize,
        client: &str,
        message: &p::WireMessage,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let Action::Preserve(work) = &self.calls[index].action else {
            return Err(Error::Http(500, "missing preservation operation"));
        };
        let step = &work.steps[work.at];
        let payload = message.payload.0.as_ref().unwrap_or(&Value::Null);
        let mut next = work.at + 1;
        if message.error.is_empty() {
            if step.method == p::METHOD_SYNC_WORKSPACE {
                let result = d::engine::WorkspaceSyncResult::from_value(payload)?;
                if result.head.is_empty() {
                    return Err(Error::Http(502, "workspace sync returned no HEAD"));
                }
                if work.at == 0
                    && (result.pushed
                        || self.store.session(&work.session)?.synced_head != result.head)
                {
                    self.store
                        .set_session_sync(&work.session, &result.head, now)?;
                }
            } else if step.method == p::METHOD_READ_TRACKED {
                let mut report = p::TrackedFilesPayload::from_value(&step.payload)?;
                report.files = d::WireMap::<d::Bytes>::from_value(payload)?;
                let report = report.to_value()?;
                self.tracked_changed(client, &report, now)?;
            } else if step.method == p::METHOD_STOP {
                let mut capsule = p::RuntimePayload::from_value(&step.payload)?.runtime;
                capsule.status = try_string("stopped")?;
                capsule.stop_pending = false;
                let composition = self.store.set_composition_runtime(
                    &work.composition,
                    work.stop_actor
                        .as_deref()
                        .ok_or(Error::Http(500, "missing stop operator"))?,
                    capsule,
                )?;
                if let Some(peer) = self.runners.iter_mut().find(|p| p.client().id == client) {
                    peer.freed();
                }
                self.last_launch_sweep = None;
                return Ok(Some(Response::json(200, &composition)?));
            } else if step.method == p::METHOD_CAPSULE_CHANGES {
                self.store.set_composition_changes(
                    &work.composition,
                    d::LayerContents::from_value(payload)?,
                )?;
            }
        } else if step.method == p::METHOD_SYNC_WORKSPACE {
            if work.strict_sync {
                return Err(Error::Http(
                    502,
                    "workspace sync failed; capsule is retained",
                ));
            }
            // Een Git-fout verhindert het vastleggen van de capsulewijzigingen niet.
            next = work.steps.len() - 1;
        } else if step.method == p::METHOD_STOP {
            return Err(Error::Http(
                502,
                "could not stop capsule; stop remains pending",
            ));
        } else if work.stop_actor.is_none() {
            return Err(Error::Http(502, "could not capture capsule changes"));
        }
        // Net als de Go-engine stoppen we ook als een best-effort uitlezing faalt.
        let Action::Preserve(work) = &self.calls[index].action else {
            return Err(Error::Http(500, "missing preservation operation"));
        };
        if let Some(step) = work.steps.get(next) {
            let method = step.method;
            let payload = step.payload.try_clone()?;
            self.continue_capsule(index, client, method, &payload, random)?;
            if let Action::Preserve(work) = &mut self.calls[index].action {
                work.at = next;
            }
            Ok(None)
        } else {
            Ok(Some(Response::empty(204)?))
        }
    }
}
