//! Runneridentiteit en capsule-affiniteit overleven een verbroken verbinding.
use crate::{Context, Error, Persistence, Result, Store};
use spin_domain::{self as d, TryClone, try_string};

impl<P: Persistence> Store<P> {
    /// Bij boot bestaan nog geen sockets. Onafgebouwde compositions worden weggehaald
    /// zodat hun loginreserveringen en Session-verwijzingen opnieuw te plaatsen zijn.
    pub fn recover_runtime_status(&mut self, now: &d::Timestamp) -> Result {
        let mut unbuilt = d::List::new();
        for (id, c) in self.state.compositions.iter() {
            if c.runtime.is_none() {
                unbuilt.push(try_string(id)?)?;
            }
        }
        let probes = self.state.compositions.iter().any(|(_, c)| {
            !c.probe_artifact_id.is_empty()
                && c.runtime.as_ref().is_some_and(|r| r.status != "stopped")
        }) || self
            .state
            .artifacts
            .iter()
            .any(|(_, a)| a.agent_options.as_ref().is_some_and(|o| o.fetching));
        if !probes
            && unbuilt.is_empty()
            && !self
                .state
                .clients
                .iter()
                .any(|(_, c)| c.status != "offline")
        {
            return Ok(());
        }
        self.edit(|state| {
            for id in unbuilt.iter() {
                discard(state, id, now)?;
            }
            for (_, composition) in state.compositions.iter_mut() {
                if !composition.probe_artifact_id.is_empty()
                    && let Some(capsule) = composition
                        .runtime
                        .as_mut()
                        .filter(|r| r.status != "stopped")
                {
                    capsule.stop_pending = true;
                }
            }
            for (_, artifact) in state.artifacts.iter_mut() {
                if let Some(options) = artifact.agent_options.as_mut().filter(|o| o.fetching) {
                    options.fetching = false;
                    options.error =
                        try_string("agent options probe interrupted by server restart")?;
                    options.fetched_at = now.try_clone()?;
                }
            }
            for (_, client) in state.clients.iter_mut() {
                client.status = try_string("offline")?;
            }
            Ok(())
        })
    }
    /// The temporary capsule remains identifiable even when its in-memory probe is lost.
    pub fn mark_options_probe(&mut self, composition: &str, artifact: &str) -> Result {
        self.edit(|state| {
            state
                .compositions
                .get_mut(composition)
                .ok_or(Error::NotFound)?
                .probe_artifact_id = try_string(artifact)?;
            Ok(())
        })
    }
    /// De bearer capability blijft versleuteld in dezelfde duurzame staat als de runners.
    pub fn worker_token(&self) -> &str {
        &self.state.worker_token
    }
    /// De bestaande token wint van een oude deployment-seed, ook na herstart.
    pub fn ensure_worker_token(&mut self, seed: &str) -> Result {
        if !self.state.worker_token.is_empty() {
            return Ok(());
        }
        self.replace_worker_token(seed)
    }
    /// De runtime levert de willekeur; een mislukte opslag trekt de oude token niet in.
    pub fn replace_worker_token(&mut self, token: &str) -> Result {
        let token = token.trim();
        if token.is_empty() || token.len() > 4096 {
            return Err(Error::Conflict("invalid worker token"));
        }
        self.edit(|state| {
            state.worker_token = try_string(token)?;
            Ok(())
        })
    }
    /// Leent de identiteit zonder een tijdelijke snapshot van alle geheimen te maken.
    pub fn client_by_instance(&self, instance: &str) -> Option<&d::Client> {
        self.state
            .clients
            .iter()
            .find(|(_, c)| c.instance_id == instance.trim())
            .map(|(_, c)| c)
    }
    /// Een herkenbare instance meldt zich opnieuw als dezelfde Client aan.
    pub fn register_client(
        &mut self,
        req: d::RegisterClientRequest,
        context: Context<'_>,
    ) -> Result<d::Client> {
        context.validate()?;
        if req.name.trim().is_empty() {
            return Err(Error::Conflict("client name is required"));
        }
        self.edit(|state| {
            let instance = req.instance_id.trim();
            let existing = state
                .clients
                .iter()
                .find(|(_, c)| {
                    (!instance.is_empty() && c.instance_id == instance)
                        || (c.instance_id.is_empty() && c.name == req.name)
                })
                .map(|(_, c)| c.try_clone())
                .transpose()?;
            let mut client = match existing {
                Some(client) => client,
                None => {
                    if state.clients.get(context.id).is_some() {
                        return Err(Error::Conflict("client id already exists"));
                    }
                    d::Client {
                        id: try_string(context.id)?,
                        created_at: context.now.try_clone()?,
                        ..Default::default()
                    }
                }
            };
            client.instance_id = try_string(instance)?;
            client.name = try_string(req.name.trim())?;
            client.capabilities = req.capabilities;
            client.status = try_string(if client.draining {
                "draining"
            } else {
                "online"
            })?;
            client.last_seen_at = context.now.try_clone()?;
            state
                .clients
                .insert(client.id.try_clone()?, client.try_clone()?)?;
            Ok(client)
        })
    }
    /// Leent de duurzame identiteit van een runner.
    pub fn client(&self, id: &str) -> Result<&d::Client> {
        self.state.clients.get(id.trim()).ok_or(Error::NotFound)
    }
    /// Drain wijzigt plaatsing, zonder affiniteit van bestaande Sessions te verliezen.
    pub fn set_client_draining(
        &mut self,
        id: &str,
        draining: bool,
        connected: bool,
    ) -> Result<d::Client> {
        self.edit(|state| {
            let client = state.clients.get_mut(id.trim()).ok_or(Error::NotFound)?;
            client.draining = draining;
            client.status = try_string(if !connected {
                "offline"
            } else if draining {
                "draining"
            } else {
                "online"
            })?;
            Ok(client.try_clone()?)
        })
    }
    /// Alleen een bevestigde runtime-stop geeft een capsule en haar agent vrij.
    pub fn set_composition_runtime(
        &mut self,
        id: &str,
        operator: &str,
        runtime: d::CapsuleRuntime,
    ) -> Result<d::Composition> {
        let operator = spin_core::validation::normalized(operator)?;
        self.edit(|state| {
            let composition = state.compositions.get_mut(id).ok_or(Error::NotFound)?;
            if composition.operator != operator {
                return Err(Error::Conflict("composition belongs to another operator"));
            }
            if runtime.status == "stopped" {
                composition.agent = None;
            }
            composition.runtime = Some(runtime);
            Ok(composition.try_clone()?)
        })
    }
    /// De ontvangen capsule en de Session-affiniteit worden samen gepubliceerd.
    /// Een laat runnerantwoord kan een gesloten of vervangen Session niet heropenen.
    pub fn finish_composition(
        &mut self,
        id: &str,
        operator: &str,
        runtime: d::CapsuleRuntime,
        now: &d::Timestamp,
    ) -> Result<d::Composition> {
        self.edit(|state| {
            let composition = state.compositions.get(id).ok_or(Error::NotFound)?;
            if composition.operator != operator {
                return Err(Error::Conflict("composition belongs to another operator"));
            }
            if state.clients.get(&runtime.client_id).is_none() {
                return Err(Error::NotFound);
            }
            if !composition.session_id.is_empty() {
                let session = state
                    .sessions
                    .get_mut(&composition.session_id)
                    .ok_or(Error::NotFound)?;
                if session.prepared_composition_id != id
                    || matches!(
                        session.status.as_str(),
                        d::SESSION_CANCELLED | d::SESSION_COMPLETED
                    )
                    || (!session.client_id.is_empty() && session.client_id != runtime.client_id)
                {
                    return Err(Error::Conflict("Session capsule was replaced or closed"));
                }
                session.client_id = runtime.client_id.try_clone()?;
                session.updated_at = now.try_clone()?;
            }
            let composition = state.compositions.get_mut(id).ok_or(Error::NotFound)?;
            composition.runtime = Some(runtime);
            Ok(composition.try_clone()?)
        })
    }
    /// Leent de Session voor plaatsing zonder een snapshot van de rest van de graaf.
    pub fn session(&self, id: &str) -> Result<&d::Session> {
        self.state.sessions.get(id).ok_or(Error::NotFound)
    }
    /// Een oude stream kan de inmiddels vervangen agent niet meer verwijderen.
    pub fn set_composition_agent(
        &mut self,
        id: &str,
        stream: &str,
        agent: Option<d::AgentProcess>,
    ) -> Result {
        if agent.is_none() {
            let composition = self.state.compositions.get(id).ok_or(Error::NotFound)?;
            if composition
                .agent
                .as_ref()
                .is_none_or(|a| a.stream_id != stream)
            {
                return Ok(());
            }
        }
        self.edit(|state| {
            let composition = state.compositions.get_mut(id).ok_or(Error::NotFound)?;
            if agent.is_some()
                && composition
                    .runtime
                    .as_ref()
                    .is_none_or(|r| r.status == "stopped")
            {
                return Err(Error::Conflict("an agent runs in a capsule that runs"));
            }
            composition.agent = agent;
            Ok(())
        })
    }
    /// De runner bevestigt welke capsules nog bestaan; alleen ontbrekende stoppen.
    pub fn reconcile_client_capsules(
        &mut self,
        client_id: &str,
        compositions: &[alloc::string::String],
        recordings: &[alloc::string::String],
    ) -> Result<usize> {
        self.edit(|state| {
            let mut count = 0;
            for (id, composition) in state.compositions.iter_mut() {
                if let Some(runtime) = composition.runtime.as_mut()
                    && runtime.client_id == client_id
                    && runtime.status != "stopped"
                    && !compositions.iter().any(|s| s == id)
                {
                    runtime.status = try_string("stopped")?;
                    runtime.stop_pending = false;
                    composition.agent = None;
                    count += 1;
                }
            }
            for (id, recording) in state.recordings.iter_mut() {
                if let Some(runtime) = recording.runtime.as_mut()
                    && runtime.client_id == client_id
                    && runtime.status != "stopped"
                    && !recordings.iter().any(|s| s == id)
                {
                    runtime.status = try_string("stopped")?;
                    runtime.stop_pending = false;
                    count += 1;
                }
            }
            Ok(count)
        })
    }
    /// Capsules zonder eigenaar nemen onterecht runnerplaatsen in. Een capsule
    /// zonder runtime is nog in aanbouw. Een andere runner kan dezelfde Docker
    /// daemon zien: diens capsules zijn geen bewijs van een verweesde kopie.
    pub fn orphan_capsules(
        &self,
        client: &str,
        compositions: &[alloc::string::String],
        recordings: &[alloc::string::String],
    ) -> Result<d::protocol::RemoveCapsulesPayload> {
        let mut orphan = d::protocol::RemoveCapsulesPayload::default();
        for id in compositions {
            let keep = self.state.compositions.get(id).is_some_and(|c| {
                c.runtime
                    .as_ref()
                    .is_none_or(|r| r.client_id != client || r.status != "stopped")
            });
            if !keep {
                orphan.compositions.push(try_string(id)?)?;
            }
        }
        for id in recordings {
            let keep = self.state.recordings.get(id).is_some_and(|c| {
                c.runtime.as_ref().is_some_and(|r| r.client_id != client)
                    || (c.status == d::RECORDING_OPEN
                        && c.runtime.as_ref().is_none_or(|r| r.status != "stopped"))
            });
            if !keep {
                orphan.recordings.push(try_string(id)?)?;
            }
        }
        Ok(orphan)
    }
}

