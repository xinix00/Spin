//! De bestaande externe-worker-API; iedere write wordt door activatie en epoch afgeschermd.
use super::*;
use spin_store::Context;

pub(crate) fn worker_path(path: &str) -> bool {
    matches!(
        path,
        "/api/runner/ws" | "/api/clients/register" | "/api/sessions/claim" | "/api/uploads"
    ) || [
        "/api/sessions/",
        "/api/activations/",
        "/api/uploads/",
        "/api/snapshots/",
        "/api/blobs/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
}
impl<P: Persistence> Server<P> {
    pub(crate) fn worker_route(
        &mut self,
        req: &Request<'_>,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        if req.method != "POST" {
            return Ok(None);
        }
        match req.path {
            "/api/clients/register" => {
                let value = Self::decode(req)?;
                let id = runtime.next("cli")?;
                let client = self
                    .store
                    .register_client(value, Context { now, id: &id })?;
                if let Some(peer) = self.runners.iter_mut().find(|p| p.client().id == client.id) {
                    peer.update_client(client.try_clone()?)?;
                }
                return Ok(Some(Response::json(200, &client)?));
            }
            "/api/sessions/claim" => {
                let value = Self::decode(req)?;
                let id = runtime.next("act")?;
                return Ok(Some(Response::json(
                    200,
                    &self.store.claim(value, Context { now, id: &id })?,
                )?));
            }
            _ => {}
        }
        let mut path = req.path.trim_start_matches('/').split('/');
        let route = (
            path.next(),
            path.next(),
            path.next(),
            path.next(),
            path.next(),
        );
        let result = match route {
            (Some("api"), Some("sessions"), Some(id), Some("start"), None) => Response::json(
                200,
                &self.store.start_session(id, &Self::decode(req)?, now)?,
            ),
            (Some("api"), Some("activations"), Some(id), Some("heartbeat"), None) => {
                Response::json(200, &self.store.heartbeat(id, &Self::decode(req)?, now)?)
            }
            (Some("api"), Some("sessions"), Some(id), Some("turns"), None) => {
                let value = Self::decode(req)?;
                let turn = runtime.next("trn")?;
                Response::json(
                    201,
                    &self
                        .store
                        .start_turn(id, value, Context { now, id: &turn })?,
                )
            }
            (Some("api"), Some("sessions"), Some(id), Some("checkpoints"), None) => {
                let value = Self::decode(req)?;
                let checkpoint = runtime.next("chk")?;
                Response::json(
                    201,
                    &self.store.add_checkpoint(
                        id,
                        value,
                        Context {
                            now,
                            id: &checkpoint,
                        },
                    )?,
                )
            }
            (Some("api"), Some("sessions"), Some(id), Some("result"), None) => {
                let value = Self::decode(req)?;
                let result = runtime.next("res")?;
                Response::json(
                    201,
                    &self
                        .store
                        .complete_session(id, value, Context { now, id: &result })?,
                )
            }
            (Some("api"), Some("sessions"), Some(id), Some("fork"), None) => {
                let value = Self::decode(req)?;
                let session = runtime.next("ses")?;
                Response::json(
                    201,
                    &self
                        .store
                        .fork_session(id, value, Context { now, id: &session })?,
                )
            }
            _ => return Ok(None),
        }?;
        Ok(Some(result))
    }
}
