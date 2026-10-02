//! De app-actor bezit logische peers; de socket bezit uitsluitend een intrekbaar handvat.
use super::*;
use alloc::vec::Vec;
use d::{
    protocol::{self as p, WireMessage},
    try_string,
};
use spin_core::{
    runner::{Peer, Ticket},
    validation::text,
};
use spin_security::{constant_time_eq, sha256};
use spin_store::Context;

/// Ook offline peers met replaywerk vallen onder het vaste vlootbudget.
pub(crate) const MAX_RUNNERS: usize = 256;
/// Autorisatie en generatie van één vervangbare fysieke runnerverbinding.
pub struct RunnerLink {
    authority: Authority,
    binding: Option<Binding>,
}
enum Authority {
    Worker([u8; 32]),
    Browser(String),
}
struct Binding {
    client: String,
    generation: u64,
}
/// De eigenaar handelt een antwoord of processtream af; de netwerktaak ziet geen Store.
// Eén tijdelijke returnwaarde; boxing zou voor ieder bericht een allocatie toevoegen.
#[allow(clippy::large_enum_variant)]
pub enum RunnerEvent {
    /// De eerste hello heeft een duurzame identiteit gekregen.
    Attached {
        /// De duurzame ClientID.
        client: String,
        /// Streams die bij de reconnect niet meer aanwezig waren.
        missing_streams: Vec<String>,
    },
    /// Antwoord op een nog open, bekende aanvraag.
    Response {
        /// De oorspronkelijke aanvraag met zijn ongewijzigde replay-ID.
        request: WireMessage,
        /// Het bijbehorende antwoord van de huidige verbinding.
        response: WireMessage,
    },
    /// Een processtream of bijgewerkt bestand van deze runner.
    Message(WireMessage),
    /// Deze runner heeft al zijn werk afgebouwd en sluit de verbinding.
    Goodbye,
    /// Een oud of onbekend bericht heeft geen effect.
    Ignored,
}
impl From<spin_core::runner::Error> for Error {
    fn from(error: spin_core::runner::Error) -> Self {
        use spin_core::runner::Error as E;
        match error {
            E::IdentityInUse(_) => Self::Http(
                409,
                "runner identity is already connected from another process",
            ),
            E::Offline => Self::Http(409, "runner connection was replaced"),
            E::Data(error) => Self::Data(error),
            _ => Self::Http(503, "runner capacity reached"),
        }
    }
}
impl<P: Persistence> Server<P> {
    /// Boot initialiseert één token; een al opgeslagen token overleeft de deployment-seed.
    pub fn ensure_worker_token(&mut self, seed: &str, runtime: &mut impl Runtime) -> Result {
        if self.store.worker_token().is_empty() {
            let generated;
            let seed = if seed.trim().is_empty() {
                generated = text(format_args!("spw_{}", auth::token(runtime)?))?;
                &generated
            } else {
                seed
            };
            self.store.ensure_worker_token(seed)?;
        }
        Ok(())
    }
    pub(crate) fn valid_worker(&self, req: &Request<'_>) -> bool {
        let Some(provided) = req.header("Authorization").strip_prefix("Bearer ") else {
            return false;
        };
        !self.store.worker_token().is_empty()
            && constant_time_eq(
                &sha256(provided.trim().as_bytes()),
                &sha256(self.store.worker_token().as_bytes()),
            )
    }
    pub(crate) fn worker_link(&self) -> RunnerLink {
        RunnerLink {
            authority: Authority::Worker(sha256(self.store.worker_token().as_bytes())),
            binding: None,
        }
    }
    pub(crate) fn browser_runner(token: String) -> RunnerLink {
        RunnerLink {
            authority: Authority::Browser(token),
            binding: None,
        }
    }
    /// Intrekking en generatie worden ook zonder inkomend verkeer gecontroleerd.
    pub fn validate_runner(&mut self, link: &RunnerLink, now: &Timestamp) -> Result {
        match &link.authority {
            Authority::Worker(hash) => {
                if self.store.worker_token().is_empty()
                    || !constant_time_eq(hash, &sha256(self.store.worker_token().as_bytes()))
                {
                    return Err(Error::Http(401, "runner token revoked"));
                }
            }
            Authority::Browser(hash) => {
                self.store.authenticate_session(hash, now)?;
            }
        }
        if let Some(binding) = &link.binding {
            self.peer(binding)?;
        }
        Ok(())
    }
    fn peer(&self, binding: &Binding) -> Result<&Peer> {
        self.runners
            .iter()
            .find(|p| p.client().id == binding.client && p.owns(binding.generation))
            .ok_or(Error::Http(409, "runner connection was replaced"))
    }
    fn peer_mut(&mut self, binding: &Binding) -> Result<&mut Peer> {
        self.runners
            .iter_mut()
            .find(|p| p.client().id == binding.client && p.owns(binding.generation))
            .ok_or(Error::Http(409, "runner connection was replaced"))
    }
    /// Alleen de actuele socket vernieuwt de claim bij ping/pong.
    pub fn runner_touch(&mut self, link: &RunnerLink, now_ms: u64) -> Result {
        if let Some(binding) = &link.binding {
            self.peer_mut(binding)?.touch(now_ms);
        }
        Ok(())
    }
    /// Verwerkt een bericht onder dezelfde eigenaar als Store en het replayregister.
    pub fn runner_message(
        &mut self,
        link: &mut RunnerLink,
        message: WireMessage,
        now: &Timestamp,
        now_ms: u64,
        runtime: &mut impl Runtime,
    ) -> Result<RunnerEvent> {
        self.validate_runner(link, now)?;
        if link.binding.is_none() {
            if !message.is_supported_hello()
                || message.instance_id.len() > 1024
                || message.process.len() > 1024
                || message.name.len() > 1024
            {
                return Err(Error::Http(
                    400,
                    "first runner message must be a supported hello",
                ));
            }
            if let Some(client) = self.store.client_by_instance(&message.instance_id)
                && let Some(peer) = self.runners.iter().find(|p| p.client().id == client.id)
            {
                peer.can_attach(&message.process, now_ms)?;
            }
            let existing = self
                .store
                .client_by_instance(&message.instance_id)
                .and_then(|c| self.runners.iter().position(|p| p.client().id == c.id));
            if existing.is_none() {
                if self.runners.len() >= MAX_RUNNERS {
                    return Err(Error::Http(503, "runner fleet capacity reached"));
                }
                self.runners
                    .try_reserve(1)
                    .map_err(|_| d::Error::OutOfMemory)?;
            }
            let id = runtime.next("cli")?;
            let client = self.store.register_client(
                d::RegisterClientRequest {
                    instance_id: message.instance_id,
                    name: message.name,
                    capabilities: message.capabilities,
                },
                Context { now, id: &id },
            )?;
            let welcome = WireMessage {
                version: p::PROTOCOL_VERSION,
                r#type: try_string(p::MESSAGE_WELCOME)?,
                client: Some(client.try_clone()?),
                ..Default::default()
            };
            let index = match existing {
                Some(index) => {
                    self.runners[index].update_client(client.try_clone()?)?;
                    index
                }
                None => {
                    self.runners.push(Peer::new(client.try_clone()?));
                    self.runners.len() - 1
                }
            };
            for agent in self
                .agents
                .iter()
                .filter(|a| a.client == client.id && !a.closed)
            {
                self.runners[index].adopt_stream(&agent.stream, false)?;
            }
            let (generation, missing_streams) = self.runners[index].attach(
                &message.process,
                message
                    .streams_reported
                    .then_some(message.streams.as_slice()),
                now_ms,
            )?;
            link.binding = Some(Binding {
                client: client.id.try_clone()?,
                generation,
            });
            // Welcome gaat vóór replay: de runner kan pas met zijn ClientID antwoorden.
            self.runners[index].prepend(welcome)?;
            let store = &self.store;
            self.watch_stamps.retain(|id, _| {
                store
                    .composition(id)
                    .is_ok_and(|c| c.runtime.as_ref().is_some_and(|r| r.client_id != client.id))
            });
            if let Some(capsules) = message.capsules {
                self.store.reconcile_client_capsules(
                    &client.id,
                    capsules.compositions.as_slice(),
                    capsules.recordings.as_slice(),
                )?;
                let orphan = self.store.orphan_capsules(
                    &client.id,
                    capsules.compositions.as_slice(),
                    capsules.recordings.as_slice(),
                )?;
                if !orphan.compositions.is_empty() || !orphan.recordings.is_empty() {
                    self.runners[index].request(WireMessage {
                        r#type: try_string(p::MESSAGE_REQUEST)?,
                        id: runtime.next("req")?,
                        method: try_string(p::METHOD_REMOVE_CAPSULES)?,
                        payload: d::RawJson(Some(orphan.to_value()?)),
                        ..Default::default()
                    })?;
                }
            }
            for id in &missing_streams {
                self.agent_runner(
                    &client.id,
                    &WireMessage {
                        r#type: try_string(p::MESSAGE_STREAM_EXIT)?,
                        id: id.try_clone()?,
                        error: try_string("runner no longer owns this agent")?,
                        ..Default::default()
                    },
                    now,
                )?;
                self.terminal_runner(
                    &client.id,
                    &WireMessage {
                        r#type: try_string(p::MESSAGE_STREAM_EXIT)?,
                        id: id.try_clone()?,
                        error: try_string("runner no longer owns this terminal")?,
                        ..Default::default()
                    },
                    now,
                )?;
            }
            return Ok(RunnerEvent::Attached {
                client: client.id,
                missing_streams,
            });
        }
        let binding = link
            .binding
            .as_ref()
            .ok_or(Error::Http(409, "runner has no identity"))?;
        let peer = self.peer_mut(binding)?;
        peer.touch(now_ms);
        match message.r#type.as_str() {
            p::MESSAGE_RESPONSE => match peer.response(&message.id) {
                Some(request) => {
                    if self.agent_runner(&binding.client, &message, now)? {
                        return Ok(RunnerEvent::Ignored);
                    }
                    if self.terminal_runner(&binding.client, &message, now)? {
                        return Ok(RunnerEvent::Ignored);
                    }
                    if self.finish_capsule(
                        &binding.client,
                        Some(&request),
                        &message,
                        now,
                        runtime,
                    )? {
                        return Ok(RunnerEvent::Ignored);
                    }
                    Ok(RunnerEvent::Response {
                        request,
                        response: message,
                    })
                }
                None => Ok(RunnerEvent::Ignored),
            },
            p::MESSAGE_EVENT if message.method == p::METHOD_TRACKED_CHANGED => {
                if let Some(payload) = message.payload.0.as_ref() {
                    if message.error.is_empty() {
                        self.tracked_changed(&binding.client, payload, now)?;
                    } else {
                        self.tracked_watcher_stopped(&binding.client, payload)?;
                    }
                }
                Ok(RunnerEvent::Ignored)
            }
            p::MESSAGE_STREAM_DATA | p::MESSAGE_STREAM_EXIT | p::MESSAGE_EVENT => {
                if message.r#type == p::MESSAGE_STREAM_EXIT {
                    peer.finish_stream(&message.id);
                }
                if self.agent_runner(&binding.client, &message, now)? {
                    return Ok(RunnerEvent::Ignored);
                }
                if self.terminal_runner(&binding.client, &message, now)? {
                    return Ok(RunnerEvent::Ignored);
                }
                Ok(RunnerEvent::Message(message))
            }
            p::MESSAGE_GOODBYE => {
                self.store.set_client_status(
                    &binding.client,
                    if message.idle { "offline" } else { "draining" },
                    now,
                )?;
                Ok(RunnerEvent::Goodbye)
            }
            _ => Ok(RunnerEvent::Ignored),
        }
    }
    /// Leent één bericht; pas bevestiging na succesvolle socket-write verwijdert het.
    pub fn runner_next(&self, link: &RunnerLink) -> Result<Option<(Ticket, &WireMessage)>> {
        let Some(binding) = &link.binding else {
            return Ok(None);
        };
        Ok(self.peer(binding)?.next(binding.generation)?)
    }
    /// Oude bevestigingen veranderen nooit de outbox van de vervangende verbinding.
    pub fn runner_acknowledge(&mut self, link: &RunnerLink, ticket: Ticket) -> bool {
        link.binding
            .as_ref()
            .and_then(|b| self.peer_mut(b).ok())
            .is_some_and(|p| p.acknowledge(ticket))
    }
    /// Disconnect geeft geen nieuwe runner toegang tot de bestaande capsule-affiniteit.
    pub fn runner_disconnect(&mut self, link: RunnerLink, now: &Timestamp) -> Result {
        if let Some(binding) = link.binding
            && let Some(peer) = self
                .runners
                .iter_mut()
                .find(|p| p.client().id == binding.client)
            && peer.detach(binding.generation)?
        {
            peer.fail_bulk_streams()?;
            self.store
                .set_client_status(&binding.client, "offline", now)?;
        }
        Ok(())
    }
    pub(crate) fn runner_info(&self) -> Result<d::CapsuleEngineInfo> {
        for peer in &self.runners {
            if peer.is_connected()
                && !peer.client().draining
                && peer.client().capabilities.engine.available
            {
                let mut info = peer.client().capabilities.engine.try_clone()?;
                info.driver = text(format_args!("runner/{}", info.driver))?;
                info.detail = text(format_args!("remote runner fleet · {}", info.detail))?;
                return Ok(info);
            }
        }
        Ok(d::CapsuleEngineInfo {
            driver: try_string("runner")?,
            detail: try_string("waiting for a connected Docker runner")?,
            ..Default::default()
        })
    }
}