fn busy(state: &d::state::PersistedState, id: &str) -> bool {
    state.sessions.iter().any(|(_, s)| {
        s.client_id == id
            && !matches!(
                s.status.as_str(),
                d::SESSION_COMPLETED | d::SESSION_CANCELLED
            )
    }) || state.compositions.iter().any(|(_, c)| {
        c.runtime
            .as_ref()
            .is_some_and(|r| r.client_id == id && r.status != "stopped")
    }) || state.recordings.iter().any(|(_, r)| {
        r.runtime
            .as_ref()
            .is_some_and(|r| r.client_id == id && r.status != "stopped")
    })
}

impl<P: Persistence> Store<P> {
    /// Een verbinding verandert alleen aanwezigheid; Session-affiniteit blijft staan.
    pub fn set_client_status(
        &mut self,
        id: &str,
        status: &str,
        now: &d::Timestamp,
    ) -> Result<d::Client> {
        self.edit(|state| {
            let c = state.clients.get_mut(id).ok_or(Error::NotFound)?;
            c.status = try_string(status.trim())?;
            c.last_seen_at = now.try_clone()?;
            Ok(c.try_clone()?)
        })
    }
    /// Vergeten is alleen toegestaan voor offline runners zonder Sessions of capsules.
    pub fn remove_client(&mut self, id: &str) -> Result {
        self.edit(|state| {
            let c = state.clients.get(id.trim()).ok_or(Error::NotFound)?;
            if matches!(c.status.as_str(), "online" | "draining") || busy(state, &c.id) {
                return Err(Error::Conflict("runner is online or still owns work"));
            }
            let id = c.id.try_clone()?;
            state.clients.remove(&id);
            Ok(())
        })
    }
    /// Ruimt ongebruikte offline runners op vóór de expliciete afkapdatum.
    pub fn prune_clients(
        &mut self,
        cutoff: &d::Timestamp,
    ) -> Result<d::List<alloc::string::String>> {
        let cutoff = cutoff.time()?;
        let mut removed = d::List::new();
        for (id, c) in self.state.clients.iter() {
            if !matches!(c.status.as_str(), "online" | "draining")
                && c.last_seen_at.time()? < cutoff
                && !busy(&self.state, id)
            {
                removed.push(try_string(id)?)?;
            }
        }
        removed.as_mut_slice().sort_unstable();
        if removed.is_empty() {
            return Ok(removed);
        }
        self.edit(|state| {
            for id in removed.iter() {
                state.clients.remove(id);
            }
            Ok(removed)
        })
    }
    /// Een eenmaal gepinde Session kan niet door een reconnect van runner wisselen.
    pub fn bind_session_client(
        &mut self,
        session: &str,
        client: &str,
        now: &d::Timestamp,
    ) -> Result<d::Session> {
        self.edit(|state| {
            if state.clients.get(client).is_none() {
                return Err(Error::NotFound);
            }
            let s = state.sessions.get_mut(session).ok_or(Error::NotFound)?;
            if !s.client_id.is_empty() && s.client_id != client {
                return Err(Error::Conflict(
                    "Session is already pinned to another client",
                ));
            }
            s.client_id = try_string(client)?;
            s.updated_at = now.try_clone()?;
            Ok(s.try_clone()?)
        })
    }
    /// Leent een Composition zonder de rest van de state te kopiëren.
    pub fn composition(&self, id: &str) -> Result<&d::Composition> {
        self.state.compositions.get(id).ok_or(Error::NotFound)
    }
    /// Leent de capsules met een live runtime; een sweep die alleen leest, kopieert niets.
    pub fn running_compositions_ref(&self) -> impl Iterator<Item = &d::Composition> {
        self.state
            .compositions
            .iter()
            .map(|(_, c)| c)
            .filter(|c| c.runtime.as_ref().is_some_and(|r| r.status != "stopped"))
    }
    /// Een kopie voor aanroepers die tijdens de lus zelf de Store muteren.
    pub fn running_compositions(&self) -> Result<d::List<d::Composition>> {
        let mut out = d::List::new();
        for c in self.running_compositions_ref() {
            out.push(c.try_clone()?)?;
        }
        Ok(out)
    }
    /// Bewaart het laatst opgehaalde filesystemverschil buiten de workspace: de
    /// samenvatting in de state, de bestandslijst als blob
    /// `manifest:composition:<id>` (die kan megabytes zijn). Een ongewijzigd
    /// verschil schrijft niets.
    pub fn set_composition_changes(&mut self, id: &str, mut changes: d::LayerContents) -> Result {
        let current = self.composition(id)?.capsule_changes.as_ref();
        // Een lege lijst wist de blob alleen als het verschil nu echt leeg is
        // en er eerder een lijst stond; anders blijft een losse lijst staan.
        let listed = current.is_some_and(|c| c.files > 0 || !c.entries.is_empty());
        let entries = core::mem::take(&mut changes.entries);
        if !entries.is_empty() || (listed && changes.files == 0) {
            self.put_manifest(
                &spin_core::validation::text(format_args!("manifest:composition:{id}"))?,
                &entries,
            )?;
        }
        if self.composition(id)?.capsule_changes.as_ref() == Some(&changes) {
            return Ok(());
        }
        self.edit(|state| {
            state
                .compositions
                .get_mut(id)
                .ok_or(Error::NotFound)?
                .capsule_changes = Some(changes);
            Ok(())
        })
    }
    /// Eenmalig na het laden: bestandslijsten die nog in de state staan, gaan
    /// naar hun blob. Daarna draagt de state alleen de samenvatting.
    pub fn externalize_composition_changes(&mut self) -> Result<usize> {
        let mut ids = d::List::new();
        for (id, c) in self.state.compositions.iter() {
            if c.capsule_changes
                .as_ref()
                .is_some_and(|x| !x.entries.is_empty())
            {
                ids.push(try_string(id)?)?;
            }
        }
        if ids.is_empty() {
            return Ok(0);
        }
        for id in ids.iter() {
            let entries = self
                .composition(id)?
                .capsule_changes
                .as_ref()
                .map(|c| c.entries.try_clone())
                .transpose()?
                .unwrap_or_default();
            self.put_manifest(
                &spin_core::validation::text(format_args!("manifest:composition:{id}"))?,
                &entries,
            )?;
        }
        self.edit(|state| {
            for id in ids.iter() {
                if let Some(changes) = state
                    .compositions
                    .get_mut(id)
                    .and_then(|c| c.capsule_changes.as_mut())
                {
                    changes.entries = d::List::new();
                }
            }
            Ok(ids.len())
        })
    }
    /// Na herstart houden onafgebouwde capsules geen loginpool meer bezet.
    pub fn discard_unbuilt_compositions(&mut self, now: &d::Timestamp) -> Result<usize> {
        let mut ids = d::List::new();
        for (id, c) in self.state.compositions.iter() {
            if c.runtime.is_none() {
                ids.push(try_string(id)?)?;
            }
        }
        if ids.is_empty() {
            return Ok(0);
        }
        self.edit(|state| {
            for id in ids.iter() {
                discard(state, id, now)?;
            }
            Ok(ids.len())
        })
    }
    /// Alleen een eigen Composition zonder runtime kan worden weggegooid.
    pub fn discard_composition(&mut self, id: &str, operator: &str, now: &d::Timestamp) -> Result {
        let operator = spin_core::validation::normalized(operator)?;
        self.edit(|state| {
            let c = state.compositions.get(id).ok_or(Error::NotFound)?;
            if c.operator != operator || c.runtime.is_some() {
                return Err(Error::Conflict(
                    "composition is built or belongs to another operator",
                ));
            }
            discard(state, id, now)
        })
    }
}
fn discard(state: &mut d::state::PersistedState, id: &str, now: &d::Timestamp) -> Result {
    let c = state.compositions.remove(id).ok_or(Error::NotFound)?;
    if c.capsule_changes.is_some() {
        crate::blobs::queue_garbage(
            state,
            &spin_core::validation::text(format_args!("manifest:composition:{id}"))?,
        )?;
    }
    if let Some(s) = state.sessions.get_mut(&c.session_id)
        && s.prepared_composition_id == id
    {
        s.prepared_composition_id.clear();
        s.updated_at = now.try_clone()?;
    }
    Ok(())
}

