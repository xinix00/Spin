//! Een recording wordt pas bij afronden een immutable artifactversie.
use crate::{Context, Error, Persistence, Result, Store};
use alloc::{string::String, vec::Vec};
use spin_core::validation::{
    artifact_scope_rank, can_use_artifact, normalize_enablements, normalized, text, valid_token,
};
use spin_domain::{self as d, List, Timestamp, TryClone, Wire, state::PersistedState, try_string};

fn unique(values: &[String]) -> Result<List<String>> {
    let mut out = List::new();
    for value in values {
        let value = value.trim();
        if !value.is_empty() && !out.iter().any(|s| s == value) {
            out.push(try_string(value)?)?;
        }
    }
    Ok(out)
}
pub(crate) fn latest<'a>(
    state: &'a PersistedState,
    kind: &str,
    name: &str,
    actor: &str,
    profile: &str,
) -> Result<&'a d::Artifact> {
    let mut selected = None;
    let mut selected_rank = -1;
    let mut selected_time = d::Time::default();
    for (_, artifact) in state.artifacts.iter() {
        if artifact.kind != kind
            || artifact.name != name
            || !artifact.superseded_by.is_empty()
            || !can_use_artifact(actor, artifact)?
            || (!profile.is_empty() && artifact.profile != profile)
        {
            continue;
        }
        let rank = artifact_scope_rank(artifact, actor)?;
        let created = artifact.created_at.time()?;
        if rank > selected_rank || (rank == selected_rank && created > selected_time) {
            selected = Some(artifact);
            selected_rank = rank;
            selected_time = created;
        }
    }
    selected.ok_or(Error::NotFound)
}
fn editable<'a>(
    state: &'a mut PersistedState,
    id: &str,
    actor: &str,
) -> Result<&'a mut d::Recording> {
    let recording = state.recordings.get_mut(id).ok_or(Error::NotFound)?;
    if recording.status != d::RECORDING_OPEN || recording.actor != normalized(actor)? {
        return Err(Error::Conflict(
            "recording is closed or belongs to another actor",
        ));
    }
    Ok(recording)
}
impl<P: Persistence> Store<P> {
    /// Start maximaal één recording per actor en valideert het versie-afstammingspad.
    pub fn create_recording(
        &mut self,
        mut req: d::CreateRecordingRequest,
        context: Context<'_>,
    ) -> Result<d::Recording> {
        context.validate()?;
        req.actor = normalized(&req.actor)?;
        req.name = normalized(&req.name)?;
        if req.actor.is_empty() || !valid_token(&req.name) || !valid_token(&req.kind) {
            return Err(Error::Conflict("actor, valid kind and name are required"));
        }
        if req.scope.is_empty() {
            req.scope = try_string(if req.kind == d::ARTIFACT_TOOL {
                d::SCOPE_GLOBAL
            } else {
                d::SCOPE_USER
            })?;
        }
        if !matches!(
            req.scope.as_str(),
            d::SCOPE_GLOBAL | d::SCOPE_TEAM | d::SCOPE_PROJECT | d::SCOPE_USER
        ) {
            return Err(Error::Conflict("invalid scope"));
        }
        if req.profile.is_empty() {
            req.profile = try_string("default")?;
        }
        if req.scope == d::SCOPE_USER {
            if req.subject.is_empty() {
                req.subject = req.actor.try_clone()?;
            }
            if normalized(&req.subject)? != req.actor {
                return Err(Error::Conflict(
                    "a user recording must belong to the current actor",
                ));
            }
            req.subject = req.actor.try_clone()?;
        } else {
            req.subject = normalized(&req.subject)?;
        }
        let selector = text(format_args!("{}:{}", req.kind, req.name))?;
        if req.provides.is_empty() {
            req.provides.push(selector.try_clone()?)?;
        }
        if req.slot.is_empty() {
            req.slot = selector;
        }
        if req.sensitivity.is_empty() {
            req.sensitivity = try_string(if req.kind == d::ARTIFACT_CREDENTIAL {
                d::SENSITIVITY_SECRET
            } else if req.scope == d::SCOPE_GLOBAL && req.kind == d::ARTIFACT_TOOL {
                d::SENSITIVITY_PUBLIC
            } else {
                d::SENSITIVITY_PRIVATE
            })?;
        }
        let parents = unique(&req.parent_artifact_ids)?;
        self.edit(|state| {
            if state.recordings.get(context.id).is_some() {
                return Err(Error::Conflict("recording id already exists"));
            }
            if state
                .recordings
                .iter()
                .any(|(_, r)| r.actor == req.actor && r.status == d::RECORDING_OPEN)
            {
                return Err(Error::Conflict("actor already has an open recording"));
            }
            for id in parents.iter() {
                let parent = state.artifacts.get(id).ok_or(Error::NotFound)?;
                if !can_use_artifact(&req.actor, parent)? {
                    return Err(Error::NotFound);
                }
            }
            let replaces = req.replaces_artifact_id.trim();
            if !replaces.is_empty() {
                let previous = state.artifacts.get(replaces).ok_or(Error::NotFound)?;
                if !can_use_artifact(&req.actor, previous)? {
                    return Err(Error::NotFound);
                }
                if previous.kind != req.kind
                    || previous.name != req.name
                    || !previous.superseded_by.is_empty()
                {
                    return Err(Error::Conflict(
                        "a new version records from the current version",
                    ));
                }
                if !parents.iter().any(|p| p == replaces) {
                    return Err(Error::Conflict(
                        "a new version records from the version it replaces",
                    ));
                }
            }
            let recording = d::Recording {
                id: try_string(context.id)?,
                actor: req.actor,
                kind: req.kind,
                name: req.name,
                scope: req.scope,
                subject: req.subject,
                profile: normalized(&req.profile)?,
                provides: unique(&req.provides)?,
                requires: unique(&req.requires)?,
                enables: normalize_enablements(&req.enables)?,
                slot: try_string(req.slot.trim())?,
                parent_artifact_ids: parents,
                compatibility_fingerprint: try_string(req.compatibility_fingerprint.trim())?,
                sensitivity: req.sensitivity,
                replaces_artifact_id: try_string(replaces)?,
                status: try_string(d::RECORDING_OPEN)?,
                commands: List::new(),
                started_at: context.now.try_clone()?,
                ..Default::default()
            };
            state
                .recordings
                .insert(recording.id.try_clone()?, recording.try_clone()?)?;
            Ok(recording)
        })
    }
    /// Voegt een genummerde uitvoering toe aan de open recording.
    pub fn record_execution(
        &mut self,
        id: &str,
        actor: &str,
        exit_code: Option<i64>,
        now: &Timestamp,
    ) -> Result<d::Recording> {
        self.edit(|state| {
            let recording = editable(state, id, actor)?;
            recording.commands.push(d::RecordingCommand {
                sequence: i64::try_from(recording.commands.len() + 1)
                    .map_err(|_| Error::Conflict("too many commands"))?,
                exit_code,
                at: now.try_clone()?,
            })?;
            Ok(recording.try_clone()?)
        })
    }
    /// Koppelt de nieuwste bruikbare versie van een parent aan een open recording.
    pub fn attach_recording_parent(
        &mut self,
        id: &str,
        req: &d::AttachRecordingParentRequest,
    ) -> Result<d::Recording> {
        self.edit(|state| {
            editable(state, id, &req.actor)?;
            let parent = latest(
                state,
                &req.kind,
                &normalized(&req.name)?,
                &normalized(&req.actor)?,
                "",
            )?
            .id
            .try_clone()?;
            let recording = editable(state, id, &req.actor)?;
            if !recording.parent_artifact_ids.contains(&parent) {
                recording.parent_artifact_ids.push(parent)?;
            }
            Ok(recording.try_clone()?)
        })
    }
    /// Persist the parent together with the obligation to replace the old container.
    pub fn prepare_recording_parent(
        &mut self,
        id: &str,
        req: &d::AttachRecordingParentRequest,
    ) -> Result<d::Recording> {
        self.edit(|state| {
            let recording = editable(state, id, &req.actor)?;
            if !recording.commands.is_empty() || !recording.parent_artifact_ids.is_empty() {
                return Err(Error::Conflict(
                    "FROM requires a recording without a parent or commands",
                ));
            }
            if recording.runtime.as_ref().is_none_or(|r| {
                r.container_id.is_empty() || r.stop_pending || r.status == "stopped"
            }) {
                return Err(Error::Conflict("recording is not ready"));
            }
            let parent = latest(
                state,
                &req.kind,
                &normalized(&req.name)?,
                &normalized(&req.actor)?,
                "",
            )?
            .id
            .try_clone()?;
            let recording = editable(state, id, &req.actor)?;
            recording.parent_artifact_ids.push(parent)?;
            if let Some(runtime) = recording.runtime.as_mut() {
                runtime.stop_pending = true;
            }
            Ok(recording.try_clone()?)
        })
    }
    /// Rondt af en draagt laagmetadata over zonder bestaande snapshots te verwijderen.
    pub fn end_recording(
        &mut self,
        id: &str,
        mut req: d::EndRecordingRequest,
        context: Context<'_>,
    ) -> Result<d::Artifact> {
        context.validate()?;
        self.edit(|state| {
            if state.artifacts.get(context.id).is_some() {
                return Err(Error::Conflict("artifact id already exists"));
            }
            let recording = editable(state, id, &req.actor)?.try_clone()?;
            let mut digest = try_string(if req.snapshot.digest.is_empty() {
                req.snapshot_digest.trim()
            } else {
                req.snapshot.digest.trim()
            })?;
            if digest.is_empty() {
                // encoding/json ontsnapt HTML en beide Unicode-regelscheiders ook buiten HTML.
                let json = recording.to_json()?;
                let mut hasher = spin_security::Sha256::new();
                for ch in json.chars() {
                    let mut bytes = [0; 4];
                    let encoded = match ch {
                        '<' => "\\u003c",
                        '>' => "\\u003e",
                        '&' => "\\u0026",
                        '\u{2028}' => "\\u2028",
                        '\u{2029}' => "\\u2029",
                        _ => ch.encode_utf8(&mut bytes),
                    };
                    hasher.update(encoded.as_bytes());
                }
                digest = try_string("sha256:")?;
                for byte in hasher.finish() {
                    d::try_push_str(&mut digest, &text(format_args!("{byte:02x}"))?)?;
                }
            }
            if req.snapshot.driver.is_empty() {
                req.snapshot.driver = try_string("journal")?;
            }
            req.snapshot.digest = digest.try_clone()?;
            let mut artifact = d::Artifact {
                id: try_string(context.id)?,
                kind: recording.kind.try_clone()?,
                name: recording.name.try_clone()?,
                scope: recording.scope.try_clone()?,
                subject: recording.subject.try_clone()?,
                profile: recording.profile.try_clone()?,
                provides: recording.provides.try_clone()?,
                requires: recording.requires.try_clone()?,
                enables: recording.enables.try_clone()?,
                slot: recording.slot.try_clone()?,
                parent_artifact_ids: recording.parent_artifact_ids.try_clone()?,
                snapshot_digest: digest,
                snapshot: req.snapshot,
                compatibility_fingerprint: recording.compatibility_fingerprint.try_clone()?,
                sensitivity: recording.sensitivity.try_clone()?,
                created_by: recording.actor.try_clone()?,
                created_at: context.now.try_clone()?,
                ..Default::default()
            };
            if let Some(previous) = state
                .artifacts
                .get_mut(&recording.replaces_artifact_id)
                .filter(|a| a.superseded_by.is_empty())
            {
                previous.superseded_by = artifact.id.try_clone()?;
                artifact.agent_options = previous.agent_options.try_clone()?;
                artifact.agent_settings = previous.agent_settings.try_clone()?;
                artifact.tracked_paths = previous.tracked_paths.try_clone()?;
                artifact.tracked_excludes = previous.tracked_excludes.try_clone()?;
            }
            state
                .artifacts
                .insert(artifact.id.try_clone()?, artifact.try_clone()?)?;
            let recording = editable(state, id, &req.actor)?;
            recording.status = try_string(d::RECORDING_COMPLETED)?;
            recording.artifact_id = artifact.id.try_clone()?;
            recording.ended_at = Some(context.now.try_clone()?);
            Ok(artifact)
        })
    }
    /// Annuleren bewaart het journal en maakt de actor vrij voor een nieuwe opname.
    pub fn cancel_recording(
        &mut self,
        id: &str,
        actor: &str,
        now: &Timestamp,
    ) -> Result<d::Recording> {
        self.edit(|state| {
            let r = editable(state, id, actor)?;
            r.status = try_string(d::RECORDING_CANCELLED)?;
            r.ended_at = Some(now.try_clone()?);
            Ok(r.try_clone()?)
        })
    }
    /// Leent één recording.
    pub fn recording(&self, id: &str) -> Result<&d::Recording> {
        self.state.recordings.get(id).ok_or(Error::NotFound)
    }
    /// Zoekt de huidige opname van een actor.
    pub fn open_recording(&self, actor: &str) -> Result<&d::Recording> {
        let actor = normalized(actor)?;
        self.state
            .recordings
            .iter()
            .find(|(_, r)| r.actor == actor && r.status == d::RECORDING_OPEN)
            .map(|(_, r)| r)
            .ok_or(Error::NotFound)
    }
    /// Een herstart hervat recordings waarvan de capsule nog niet beschikbaar was.
    pub fn starting_recordings(&self) -> Result<List<d::Recording>> {
        let mut sorted = Vec::new();
        for (_, r) in self.state.recordings.iter().filter(|(_, r)| {
            r.status == d::RECORDING_OPEN
                && r.runtime
                    .as_ref()
                    .is_none_or(|v| v.container_id.is_empty() || v.stop_pending)
        }) {
            d::try_push(&mut sorted, (r.started_at.time()?, r))?;
        }
        sorted.sort_unstable_by_key(|(time, _)| *time);
        let mut out = List::new();
        for (_, r) in sorted {
            out.push(r.try_clone()?)?;
        }
        Ok(out)
    }
    /// Alleen de actor van een nog open recording mag de runtime vastleggen.
    pub fn set_recording_runtime(
        &mut self,
        id: &str,
        actor: &str,
        runtime: d::CapsuleRuntime,
    ) -> Result<d::Recording> {
        self.edit(|state| {
            let r = editable(state, id, actor)?;
            r.runtime = Some(runtime);
            Ok(r.try_clone()?)
        })
    }
    /// Leent een artifactversie.
    pub fn artifact(&self, id: &str) -> Result<&d::Artifact> {
        self.state.artifacts.get(id).ok_or(Error::NotFound)
    }
    /// Gebruikersscope gaat voor project, team en global; daarna wint de nieuwste datum.
    pub fn latest_artifact(
        &self,
        kind: &str,
        name: &str,
        actor: &str,
        profile: &str,
    ) -> Result<&d::Artifact> {
        latest(
            &self.state,
            kind,
            &normalized(name)?,
            &normalized(actor)?,
            &normalized(profile)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Memory;
    use core::cell::Cell;
    fn now() -> Timestamp {
        Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap()
    }
    #[test]
    fn recording_versions_keep_parents_and_carry_layer_metadata() {
        let fail = Cell::new(false);
        let mut store = Store::new(PersistedState::default(), Memory(&fail));
        let req = || {
            d::CreateRecordingRequest::from_json(
                br#"{"actor":" DEREK ","kind":"tool","name":" CODEX "}"#,
            )
            .unwrap()
        };
        let first = store
            .create_recording(
                req(),
                Context {
                    now: &now(),
                    id: "r1",
                },
            )
            .unwrap();
        assert_eq!(first.scope, "global");
        assert_eq!(first.sensitivity, "public");
        assert_eq!(&*first.provides, &[String::from("tool:codex")]);
        assert!(matches!(
            store.create_recording(
                req(),
                Context {
                    now: &now(),
                    id: "r2"
                }
            ),
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            store.record_execution("r1", "other", Some(0), &now()),
            Err(Error::Conflict(_))
        ));
        store
            .record_execution("r1", "derek", Some(0), &now())
            .unwrap();
        let end = || {
            d::EndRecordingRequest::from_json(
                br#"{"actor":"derek","snapshot":{"driver":"docker","digest":"sha256:a"}}"#,
            )
            .unwrap()
        };
        store
            .end_recording(
                "r1",
                end(),
                Context {
                    now: &now(),
                    id: "a1",
                },
            )
            .unwrap();
        let old = store.state.artifacts.get_mut("a1").unwrap();
        old.tracked_paths.push("/auth/".into()).unwrap();
        old.agent_settings = Some(d::AgentSettings {
            model: "model-a".into(),
            ..Default::default()
        });
        let mut edit = req();
        edit.replaces_artifact_id = "a1".into();
        edit.parent_artifact_ids.push("a1".into()).unwrap();
        store
            .create_recording(
                edit,
                Context {
                    now: &now(),
                    id: "r2",
                },
            )
            .unwrap();
        fail.set(true);
        assert_eq!(
            store
                .end_recording(
                    "r2",
                    end(),
                    Context {
                        now: &now(),
                        id: "a2"
                    }
                )
                .unwrap_err(),
            Error::Storage(10)
        );
        assert!(store.artifact("a1").unwrap().superseded_by.is_empty());
        assert!(store.artifact("a2").is_err());
        assert_eq!(store.recording("r2").unwrap().status, d::RECORDING_OPEN);
        fail.set(false);
        let second = store
            .end_recording(
                "r2",
                end(),
                Context {
                    now: &now(),
                    id: "a2",
                },
            )
            .unwrap();
        assert_eq!(store.artifact("a1").unwrap().superseded_by, "a2");
        assert_eq!(store.artifact("a1").unwrap().snapshot.digest, "sha256:a");
        assert_eq!(second.agent_settings.unwrap().model, "model-a");
        assert_eq!(&*second.tracked_paths, &[String::from("/auth/")]);
        assert_eq!(
            store
                .latest_artifact("tool", "codex", "derek", "")
                .unwrap()
                .id,
            "a2"
        );
    }
    #[test]
    fn selection_and_recording_access_respect_scope() {
        let fail = Cell::new(false);
        let state=PersistedState::from_json(br#"{"artifacts":{
          "global":{"id":"global","name":"codex","kind":"tool","scope":"global","created_at":"2026-09-30T12:00:00Z"},
          "private":{"id":"private","name":"codex","kind":"tool","scope":"user","subject":"derek","created_at":"2020-01-01T00:00:00Z"}
        }}"#).unwrap();
        let mut store = Store::new(state, Memory(&fail));
        assert_eq!(
            store
                .latest_artifact("tool", "codex", "derek", "")
                .unwrap()
                .id,
            "private"
        );
        assert_eq!(
            store
                .latest_artifact("tool", "codex", "other", "")
                .unwrap()
                .id,
            "global"
        );
        let req = d::CreateRecordingRequest::from_json(
            br#"{"actor":"other","kind":"tool","name":"node","parent_artifact_ids":["private"]}"#,
        )
        .unwrap();
        assert_eq!(
            store
                .create_recording(
                    req,
                    Context {
                        now: &now(),
                        id: "r1"
                    }
                )
                .unwrap_err(),
            Error::NotFound
        );
        assert_eq!(store.version(), 0);
    }
}
