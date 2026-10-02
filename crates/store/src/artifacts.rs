//! Versie-afstamming, metadata en opruiming van artifacts.
use crate::{Error, Persistence, Result, Store, layer_key};
use alloc::{string::String, vec::Vec};
use spin_core::validation::{clean_tracked_paths, normalized};
use spin_domain::{self as d, List, Map, Timestamp, TryClone, state::PersistedState, try_string};

fn in_use(state: &PersistedState, id: &str) -> bool {
    state.compositions.iter().any(|(_, c)| {
        c.runtime.as_ref().is_some_and(|r| r.status != "stopped")
            && (c.layers.iter().any(|layer| layer == id)
                || c.resolved_artifacts.iter().any(|r| r.artifact_id == id))
    }) || state.recordings.iter().any(|(_, r)| {
        r.status == d::RECORDING_OPEN && r.parent_artifact_ids.iter().any(|p| p == id)
    })
}
fn tree(state: &PersistedState, id: &str) -> Result<List<d::Artifact>> {
    // Postorder met een expliciete stack: ook een corrupte cyclus vult geen CPU-stack.
    let mut out = List::new();
    let mut seen = Map::new();
    let mut stack = Vec::new();
    d::try_push(&mut stack, (id, false))?;
    while let Some((id, finish)) = stack.pop() {
        if finish {
            if let Some(a) = state.artifacts.get(id) {
                out.push(a.try_clone()?)?;
            }
            continue;
        }
        if seen.contains_key(id) {
            continue;
        }
        seen.insert(try_string(id)?, true)?;
        d::try_push(&mut stack, (id, true))?;
        // Directe versieverbindingen vormen dezelfde transitieve lineage als Go.
        for (candidate, a) in state.artifacts.iter() {
            if seen.contains_key(candidate) {
                continue;
            }
            if a.parent_artifact_ids.iter().any(|p| p == id)
                || a.superseded_by == id
                || state
                    .artifacts
                    .get(id)
                    .is_some_and(|old| old.superseded_by == candidate)
            {
                d::try_push(&mut stack, (candidate, false))?;
            }
        }
    }
    Ok(out)
}
fn prepare(
    state: &PersistedState,
    id: &str,
    operator: &str,
    admin: bool,
) -> Result<List<d::Artifact>> {
    let artifact = state.artifacts.get(id).ok_or(Error::NotFound)?;
    let operator = normalized(operator)?;
    if operator.is_empty() {
        return Err(Error::Conflict("operator is required"));
    }
    if !admin && !artifact.created_by.is_empty() && artifact.created_by != operator {
        return Err(Error::Conflict(
            "only the recorder or an admin can remove this layer",
        ));
    }
    let members = tree(state, id)?;
    if members.iter().any(|a| in_use(state, &a.id)) {
        return Err(Error::Conflict(
            "layer is used by an open recording or running composition",
        ));
    }
    Ok(members)
}
impl<P: Persistence> Store<P> {
    /// De laag en alle versies en afgeleiden, kinderen vóór hun parents.
    pub fn artifact_tree(&self, id: &str) -> Result<List<d::Artifact>> {
        tree(&self.state, id)
    }
    /// Controle vóór het opruimen van externe snapshots; verwijderen controleert opnieuw.
    pub fn prepare_artifact_deletion(
        &self,
        id: &str,
        operator: &str,
        admin: bool,
    ) -> Result<d::Artifact> {
        prepare(&self.state, id, operator, admin)?;
        Ok(self.artifact(id)?.try_clone()?)
    }
    /// Verwijdert de hele boom met verwijzingen en logins in één duurzame mutatie.
    pub fn delete_artifact_tree(
        &mut self,
        id: &str,
        operator: &str,
        admin: bool,
    ) -> Result<List<d::Artifact>> {
        self.edit(|state| {
            let members = prepare(state, id, operator, admin)?;
            let mut removed_compositions = Map::new();
            for a in members.iter() {
                if !a.snapshot.digest.is_empty() {
                    crate::blobs::queue_garbage(
                        state,
                        &spin_core::validation::text(format_args!(
                            "snapshot:{}",
                            a.snapshot.digest
                        ))?,
                    )?;
                }
                crate::blobs::queue_garbage(
                    state,
                    &spin_core::validation::text(format_args!("manifest:artifact:{}", a.id))?,
                )?;
                state.artifacts.remove(&a.id);
                state.recordings.retain(|_, r| r.artifact_id != a.id);
                for (id, c) in state.compositions.iter() {
                    if c.layers.contains(&a.id)
                        || c.resolved_artifacts.iter().any(|r| r.artifact_id == a.id)
                    {
                        removed_compositions.insert(try_string(id)?, true)?;
                    }
                }
            }
            state
                .compositions
                .retain(|id, _| !removed_compositions.contains_key(id));
            for (_, session) in state.sessions.iter_mut() {
                if removed_compositions.contains_key(&session.prepared_composition_id) {
                    session.prepared_composition_id.clear();
                }
            }
            let mut remaining = Map::new();
            for (_, a) in state.artifacts.iter() {
                remaining.insert(layer_key(a)?, true)?;
            }
            state.logins.retain(|_, l| remaining.contains_key(&l.key));
            Ok(members)
        })
    }
    /// Verwijdert een boom en geeft de gevraagde basislaag terug.
    pub fn delete_artifact(&mut self, id: &str, operator: &str) -> Result<d::Artifact> {
        let root = self.artifact(id)?.try_clone()?;
        self.delete_artifact_tree(id, operator, false)?;
        Ok(root)
    }
    /// Legt een later opgehaald manifest vast.
    pub fn set_artifact_contents(
        &mut self,
        id: &str,
        contents: d::LayerContents,
    ) -> Result<d::Artifact> {
        self.edit(|state| {
            let a = state.artifacts.get_mut(id).ok_or(Error::NotFound)?;
            a.snapshot.contents = Some(contents);
            Ok(a.try_clone()?)
        })
    }
    /// Wijzigt het entrypoint van een capability die de laag daadwerkelijk ENABLES.
    pub fn set_artifact_enablement_command(
        &mut self,
        id: &str,
        name: &str,
        command: &str,
    ) -> Result<d::Artifact> {
        let name = normalized(name)?;
        let command = command.trim();
        if command.is_empty() {
            return Err(Error::Conflict("command is required"));
        }
        self.edit(|state| {
            let a = state.artifacts.get_mut(id).ok_or(Error::NotFound)?;
            let enabled = a
                .enables
                .as_mut_slice()
                .iter_mut()
                .find(|e| e.name == name)
                .ok_or(Error::Conflict("layer does not ENABLE this capability"))?;
            enabled.command = try_string(command)?;
            if name == "acp" && enabled.transport.is_empty() {
                enabled.transport = try_string("stdio")?;
            }
            Ok(a.try_clone()?)
        })
    }
    /// Excludes gelden uitsluitend binnen een expliciet gevolgde folder.
    pub fn set_artifact_tracked(
        &mut self,
        id: &str,
        paths: &[String],
        excludes: &[String],
    ) -> Result<d::Artifact> {
        let tracked = clean_tracked_paths(paths)?;
        let mut kept = List::default();
        for exclude in clean_tracked_paths(excludes)?.into_vec() {
            if tracked.iter().any(|folder| {
                d::tracked_folder(folder) && exclude.starts_with(folder) && exclude != *folder
            }) {
                kept.push(exclude)?;
            }
        }
        self.edit(|state| {
            let a = state.artifacts.get_mut(id).ok_or(Error::NotFound)?;
            a.tracked_paths = tracked;
            a.tracked_excludes = kept;
            Ok(a.try_clone()?)
        })
    }
    /// Exclude and remove captured copies in one commit, so a watcher cannot restore them.
    pub fn exclude_artifact_login_path(
        &mut self,
        id: &str,
        path: &str,
        now: &Timestamp,
    ) -> Result<(d::Artifact, usize)> {
        let path = try_string(path.trim())?;
        let mut input = List::new();
        input.push(path.try_clone()?)?;
        let normalized = clean_tracked_paths(input.as_slice())?;
        if normalized.len() != 1 || normalized[0] != path {
            return Err(Error::Conflict("invalid tracked path"));
        }
        self.edit(|state| {
            let artifact = state.artifacts.get_mut(id).ok_or(Error::NotFound)?;
            if !artifact.tracked_paths.iter().any(|folder| {
                d::tracked_folder(folder) && path.starts_with(folder) && path != *folder
            }) {
                return Err(Error::Conflict(
                    "path is outside the layer's tracked folders",
                ));
            }
            if !artifact.tracked_excludes.contains(&path) {
                artifact.tracked_excludes.push(path.try_clone()?)?;
            }
            let key = layer_key(artifact)?;
            let artifact = artifact.try_clone()?;
            let mut removed = 0;
            for (_, login) in state.logins.iter_mut().filter(|(_, l)| l.key == key) {
                let before = login.files.len();
                login.files.retain(|p, _| {
                    p != path && !(d::tracked_folder(&path) && p.starts_with(&path))
                });
                if login.files.len() != before {
                    removed += before - login.files.len();
                    login.updated_at = now.try_clone()?;
                }
            }
            Ok((artifact, removed))
        })
    }
    /// Normaliseert agentinstellingen; een lege instelling verwijdert de override.
    pub fn set_artifact_agent_settings(
        &mut self,
        id: &str,
        mut settings: d::AgentSettings,
    ) -> Result<d::Artifact> {
        settings.mode = try_string(settings.mode.trim())?;
        settings.model = try_string(settings.model.trim())?;
        settings.reasoning_effort = try_string(settings.reasoning_effort.trim())?;
        self.edit(|state| {
            let a = state.artifacts.get_mut(id).ok_or(Error::NotFound)?;
            a.agent_settings = if settings == d::AgentSettings::default() {
                None
            } else {
                Some(settings)
            };
            Ok(a.try_clone()?)
        })
    }
    /// Bewaart eerdere opties terwijl een nieuw verzoek loopt.
    pub fn mark_artifact_agent_options_fetching(&mut self, id: &str, fetching: bool) -> Result {
        self.edit(|state| {
            let a = state.artifacts.get_mut(id).ok_or(Error::NotFound)?;
            a.agent_options.get_or_insert_default().fetching = fetching;
            Ok(())
        })
    }
    /// Slaat gerapporteerde opties op met een expliciete opvraagtijd.
    pub fn set_artifact_agent_options(
        &mut self,
        id: &str,
        mut options: d::AgentOptions,
        now: &Timestamp,
    ) -> Result<d::Artifact> {
        if options.fetched_at.time()?.is_zero() {
            options.fetched_at = now.try_clone()?;
        }
        self.edit(|state| {
            let a = state.artifacts.get_mut(id).ok_or(Error::NotFound)?;
            a.agent_options = Some(options);
            Ok(a.try_clone()?)
        })
    }
    /// Een replica hoort bij een bekende runner; herhaalde meldingen zijn idempotent.
    pub fn add_snapshot_replica(&mut self, id: &str, client: &str) -> Result<d::Artifact> {
        let artifact = self.artifact(id)?;
        self.client(client)?;
        if artifact.snapshot.client_id == client
            || artifact
                .snapshot
                .replica_client_ids
                .iter()
                .any(|c| c == client)
        {
            return Ok(artifact.try_clone()?);
        }
        self.edit(|state| {
            let a = state.artifacts.get_mut(id).ok_or(Error::NotFound)?;
            a.snapshot.replica_client_ids.push(try_string(client)?)?;
            a.snapshot.replica_client_ids.as_mut_slice().sort_unstable();
            Ok(a.try_clone()?)
        })
    }
    /// Alleen ongebruikte, ongedeeelde snapshots zonder afhankelijke delta mogen weg.
    pub fn prunable_artifacts(&self) -> Result<List<d::Artifact>> {
        let mut out = List::new();
        for (_, a) in self.state.artifacts.iter() {
            if a.superseded_by.is_empty()
                || a.snapshot_pruned_at.is_some()
                || a.snapshot.digest.is_empty()
                || in_use(&self.state, &a.id)
            {
                continue;
            }
            if self.state.artifacts.iter().any(|(_, other)| {
                other.snapshot_pruned_at.is_none()
                    && ((other.id != a.id && other.snapshot.digest == a.snapshot.digest)
                        || (!a.snapshot.r#ref.is_empty()
                            && other.snapshot.delta
                            && other.snapshot.parent_ref == a.snapshot.r#ref))
            }) {
                continue;
            }
            out.push(a.try_clone()?)?;
        }
        out.as_mut_slice().sort_unstable_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
    /// Bevestigt pas na het verwijderen van het externe object dat de snapshot weg is.
    pub fn mark_snapshot_pruned(&mut self, id: &str, now: &Timestamp) -> Result<d::Artifact> {
        self.edit(|state| {
            let a = state.artifacts.get_mut(id).ok_or(Error::NotFound)?;
            a.snapshot_pruned_at = Some(now.try_clone()?);
            Ok(a.try_clone()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Memory;
    use core::cell::Cell;
    use d::Wire;
    #[test]
    fn tree_removal_covers_old_versions_descendants_and_releases_references_atomically() {
        let fail = Cell::new(false);
        let state = PersistedState::from_json(
            br#"{
          "artifacts":{
            "a1":{"id":"a1","kind":"tool","name":"node","superseded_by":"a2"},
            "a2":{"id":"a2","kind":"tool","name":"node","parent_artifact_ids":["a1"]},
            "child":{"id":"child","kind":"tool","name":"app","parent_artifact_ids":["a1"]},
            "keep":{"id":"keep","kind":"tool","name":"keep"}},
          "compositions":{"c":{"id":"c","resolved_artifacts":[{"artifact_id":"child"}]}},
          "sessions":{"s":{"id":"s","prepared_composition_id":"c"}},
          "logins":{"gone":{"key":"/tool:node"},"kept":{"key":"/tool:keep"}}
        }"#,
        )
        .unwrap();
        let mut store = Store::new(state, Memory(&fail));
        assert_eq!(store.artifact_tree("a2").unwrap().len(), 3);
        fail.set(true);
        assert_eq!(
            store
                .delete_artifact_tree("a2", "derek", false)
                .unwrap_err(),
            Error::Storage(10)
        );
        assert_eq!(store.state.artifacts.len(), 4);
        fail.set(false);
        let removed = store.delete_artifact_tree("a2", "derek", false).unwrap();
        assert_eq!(removed.len(), 3);
        assert_eq!(removed.last().unwrap().id, "a2");
        assert_eq!(store.state.artifacts.len(), 1);
        assert!(store.state.compositions.is_empty());
        assert!(
            store
                .state
                .sessions
                .get("s")
                .unwrap()
                .prepared_composition_id
                .is_empty()
        );
        assert!(store.state.logins.get("gone").is_none());
        assert!(store.state.logins.get("kept").is_some());
    }
    #[test]
    fn pruning_respects_shared_blobs_delta_parents_and_live_users() {
        let fail = Cell::new(false);
        let state = PersistedState::from_json(
            br#"{"artifacts":{
          "a1":{"id":"a1","superseded_by":"a2","snapshot":{"digest":"old","ref":"base"}},
          "a2":{"id":"a2","snapshot":{"digest":"new","delta":true,"parent_ref":"base"}}
        }}"#,
        )
        .unwrap();
        let mut store = Store::new(state, Memory(&fail));
        assert!(store.prunable_artifacts().unwrap().is_empty());
        store.state.artifacts.get_mut("a2").unwrap().snapshot.delta = false;
        assert_eq!(store.prunable_artifacts().unwrap()[0].id, "a1");
        store.state.artifacts.get_mut("a2").unwrap().snapshot.digest = "old".into();
        assert!(store.prunable_artifacts().unwrap().is_empty());
        store.state.artifacts.get_mut("a2").unwrap().snapshot.digest = "new".into();
        store
            .state
            .recordings
            .insert(
                "r".into(),
                d::Recording::from_json(br#"{"status":"recording","parent_artifact_ids":["a1"]}"#)
                    .unwrap(),
            )
            .unwrap();
        assert!(store.prunable_artifacts().unwrap().is_empty());
        assert!(matches!(
            store.delete_artifact_tree("a2", "derek", true),
            Err(Error::Conflict(_))
        ));
    }
}
