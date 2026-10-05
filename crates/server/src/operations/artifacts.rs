//! Drain the entire lineage before removing runner images and committing graph deletion.
use super::*;
use crate::capsules::Action;
use d::protocol as p;

pub(super) struct Deletion {
    id: String,
    admin: bool,
    pending: Option<CapsuleWait>,
    pending_snapshot: Option<String>,
    removed: Map<bool>,
}
impl<P: Persistence> Server<P> {
    pub(crate) fn artifact_operation_route(
        &mut self,
        req: &Request<'_>,
        user: &d::User,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        let Some(id) = req
            .path
            .strip_prefix("/api/artifacts/")
            .filter(|id| req.method == "DELETE" && !id.is_empty() && !id.contains('/'))
        else {
            return Ok(None);
        };
        let root = self.store.artifact(id)?;
        let admin = user.role == d::USER_ADMIN;
        if !admin && !root.created_by.is_empty() && root.created_by != user.username {
            return Err(Error::Http(
                403,
                "only the recorder or an admin can remove this layer",
            ));
        }
        if self
            .operations
            .iter()
            .any(|op| op.response.is_none() && op.artifact.as_ref().is_some_and(|a| a.id == id))
        {
            return Err(Error::Http(409, "layer deletion is already in progress"));
        }
        self.operations
            .retain(|op| op.response.is_none() || op.expires > now.time().map_or(0, |t| t.0));
        if self.operations.len() >= 32 {
            return Err(Error::Http(503, "operation capacity reached"));
        }
        self.operations
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        let operation = random.next("op")?;
        let wait = OperationWait {
            id: operation.try_clone()?,
        };
        self.operations.push(Operation {
            id: operation,
            job: String::new(),
            actor: user.username.try_clone()?,
            delete: true,
            artifact: Some(Deletion {
                id: try_string(id)?,
                admin,
                pending: None,
                pending_snapshot: None,
                removed: Map::new(),
            }),
            restart: None,
            session: String::new(),
            preserved: true,
            preserving: None,
            expires: now.time()?.0.saturating_add(300_000_000_000),
            response: None,
        });
        Ok(Some(Outcome::Operation(wait)))
    }
    pub(super) fn advance_artifact_deletion(
        &mut self,
        index: usize,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let work = self.operations[index]
            .artifact
            .as_mut()
            .ok_or(Error::Http(500, "missing layer deletion"))?;
        if let Some(wait) = work.pending.take() {
            match self.poll_capsule(&wait, now) {
                Ok(None) => {
                    self.operations[index]
                        .artifact
                        .as_mut()
                        .ok_or(Error::Http(500, "missing layer deletion"))?
                        .pending = Some(wait);
                    return Ok(None);
                }
                Err(error) => {
                    self.operations[index]
                        .artifact
                        .as_mut()
                        .ok_or(Error::Http(500, "missing layer deletion"))?
                        .pending = Some(wait);
                    return Err(error);
                }
                Ok(Some(response)) if response.status >= 400 => return Ok(Some(response)),
                Ok(Some(_)) => {
                    let work = self.operations[index]
                        .artifact
                        .as_mut()
                        .ok_or(Error::Http(500, "missing layer deletion"))?;
                    if let Some(key) = work.pending_snapshot.as_ref() {
                        work.removed.insert(key.try_clone()?, true)?;
                    }
                    work.pending_snapshot = None;
                }
            }
        }
        let work = self.operations[index]
            .artifact
            .as_ref()
            .ok_or(Error::Http(500, "missing layer deletion"))?;
        let members = self.store.artifact_tree(&work.id)?;
        let uses = |ids: &[String]| ids.iter().any(|id| members.iter().any(|a| a.id == *id));
        let snapshot = self.store.snapshot()?;
        for composition in snapshot.compositions.iter() {
            if !uses(composition.layers.as_slice())
                && !composition
                    .resolved_artifacts
                    .iter()
                    .any(|r| members.iter().any(|a| a.id == r.artifact_id))
            {
                continue;
            }
            if self
                .calls
                .iter()
                .any(|c| !c.finished && c.action.object() == composition.id)
            {
                return Ok(None);
            }
            if composition
                .runtime
                .as_ref()
                .is_some_and(|r| r.status != "stopped")
            {
                if let Some(wait) =
                    self.begin_stop(&composition.id, "artifact_operation", now, random)?
                {
                    self.operations[index]
                        .artifact
                        .as_mut()
                        .ok_or(Error::Http(500, "missing layer deletion"))?
                        .pending = Some(wait);
                }
                return Ok(None);
            }
        }
        if let Some(recording) = snapshot
            .recordings
            .iter()
            .find(|r| r.status == d::RECORDING_OPEN && uses(r.parent_artifact_ids.as_slice()))
        {
            if self
                .calls
                .iter()
                .any(|c| !c.finished && c.action.object() == recording.id)
            {
                return Ok(None);
            }
            if let Some(capsule) = recording
                .runtime
                .as_ref()
                .filter(|r| r.status != "stopped" && !r.container_id.is_empty())
            {
                if !self
                    .runners
                    .iter()
                    .any(|p| p.client().id == capsule.client_id && p.is_connected())
                {
                    return Ok(None);
                }
                let wait = self.enqueue_call(
                    Action::Cancel {
                        recording: recording.id.try_clone()?,
                        actor: recording.actor.try_clone()?,
                    },
                    &capsule.client_id,
                    p::METHOD_CANCEL_RECORDING,
                    &p::RecordingPayload {
                        recording: recording.try_clone()?,
                    },
                    now,
                    random,
                )?;
                self.operations[index]
                    .artifact
                    .as_mut()
                    .ok_or(Error::Http(500, "missing layer deletion"))?
                    .pending = Some(wait);
            } else {
                self.store
                    .cancel_recording(&recording.id, &recording.actor, now)?;
            }
            return Ok(None);
        }
        let work = self.operations[index]
            .artifact
            .as_ref()
            .ok_or(Error::Http(500, "missing layer deletion"))?;
        self.store.prepare_artifact_deletion(
            &work.id,
            &self.operations[index].actor,
            work.admin,
        )?;
        for artifact in members.iter() {
            if snapshot.artifacts.iter().any(|other| {
                !members.iter().any(|m| m.id == other.id)
                    && other.snapshot.digest == artifact.snapshot.digest
            }) {
                continue;
            }
            for client in core::iter::once(&artifact.snapshot.client_id)
                .chain(artifact.snapshot.replica_client_ids.iter())
            {
                if client.is_empty()
                    || !self
                        .runners
                        .iter()
                        .any(|p| p.client().id == *client && p.is_connected())
                {
                    continue;
                }
                let key = spin_core::validation::text(format_args!("{}:{client}", artifact.id))?;
                if work.removed.contains_key(&key) {
                    continue;
                }
                let wait = self.enqueue_call(
                    Action::RemoveSnapshot {
                        artifact: artifact.id.try_clone()?,
                    },
                    client,
                    p::METHOD_REMOVE_SNAPSHOT,
                    &p::SnapshotPayload {
                        snapshot: artifact.snapshot.try_clone()?,
                    },
                    now,
                    random,
                )?;
                let work = self.operations[index]
                    .artifact
                    .as_mut()
                    .ok_or(Error::Http(500, "missing layer deletion"))?;
                work.pending = Some(wait);
                work.pending_snapshot = Some(key);
                return Ok(None);
            }
        }
        let response = Response::json(200, self.store.artifact(&work.id)?)?;
        self.store
            .delete_artifact_tree(&work.id, &self.operations[index].actor, work.admin)?;
        Ok(Some(response))
    }
}
