//! Activatie-epochs schermen elke workerwrite af tegen vorige uitvoeringen.
use crate::{Context, Error, Persistence, Result, Store};
use spin_domain::{self as d, Timestamp, TryClone, WireMap, state::PersistedState, try_string};

fn active<'a>(
    state: &'a PersistedState,
    session: &str,
    activation: &str,
    epoch: i64,
) -> Result<(&'a d::Session, &'a d::Activation)> {
    let s = state.sessions.get(session).ok_or(Error::NotFound)?;
    let a = state.activations.get(activation).ok_or(Error::NotFound)?;
    if s.activation_id != activation
        || s.activation_epoch != epoch
        || a.session_id != session
        || a.epoch != epoch
        || a.status == d::ACTIVATION_ENDED
    {
        return Err(Error::StaleActivation);
    }
    Ok((s, a))
}
fn lease(now: &Timestamp) -> Result<Timestamp> {
    Ok(Timestamp::from_time(d::Time(
        now.time()?
            .0
            .checked_add(30_000_000_000)
            .ok_or(Error::Conflict("lease time overflow"))?,
    ))?)
}
impl<P: Persistence> Store<P> {
    /// Externe workers claimen uitsluitend normale queued Sessions; workflows blijven bij ACP.
    pub fn claim(&mut self, req: d::ClaimRequest, context: Context<'_>) -> Result<d::Assignment> {
        context.validate()?;
        if self.client(&req.client_id)?.draining {
            return Err(Error::NoWork);
        }
        let result = self.edit(|state| {
            let client = state
                .clients
                .get_mut(&req.client_id)
                .ok_or(Error::NotFound)?;
            client.last_seen_at = context.now.try_clone()?;
            client.status = try_string("online")?;
            let mut choice: Option<&d::Session> = None;
            for (_, s) in state.sessions.iter() {
                if !s.phase_run_id.is_empty()
                    || s.status != d::SESSION_QUEUED
                    || !req.tools.iter().any(|t| t == "*" || *t == s.tool)
                {
                    continue;
                }
                if let Some(current) = choice
                    && (s.created_at.time()?, &s.id) >= (current.created_at.time()?, &current.id)
                {
                    continue;
                }
                choice = Some(s);
            }
            let Some(mut s) = choice.map(TryClone::try_clone).transpose()? else {
                return Ok(None);
            };
            if state.activations.get(context.id).is_some() {
                return Err(Error::Conflict("activation id already exists"));
            }
            s.activation_epoch = s
                .activation_epoch
                .checked_add(1)
                .ok_or(Error::Conflict("activation epoch exhausted"))?;
            let composition = state
                .compositions
                .get(&s.prepared_composition_id)
                .map(TryClone::try_clone)
                .transpose()?;
            let mut bindings = WireMap::default();
            if let Some(c) = &composition {
                bindings = WireMap::new();
                for (slot, id) in c.slot_bindings.iter() {
                    if slot.starts_with("credential:") {
                        bindings.insert(try_string(slot)?, id.try_clone()?)?;
                    }
                }
            }
            let a = d::Activation {
                id: try_string(context.id)?,
                session_id: s.id.try_clone()?,
                client_id: req.client_id.try_clone()?,
                operator: s.operator.try_clone()?,
                composition_id: s.prepared_composition_id.try_clone()?,
                epoch: s.activation_epoch,
                status: try_string(d::ACTIVATION_CLAIMED)?,
                started_at: context.now.try_clone()?,
                credential_bindings: bindings,
                ..Default::default()
            };
            s.client_id = req.client_id;
            s.activation_id = a.id.try_clone()?;
            s.lease_expires_at = Some(lease(context.now)?);
            s.status = try_string(d::SESSION_CLAIMED)?;
            s.updated_at = context.now.try_clone()?;
            let job = state
                .jobs
                .get(&s.job_id)
                .ok_or(Error::NotFound)?
                .try_clone()?;
            state.sessions.insert(s.id.try_clone()?, s.try_clone()?)?;
            state
                .activations
                .insert(a.id.try_clone()?, a.try_clone()?)?;
            Ok(Some(d::Assignment {
                job,
                session: s,
                activation: a,
                composition,
            }))
        })?;
        result.ok_or(Error::NoWork)
    }
    /// Een actuele claim kan idempotent naar running gaan.
    pub fn start_session(
        &mut self,
        id: &str,
        req: &d::ActivationRequest,
        now: &Timestamp,
    ) -> Result<d::Session> {
        self.edit(|state| {
            let (s, a) = active(state, id, &req.activation_id, req.epoch)?;
            if !matches!(s.status.as_str(), d::SESSION_CLAIMED | d::SESSION_RUNNING) {
                return Err(Error::Conflict("Session is not claimed or running"));
            }
            let mut s = s.try_clone()?;
            let mut a = a.try_clone()?;
            s.status = try_string(d::SESSION_RUNNING)?;
            s.updated_at = now.try_clone()?;
            a.status = try_string(d::ACTIVATION_RUNNING)?;
            state.activations.insert(a.id.try_clone()?, a)?;
            state.sessions.insert(s.id.try_clone()?, s.try_clone()?)?;
            Ok(s)
        })
    }
    /// Alleen de huidige activatie kan de dertigsecondenlease verlengen.
    pub fn heartbeat(
        &mut self,
        id: &str,
        req: &d::ActivationRequest,
        now: &Timestamp,
    ) -> Result<d::Activation> {
        self.edit(|state| {
            let a = state.activations.get(id).ok_or(Error::NotFound)?;
            if req.activation_id != id || req.epoch != a.epoch || a.status == d::ACTIVATION_ENDED {
                return Err(Error::StaleActivation);
            }
            let s = state
                .sessions
                .get(&a.session_id)
                .filter(|s| s.activation_id == id && s.activation_epoch == req.epoch)
                .ok_or(Error::StaleActivation)?;
            let mut s = s.try_clone()?;
            let a = a.try_clone()?;
            s.lease_expires_at = Some(lease(now)?);
            s.updated_at = now.try_clone()?;
            let client = state.clients.get_mut(&a.client_id).ok_or(Error::NotFound)?;
            client.last_seen_at = now.try_clone()?;
            client.status = try_string("online")?;
            state.sessions.insert(s.id.try_clone()?, s)?;
            Ok(a)
        })
    }
    /// Er draait hoogstens één turn tegelijk per Session.
    pub fn start_turn(
        &mut self,
        id: &str,
        req: d::CreateTurnRequest,
        context: Context<'_>,
    ) -> Result<d::Turn> {
        context.validate()?;
        self.edit(|state| {
            let (session, activation) = active(state, id, &req.activation_id, req.epoch)?;
            if session.status != d::SESSION_RUNNING {
                return Err(Error::Conflict("Session is not running"));
            }
            if req.input.trim().is_empty() {
                return Err(Error::Conflict("turn input is required"));
            }
            if session.turn_ids.iter().any(|id| {
                state
                    .turns
                    .get(id)
                    .is_some_and(|t| t.status == d::TURN_RUNNING)
            }) {
                return Err(Error::Conflict("Session already has a running turn"));
            }
            if state.turns.get(context.id).is_some() {
                return Err(Error::Conflict("turn id already exists"));
            }
            let turn = d::Turn {
                id: try_string(context.id)?,
                session_id: session.id.try_clone()?,
                activation_id: req.activation_id,
                activation_epoch: req.epoch,
                sequence: i64::try_from(session.turn_ids.len() + 1)
                    .map_err(|_| Error::Conflict("too many turns"))?,
                input: try_string(req.input.trim())?,
                actor: req.actor,
                credential_bindings: activation.credential_bindings.try_clone()?,
                status: try_string(d::TURN_RUNNING)?,
                started_at: context.now.try_clone()?,
                ..Default::default()
            };
            let s = state.sessions.get_mut(id).ok_or(Error::NotFound)?;
            s.turn_ids.push(turn.id.try_clone()?)?;
            s.updated_at = context.now.try_clone()?;
            state
                .turns
                .insert(turn.id.try_clone()?, turn.try_clone()?)?;
            Ok(turn)
        })
    }
    /// Een checkpoint hoort bij dezelfde activatie en optioneel zijn nog lopende turn.
    pub fn add_checkpoint(
        &mut self,
        id: &str,
        mut req: d::CreateCheckpointRequest,
        context: Context<'_>,
    ) -> Result<d::Checkpoint> {
        context.validate()?;
        if req.kind.is_empty() {
            req.kind = try_string(d::CHECKPOINT_TURN_END)?;
        }
        self.edit(|state| {
            let (s, _) = active(state, id, &req.activation_id, req.epoch)?;
            if !matches!(s.status.as_str(), d::SESSION_RUNNING | d::SESSION_CLAIMED) {
                return Err(Error::Conflict("Session cannot checkpoint"));
            }
            if !req.turn_id.is_empty() {
                let turn = state
                    .turns
                    .get(&req.turn_id)
                    .filter(|t| t.session_id == id)
                    .ok_or(Error::NotFound)?;
                if turn.activation_id != req.activation_id
                    || turn.activation_epoch != req.epoch
                    || turn.status != d::TURN_RUNNING
                {
                    return Err(Error::StaleActivation);
                }
            }
            if state.checkpoints.get(context.id).is_some() {
                return Err(Error::Conflict("checkpoint id already exists"));
            }
            let checkpoint = d::Checkpoint {
                id: try_string(context.id)?,
                session_id: try_string(id)?,
                activation_id: req.activation_id,
                activation_epoch: req.epoch,
                turn_id: req.turn_id,
                parent_checkpoint_id: s.current_checkpoint_id.try_clone()?,
                sequence: i64::try_from(s.checkpoint_ids.len() + 1)
                    .map_err(|_| Error::Conflict("too many checkpoints"))?,
                kind: req.kind,
                summary: req.summary,
                capsule: req.capsule,
                created_at: context.now.try_clone()?,
            };
            let s = state.sessions.get_mut(id).ok_or(Error::NotFound)?;
            s.checkpoint_ids.push(checkpoint.id.try_clone()?)?;
            s.current_checkpoint_id = checkpoint.id.try_clone()?;
            s.updated_at = context.now.try_clone()?;
            if checkpoint.capsule.restorable {
                s.continuity_level = try_string("full_checkpoint")?;
                s.continuity_score = 95;
            }
            if !checkpoint.turn_id.is_empty()
                && matches!(
                    checkpoint.kind.as_str(),
                    d::CHECKPOINT_TURN_END | d::CHECKPOINT_RESULT
                )
            {
                let turn = state
                    .turns
                    .get_mut(&checkpoint.turn_id)
                    .ok_or(Error::NotFound)?;
                turn.status = try_string(d::TURN_COMPLETED)?;
                turn.checkpoint_id = checkpoint.id.try_clone()?;
                turn.ended_at = Some(context.now.try_clone()?);
            }
            state
                .checkpoints
                .insert(checkpoint.id.try_clone()?, checkpoint.try_clone()?)?;
            Ok(checkpoint)
        })
    }
    /// Een resultaat beëindigt zijn activatie en bepaalt of de Job review of vergelijking vraagt.
    pub fn complete_session(
        &mut self,
        id: &str,
        mut req: d::CreateResultRequest,
        context: Context<'_>,
    ) -> Result<d::Result> {
        context.validate()?;
        if req.status.is_empty() {
            req.status = try_string(d::RESULT_FAILED)?;
        }
        if !matches!(
            req.status.as_str(),
            d::RESULT_SUCCESS | d::RESULT_PARTIAL | d::RESULT_FAILED
        ) || req.summary.trim().is_empty()
        {
            return Err(Error::Conflict(
                "valid result status and summary are required",
            ));
        }
        self.edit(|state| {
            let (session, activation) = active(state, id, &req.activation_id, req.epoch)?;
            if session.status == d::SESSION_COMPLETED {
                return Err(Error::Conflict("Session is already completed"));
            }
            let checkpoint = state
                .checkpoints
                .get(&req.checkpoint_id)
                .filter(|c| c.session_id == id)
                .ok_or(Error::NotFound)?;
            if checkpoint.kind != d::CHECKPOINT_RESULT {
                return Err(Error::Conflict("result must reference a result checkpoint"));
            }
            if state.results.get(context.id).is_some() {
                return Err(Error::Conflict("result id already exists"));
            }
            let mut session = session.try_clone()?;
            let mut activation = activation.try_clone()?;
            let result = d::Result {
                id: try_string(context.id)?,
                job_id: session.job_id.try_clone()?,
                session_id: session.id.try_clone()?,
                checkpoint_id: checkpoint.id.try_clone()?,
                status: req.status,
                summary: req.summary,
                git_head: req.git_head,
                tests: req.tests,
                acceptance_evidence: req.acceptance_evidence,
                open_issues: req.open_issues,
                usage: req.usage,
                created_at: context.now.try_clone()?,
            };
            session.status = try_string(d::SESSION_COMPLETED)?;
            session.final_result_id = result.id.try_clone()?;
            session.lease_expires_at = None;
            session.updated_at = context.now.try_clone()?;
            activation.status = try_string(d::ACTIVATION_ENDED)?;
            activation.reason = try_string("completed")?;
            activation.ended_at = Some(context.now.try_clone()?);
            let job = state.jobs.get_mut(&session.job_id).ok_or(Error::NotFound)?;
            let previous_success = job.candidate_result_ids.iter().any(|id| {
                state
                    .results
                    .get(id)
                    .is_some_and(|r| r.status == d::RESULT_SUCCESS)
            });
            job.candidate_result_ids.push(result.id.try_clone()?)?;
            job.status = try_string(if result.status == d::RESULT_SUCCESS {
                if previous_success {
                    d::JOB_COMPARING
                } else {
                    d::JOB_REVIEW
                }
            } else {
                d::JOB_ACTIVE
            })?;
            job.updated_at = context.now.try_clone()?;
            state.sessions.insert(session.id.try_clone()?, session)?;
            state
                .activations
                .insert(activation.id.try_clone()?, activation)?;
            state
                .results
                .insert(result.id.try_clone()?, result.try_clone()?)?;
            Ok(result)
        })
    }
    /// Kiest uitsluitend een resultaat dat bij deze Job hoort.
    pub fn select_result(
        &mut self,
        job: &str,
        req: &d::SelectResultRequest,
        now: &Timestamp,
    ) -> Result<d::Job> {
        self.edit(|state| {
            let result = state
                .results
                .get(&req.result_id)
                .filter(|r| r.job_id == job)
                .ok_or(Error::NotFound)?;
            let job = state.jobs.get_mut(job).ok_or(Error::NotFound)?;
            job.final_result_id = result.id.try_clone()?;
            job.status = try_string(d::JOB_DONE)?;
            job.updated_at = now.try_clone()?;
            Ok(job.try_clone()?)
        })
    }
    /// Bewaart de gepushte work-in-progress-commit met een expliciete klok.
    pub fn set_session_sync(
        &mut self,
        id: &str,
        head: &str,
        now: &Timestamp,
    ) -> Result<d::Session> {
        self.edit(|state| {
            let s = state.sessions.get_mut(id).ok_or(Error::NotFound)?;
            s.synced_head = try_string(head)?;
            s.synced_at = Some(now.try_clone()?);
            Ok(s.try_clone()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Memory;
    use core::cell::Cell;
    use d::Wire;
    fn now() -> Timestamp {
        Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap()
    }
    #[test]
    fn activation_fencing_checkpoint_and_result_are_one_durable_lifecycle() {
        let fail = Cell::new(true);
        let state=PersistedState::from_json(br#"{
          "clients":{"c":{"id":"c"}},"jobs":{"j":{"id":"j"}},
          "sessions":{"s":{"id":"s","job_id":"j","status":"queued","tool":"codex"},"workflow":{"id":"workflow","job_id":"j","status":"queued","tool":"codex","phase_run_id":"p"}}
        }"#).unwrap();
        let mut store = Store::new(state, Memory(&fail));
        let req = || d::ClaimRequest::from_json(br#"{"client_id":"c","tools":["codex"]}"#).unwrap();
        assert_eq!(
            store
                .claim(
                    req(),
                    Context {
                        now: &now(),
                        id: "a"
                    }
                )
                .unwrap_err(),
            Error::Storage(10)
        );
        assert_eq!(store.state.sessions.get("s").unwrap().activation_epoch, 0);
        fail.set(false);
        let assignment = store
            .claim(
                req(),
                Context {
                    now: &now(),
                    id: "a",
                },
            )
            .unwrap();
        assert_eq!(assignment.session.id, "s");
        assert_eq!(assignment.activation.epoch, 1);
        assert_eq!(
            assignment.session.lease_expires_at.unwrap().as_str(),
            "2026-09-30T12:00:30Z"
        );
        let activation = d::ActivationRequest {
            activation_id: "a".into(),
            epoch: 1,
        };
        store.start_session("s", &activation, &now()).unwrap();
        let stale = d::ActivationRequest {
            activation_id: "a".into(),
            epoch: 0,
        };
        assert_eq!(
            store.heartbeat("a", &stale, &now()).unwrap_err(),
            Error::StaleActivation
        );
        let turn = || d::CreateTurnRequest {
            activation_id: "a".into(),
            epoch: 1,
            input: " work ".into(),
            actor: "derek".into(),
        };
        store
            .start_turn(
                "s",
                turn(),
                Context {
                    now: &now(),
                    id: "t",
                },
            )
            .unwrap();
        assert!(matches!(
            store.start_turn(
                "s",
                turn(),
                Context {
                    now: &now(),
                    id: "t2"
                }
            ),
            Err(Error::Conflict(_))
        ));
        let checkpoint = d::CreateCheckpointRequest {
            activation_id: "a".into(),
            epoch: 1,
            turn_id: "t".into(),
            kind: d::CHECKPOINT_RESULT.into(),
            ..Default::default()
        };
        store
            .add_checkpoint(
                "s",
                checkpoint,
                Context {
                    now: &now(),
                    id: "cp",
                },
            )
            .unwrap();
        assert_eq!(
            store.state.turns.get("t").unwrap().status,
            d::TURN_COMPLETED
        );
        let result = d::CreateResultRequest {
            activation_id: "a".into(),
            epoch: 1,
            checkpoint_id: "cp".into(),
            status: d::RESULT_SUCCESS.into(),
            summary: "done".into(),
            ..Default::default()
        };
        store
            .complete_session(
                "s",
                result,
                Context {
                    now: &now(),
                    id: "result",
                },
            )
            .unwrap();
        assert_eq!(store.state.jobs.get("j").unwrap().status, d::JOB_REVIEW);
        assert!(
            store
                .state
                .sessions
                .get("s")
                .unwrap()
                .lease_expires_at
                .is_none()
        );
        assert_eq!(
            store.heartbeat("a", &activation, &now()).unwrap_err(),
            Error::StaleActivation
        );
        assert_eq!(
            store
                .claim(
                    req(),
                    Context {
                        now: &now(),
                        id: "other"
                    }
                )
                .unwrap_err(),
            Error::NoWork
        );
        assert_eq!(
            store.state.sessions.get("workflow").unwrap().status,
            d::SESSION_QUEUED
        );
    }
}
