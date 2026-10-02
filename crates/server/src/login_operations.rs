//! Loginbeheer houdt bindings vast tot lezen, schrijven en stoppen bevestigd zijn.
use super::*;
use crate::{capsules::Action, materialize::Target};
use d::{List, WireMap, protocol as p, try_string};

pub(crate) struct Work {
    pub(crate) id: String,
    pub(crate) response: Option<Response>,
    mode: Mode,
    pending: Option<Pending>,
    stop: Option<String>,
    swapped: u64,
    failed: bool,
    expires: u64,
}
enum Mode {
    Save {
        composition: String,
        keys: List<String>,
    },
    Park {
        login: String,
        delete: bool,
    },
}
struct Pending {
    wait: CapsuleWait,
    composition: String,
    swap: bool,
}
pub(crate) struct Capture {
    pub(crate) composition: String,
    target: Target,
    new_id: Option<String>,
    writing: bool,
}
impl<P: Persistence> Server<P> {
    pub(crate) fn login_operation_route(
        &mut self,
        req: &Request<'_>,
        user: &d::User,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        if let Some(id) = req
            .path
            .strip_prefix("/api/artifacts/")
            .and_then(|p| p.strip_suffix("/tracked/exclude"))
            .filter(|id| req.method == "POST" && !id.is_empty() && !id.contains('/'))
        {
            let key = spin_store::layer_key(self.store.artifact(id)?)?;
            self.manage_login_key(&key, user)?;
            let value: Value = Self::decode(req)?;
            let path = value
                .as_object()
                .and_then(|o| o.get("path"))
                .and_then(Value::as_str)
                .ok_or(Error::Http(400, "path is required"))?;
            let (artifact, removed) = self.store.exclude_artifact_login_path(id, path, now)?;
            return Ok(Some(Outcome::Response(Response::json(
                200,
                &http::object(&[
                    ("artifact", artifact.to_value()?),
                    ("removed", Value::uint(removed as u64)),
                ])?,
            )?)));
        }
        let mode = if let Some(id) = req
            .path
            .strip_prefix("/api/compositions/")
            .and_then(|p| p.strip_suffix("/login"))
            .filter(|id| req.method == "POST" && !id.is_empty() && !id.contains('/'))
        {
            let composition = self.store.composition(id)?;
            if composition.operator != user.username || !composition.for_login {
                return Err(Error::Http(
                    409,
                    "start your own capsule with New login first",
                ));
            }
            if !composition
                .runtime
                .as_ref()
                .is_some_and(|r| r.status == "ready" && !r.stop_pending)
            {
                return Err(Error::Http(409, "login capsule is not running"));
            }
            let mut keys = List::new();
            for target in self.tracked_targets(composition)? {
                if target.exclusive && composition.logins.get(&target.key).is_none() {
                    keys.push(target.key)?;
                }
            }
            if keys.is_empty() {
                return Err(Error::Http(
                    409,
                    "no unsaved credential layer with tracked files",
                ));
            }
            Mode::Save {
                composition: try_string(id)?,
                keys,
            }
        } else if let Some(tail) = req.path.strip_prefix("/api/logins/") {
            let (id, delete) = match (req.method, tail.strip_suffix("/disabled")) {
                ("PUT", Some(id)) => (id, false),
                ("DELETE", None) => (tail, true),
                _ => return Ok(None),
            };
            if id.is_empty() || id.contains('/') {
                return Ok(None);
            }
            self.manage_login(id, user)?;
            let disabled = if !delete && !req.body.is_empty() {
                let value: Value = Self::decode(req)?;
                match value.as_object().and_then(|o| o.get("disabled")) {
                    None | Some(Value::Null) => true,
                    Some(Value::Bool(v)) => *v,
                    _ => return Err(Error::Http(400, "disabled must be a boolean")),
                }
            } else {
                true
            };
            if self.login_operations.iter().any(|op| {
                op.response.is_none() && matches!(&op.mode, Mode::Park { login, .. } if login == id)
            }) {
                return Err(Error::Http(409, "login operation is still running"));
            }
            if !disabled {
                let response = park_response(false, 0, false)?;
                self.store.set_login_disabled(id, false)?;
                return Ok(Some(Outcome::Response(response)));
            }
            Mode::Park {
                login: try_string(id)?,
                delete,
            }
        } else {
            return Ok(None);
        };
        if self.login_operations.iter().any(|op| op.response.is_none() && matches!((&op.mode, &mode), (Mode::Save { composition: a, .. }, Mode::Save { composition: b, .. }) if a == b)) {
            return Err(Error::Http(409, "login is already being captured"));
        }
        self.login_operations
            .retain(|op| op.response.is_none() || op.expires > now.time().map_or(0, |t| t.0));
        if self.login_operations.len() >= 32 {
            return Err(Error::Http(503, "login operation capacity reached"));
        }
        self.login_operations
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        let id = random.next("login-op")?;
        let wait = OperationWait {
            id: id.try_clone()?,
        };
        let work = Work {
            id,
            response: None,
            mode,
            pending: None,
            stop: None,
            swapped: 0,
            failed: false,
            expires: now.time()?.0.saturating_add(300_000_000_000),
        };
        if let Mode::Park { login, .. } = &work.mode {
            // A parked or deleting login cannot acquire new holders while we drain it.
            self.store.set_login_disabled(login, true)?;
        }
        self.login_operations.push(work);
        Ok(Some(Outcome::Operation(wait)))
    }
    pub(crate) fn maintain_login_operations(
        &mut self,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        for index in 0..self.login_operations.len() {
            if self.login_operations[index].response.is_some() {
                continue;
            }
            let result = self.advance_login_operation(index, now, random);
            let response = match result {
                Ok(None) => continue,
                Ok(Some(response)) => response,
                Err(error) => error.response()?,
            };
            self.login_operations[index].response = Some(response);
            self.login_operations[index].expires = now.time()?.0.saturating_add(300_000_000_000);
            self.last_launch_sweep = None;
            return Ok(());
        }
        Ok(())
    }
    fn advance_login_operation(
        &mut self,
        index: usize,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        if self.login_operations[index].expires <= now.time()?.0 {
            return Err(Error::Http(
                503,
                "login operation was not confirmed; its capsule bindings are retained",
            ));
        }
        if let Some(pending) = self.login_operations[index].pending.take() {
            match self.poll_capsule(&pending.wait, now) {
                Ok(None) => {
                    self.login_operations[index].pending = Some(pending);
                    return Ok(None);
                }
                Err(error) => {
                    self.login_operations[index].pending = Some(pending);
                    return Err(error);
                }
                Ok(Some(response)) if response.status >= 400 => {
                    if !pending.swap {
                        return Ok(Some(response));
                    }
                    self.login_operations[index].failed = true;
                    self.login_operations[index].stop = Some(pending.composition);
                }
                Ok(Some(_)) => {
                    if pending.swap {
                        self.login_operations[index].swapped += 1;
                    }
                }
            }
        }
        if let Some(id) = self.login_operations[index].stop.as_ref() {
            let composition = self.store.composition(id)?;
            if composition
                .runtime
                .as_ref()
                .is_some_and(|r| r.status != "stopped")
            {
                let id = id.try_clone()?;
                if self
                    .calls
                    .iter()
                    .any(|c| !c.finished && c.action.object() == id)
                {
                    return Ok(None);
                }
                if let Some(wait) = self.begin_stop(&id, now, random)? {
                    self.login_operations[index].pending = Some(Pending {
                        wait,
                        composition: id,
                        swap: false,
                    });
                }
                return Ok(None);
            }
            self.login_operations[index].stop = None;
        }
        match &self.login_operations[index].mode {
            Mode::Save {
                composition: id,
                keys,
            } => {
                let composition = self.store.composition(id)?;
                if self
                    .calls
                    .iter()
                    .any(|c| !c.finished && c.action.object() == *id)
                {
                    return Ok(None);
                }
                let target = self
                    .tracked_targets(composition)?
                    .into_iter()
                    .find(|t| keys.contains(&t.key) && composition.logins.get(&t.key).is_none());
                if let Some(target) = target {
                    if !composition
                        .runtime
                        .as_ref()
                        .is_some_and(|r| r.status == "ready" && !r.stop_pending)
                    {
                        return Err(Error::Http(
                            409,
                            "login capsule stopped before credentials were captured",
                        ));
                    }
                    let id = id.try_clone()?;
                    let new_id = random.next("lgn")?;
                    let wait = self.begin_login_capture(&id, target, Some(new_id), now, random)?;
                    self.login_operations[index].pending = Some(Pending {
                        wait,
                        composition: id,
                        swap: false,
                    });
                    return Ok(None);
                }
                if keys.iter().any(|k| composition.logins.get(k).is_none()) {
                    return Err(Error::Http(
                        409,
                        "credential tracking changed during capture",
                    ));
                }
                if composition
                    .runtime
                    .as_ref()
                    .is_some_and(|r| r.status != "stopped")
                {
                    self.login_operations[index].stop = Some(id.try_clone()?);
                    return Ok(None);
                }
                let mut summaries = self.store.login_summaries()?;
                summaries.retain(|s| {
                    keys.contains(&s.key) && composition.logins.get(&s.key) == Some(&s.id)
                });
                Ok(Some(Response::json(201, &summaries)?))
            }
            Mode::Park { login, delete } => {
                let key = &self.store.login(login)?.key;
                let composition = self
                    .store
                    .running_compositions()?
                    .into_vec()
                    .into_iter()
                    .find(|c| c.logins.get(key) == Some(login));
                let Some(composition) = composition else {
                    let response = if *delete {
                        Response::empty(204)?
                    } else {
                        park_response(
                            true,
                            self.login_operations[index].swapped,
                            self.login_operations[index].failed,
                        )?
                    };
                    if *delete {
                        self.store.delete_login(login)?;
                    }
                    return Ok(Some(response));
                };
                if self
                    .calls
                    .iter()
                    .any(|c| !c.finished && c.action.object() == composition.id)
                {
                    return Ok(None);
                }
                let target = self
                    .tracked_targets(&composition)?
                    .into_iter()
                    .find(|t| t.key == *key);
                let online = composition.runtime.as_ref().is_some_and(|r| {
                    !r.stop_pending
                        && self
                            .runners
                            .iter()
                            .any(|p| p.client().id == r.client_id && p.is_connected())
                });
                if *delete || !online || target.is_none() {
                    self.login_operations[index].stop = Some(composition.id);
                    return Ok(None);
                }
                let target = target.ok_or(Error::Http(500, "missing tracked selection"))?;
                let wait = self.begin_login_capture(&composition.id, target, None, now, random)?;
                self.login_operations[index].pending = Some(Pending {
                    wait,
                    composition: composition.id,
                    swap: true,
                });
                Ok(None)
            }
        }
    }
    fn begin_login_capture(
        &mut self,
        id: &str,
        target: Target,
        new_id: Option<String>,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<CapsuleWait> {
        let capsule = self
            .store
            .composition(id)?
            .runtime
            .as_ref()
            .ok_or(Error::Http(409, "capsule is not running"))?;
        let client = capsule.client_id.try_clone()?;
        let payload = p::TrackedFilesPayload {
            runtime: capsule.try_clone()?,
            paths: target.paths.try_clone()?,
            excludes: target.excludes.try_clone()?,
            ..Default::default()
        };
        self.enqueue_call(
            Action::Login(Capture {
                composition: try_string(id)?,
                target,
                new_id,
                writing: false,
            }),
            &client,
            p::METHOD_READ_TRACKED,
            &payload,
            now,
            random,
        )
    }
    pub(crate) fn advance_login_capture(
        &mut self,
        index: usize,
        client: &str,
        message: &p::WireMessage,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let Action::Login(work) = &self.calls[index].action else {
            return Err(Error::Http(500, "missing login capture"));
        };
        if !message.error.is_empty() {
            return Err(Error::Http(
                502,
                "login files could not be read or installed",
            ));
        }
        if work.writing {
            let composition = self.store.composition(&work.composition)?;
            let mut capsule = composition
                .runtime
                .as_ref()
                .ok_or(Error::Http(409, "capsule stopped"))?
                .try_clone()?;
            if capsule.status != "installing_login" {
                return Err(Error::Http(
                    409,
                    "capsule changed during login installation",
                ));
            }
            let response = Response::empty(204)?;
            capsule.status = try_string("ready")?;
            let actor = composition.operator.try_clone()?;
            self.store
                .set_composition_runtime(&work.composition, &actor, capsule)?;
            return Ok(Some(response));
        }
        let mut files =
            WireMap::<d::Bytes>::from_value(message.payload.0.as_ref().unwrap_or(&Value::Null))?;
        files.retain(|path, _| {
            d::tracked_covers(
                path,
                work.target.paths.as_slice(),
                work.target.excludes.as_slice(),
            )
        });
        let composition = self.store.composition(&work.composition)?;
        if let Some(id) = &work.new_id {
            let owner = if composition.for_login_private {
                composition.operator.try_clone()?
            } else {
                String::new()
            };
            let response = Response::empty(204)?;
            self.store.create_login(
                &work.composition,
                &work.target.key,
                &files,
                &owner,
                spin_store::Context { now, id },
            )?;
            return Ok(Some(response));
        }
        let old = composition
            .logins
            .get(&work.target.key)
            .ok_or(Error::Http(409, "capsule no longer holds this login"))?
            .try_clone()?;
        let capsule = composition
            .runtime
            .as_ref()
            .ok_or(Error::Http(409, "capsule stopped"))?
            .try_clone()?;
        let composition = work.composition.try_clone()?;
        let key = work.target.key.try_clone()?;
        let mut folders = List::new();
        for path in work.target.paths.iter().filter(|p| d::tracked_folder(p)) {
            folders.push(path.try_clone()?)?;
        }
        self.store
            .save_login_files(&old, &files, folders.as_slice(), now)?;
        let replacement = self.store.prepare_login_install(&composition, &key, now)?;
        // If dispatch or the ACK fails, the outer operation stops this capsule.
        // Its replacement remains held until that stop is confirmed.
        let payload = p::TrackedFilesPayload {
            runtime: capsule,
            files: replacement.files,
            ..Default::default()
        };
        self.continue_capsule(index, client, p::METHOD_WRITE_TRACKED, &payload, random)?;
        if let Action::Login(work) = &mut self.calls[index].action {
            work.writing = true;
        }
        Ok(None)
    }
}
fn park_response(disabled: bool, swapped: u64, failed: bool) -> Result<Response> {
    Response::json(
        200,
        &http::object(&[
            ("disabled", Value::Bool(disabled)),
            ("swapped", Value::uint(swapped)),
            (
                "error",
                Value::string(if failed {
                    "Een capsule is gesloten omdat een andere login niet kon worden geplaatst."
                } else {
                    ""
                })?,
            ),
        ])?,
    )
}