#[cfg(test)]
mod shared_daemon_tests {
    use super::*;
    use crate::tests::Memory;
    use core::cell::Cell;
    use d::{Wire, state::PersistedState};

    #[test]
    fn reconnect_never_deletes_another_runners_capsules_on_a_shared_daemon() {
        let state = PersistedState::from_json(br#"{
            "compositions": {
                "a": {"id":"a","runtime":{"client_id":"runner-a","status":"ready"}},
                "b": {"id":"b","runtime":{"client_id":"runner-b","status":"ready"}},
                "stopped": {"id":"stopped","runtime":{"client_id":"runner-a","status":"stopped"}},
                "building": {"id":"building"}
            },
            "recordings": {
                "rec-a": {"id":"rec-a","status":"recording","runtime":{"client_id":"runner-a","status":"recording"}},
                "rec-b": {"id":"rec-b","status":"recording","runtime":{"client_id":"runner-b","status":"recording"}},
                "sealed": {"id":"sealed","status":"sealed","runtime":{"client_id":"runner-a","status":"stopped"}}
            }
        }"#).unwrap();
        let fail = Cell::new(false);
        let mut store = Store::new(state, Memory(&fail));
        let compositions = ["a", "b", "building"].map(alloc::string::String::from);
        let recordings = ["rec-a", "rec-b"].map(alloc::string::String::from);
        for client in ["runner-a", "runner-b", "runner-a"] {
            assert_eq!(
                store
                    .reconcile_client_capsules(client, &compositions, &recordings)
                    .unwrap(),
                0
            );
            let orphan = store
                .orphan_capsules(client, &compositions, &recordings)
                .unwrap();
            assert!(orphan.compositions.is_empty());
            assert!(orphan.recordings.is_empty());
        }
        let stopped = [alloc::string::String::from("stopped")];
        let sealed = [alloc::string::String::from("sealed")];
        let foreign = store
            .orphan_capsules("runner-b", &stopped, &sealed)
            .unwrap();
        assert!(foreign.compositions.is_empty());
        assert!(foreign.recordings.is_empty());
        let owned = store
            .orphan_capsules("runner-a", &stopped, &sealed)
            .unwrap();
        assert_eq!(owned.compositions.as_slice(), stopped.as_slice());
        assert_eq!(owned.recordings.as_slice(), sealed.as_slice());
        assert_eq!(
            store
                .reconcile_client_capsules("runner-a", &[], &[])
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .composition("b")
                .unwrap()
                .runtime
                .as_ref()
                .unwrap()
                .status,
            "ready"
        );
        assert_eq!(
            store
                .recording("rec-b")
                .unwrap()
                .runtime
                .as_ref()
                .unwrap()
                .status,
            "recording"
        );
    }
}
