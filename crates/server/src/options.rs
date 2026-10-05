//! Discover agent settings through a temporary, normally authenticated capsule.
use super::*;
use crate::capsules::Action;
use d::try_string;

pub(crate) struct Work {
    artifact: String,
    composition: String,
    stream: String,
    pending: Option<CapsuleWait>,
    stopping: bool,
    expires: u64,
    failure: Option<String>,
    reported: bool,
}
impl<P: Persistence> Server<P> {
    pub(crate) fn options_route(
        &mut self,
        req: &Request<'_>,
        actor: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let Some(id) = req
            .path
            .strip_prefix("/api/artifacts/")
            .and_then(|p| p.strip_suffix("/acp/options"))
            .filter(|id| req.method == "POST" && !id.is_empty() && !id.contains('/'))
        else {
            return Ok(None);
        };
        let target = self.store.enabling_layer(id, "acp")?.id.try_clone()?;
        let response = Response::json(
            202,
            &http::object(&[
                ("status", Value::string("fetching")?),
                ("artifact_id", Value::string(id)?),
            ])?,
        )?;
        if self.options.iter().any(|w| w.artifact == target) {
            return Ok(Some(response));
        }
        if self.options.len() >= 8 {
            return Err(Error::Http(503, "agent options capacity reached"));
        }
        self.options
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        let entry = self.store.identity_layer_for(id, actor)?;
        let request = d::UseRequest {
            selector: spin_core::validation::text(format_args!("{}:{}", entry.kind, entry.name))?,
            profile: entry.profile.try_clone()?,
            operator: try_string(actor)?,
            ..Default::default()
        };
        let mut work = Work {
            artifact: target,
            composition: String::new(),
            stream: String::new(),
            pending: None,
            stopping: false,
            expires: now.time()?.0.saturating_add(300_000_000_000),
            failure: None,
            reported: false,
        };
        self.store
            .mark_artifact_agent_options_fetching(&work.artifact, true)?;
        match self.begin_materialize_probe(request, &work.artifact, now, random) {
            Ok(wait) => work.pending = Some(wait),
            Err(error) => {
                self.store.set_artifact_agent_options(
                    &work.artifact,
                    d::AgentOptions {
                        error: match error {
                            Error::Http(_, reason) => try_string(reason)?,
                            _ => try_string("agent probe capsule could not be prepared")?,
                        },
                        ..Default::default()
                    },
                    now,
                )?;
                return Ok(Some(response));
            }
        }
        // From here the queue owns cleanup, including operation timeouts and runner disconnects.
        self.options.push(work);
        Ok(Some(response))
    }
    pub(crate) fn maintain_options(
        &mut self,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        let mut index = 0;
        while index < self.options.len() {
            match self.advance_options(index, now, random) {
                Ok(true) => {
                    self.options.remove(index);
                }
                Ok(false) => index += 1,
                Err(error) => {
                    // Keep the owner and retry its cleanup on the next actor round.
                    self.options[index].failure =
                        Some(spin_core::validation::text(format_args!("{error}"))?);
                    self.options[index].stopping = true;
                    index += 1;
                }
            }
        }
        Ok(())
    }
    fn advance_options(
        &mut self,
        index: usize,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<bool> {
        if self.options[index].composition.is_empty()
            && let Some(wait) = &self.options[index].pending
        {
            let call = self
                .calls
                .iter()
                .find(|c| c.id == wait.id)
                .ok_or(Error::Http(409, "probe materialization disappeared"))?;
            if let Action::Materialize(work) = &call.action {
                self.options[index].composition = work.id.try_clone()?;
            } else {
                return Err(Error::Http(500, "invalid probe materialization"));
            }
        }
        if self.options[index].expires <= now.time()?.0 && !self.options[index].stopping {
            self.options[index].failure = Some(try_string("agent options probe timed out")?);
            self.options[index].stopping = true;
        }
        if let Some(wait) = self.options[index].pending.take() {
            match self.poll_capsule(&wait, now) {
                Ok(None) => {
                    self.options[index].pending = Some(wait);
                    return Ok(false);
                }
                Err(Error::Http(404, _)) => {
                    self.options[index].failure =
                        Some(try_string("agent probe operation expired")?);
                    self.options[index].stopping = true;
                }
                Err(error) => {
                    self.options[index].pending = Some(wait);
                    return Err(error);
                }
                Ok(Some(response)) if response.status >= 400 => {
                    // De reden van de capsule (runner, login, plaats) hoort in de melding.
                    let reason = Value::from_json(&response.body).ok();
                    let reason = reason
                        .as_ref()
                        .and_then(|v| v.as_object())
                        .and_then(|o| o.get("error"))
                        .and_then(|e| e.as_str())
                        .unwrap_or("");
                    self.options[index].failure = Some(spin_core::validation::text(format_args!(
                        "agent probe capsule operation failed ({}): {reason}",
                        response.status
                    ))?);
                    self.options[index].stopping = true;
                }
                Ok(Some(_)) => {}
            }
        }
        if !self.options[index].stopping {
            if self.options[index].stream.is_empty() {
                let composition = self.options[index].composition.try_clone()?;
                let artifact = self.options[index].artifact.try_clone()?;
                let stream = self.start_options_agent(&composition, &artifact, now, random)?;
                self.options[index].stream = stream;
                return Ok(false);
            }
            let stream = &self.options[index].stream;
            let agent = self
                .agents
                .iter()
                .find(|a| a.stream == *stream)
                .ok_or(Error::Http(502, "agent options process disappeared"))?;
            if agent.options_done {
                self.options[index].stopping = true;
                self.options[index].reported = true;
            } else if !agent.failure.is_empty() || agent.closed {
                self.options[index].failure =
                    Some(try_string("agent did not complete its ACP handshake")?);
                self.options[index].stopping = true;
            } else {
                return Ok(false);
            }
        }
        if !self.options[index].reported {
            let error = self.options[index]
                .failure
                .as_deref()
                .unwrap_or("agent options probe failed");
            if self.store.artifact(&self.options[index].artifact).is_ok() {
                self.store.set_artifact_agent_options(
                    &self.options[index].artifact,
                    d::AgentOptions {
                        error: try_string(error)?,
                        ..Default::default()
                    },
                    now,
                )?;
            }
            self.options[index].reported = true;
        }
        let id = self.options[index].composition.try_clone()?;
        let Ok(composition) = self.store.composition(&id) else {
            return Ok(true);
        };
        if composition
            .runtime
            .as_ref()
            .is_none_or(|r| r.status == "stopped")
        {
            return Ok(true);
        }
        if self
            .calls
            .iter()
            .any(|c| !c.finished && c.action.object() == id)
        {
            return Ok(false);
        }
        if let Some(wait) = self.begin_stop(&id, "options_probe_done", now, random)? {
            self.options[index].pending = Some(wait);
            Ok(false)
        } else {
            // Offline cleanup is now represented by the durable stop_pending flag.
            Ok(true)
        }
    }
}
