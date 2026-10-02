//! Beheer gebruikt dezelfde eigenaar en geeft nooit logininhoud of tokens terug.
use super::*;
use d::{List, try_string};
use spin_core::validation::text;

pub(crate) struct Probe {
    pub(crate) composition: String,
    enablement: d::Enablement,
}
pub(crate) fn probe_response(work: &Probe, payload: &Value) -> Result<Response> {
    let object = payload
        .as_object()
        .ok_or(Error::Http(502, "invalid ACP initialize response"))?;
    let version = if work.enablement.protocol_version == 0 {
        1
    } else {
        work.enablement.protocol_version
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || object.get("id").and_then(Value::as_i64) != Some(0)
        || object
            .get("error")
            .is_some_and(|e| !matches!(e, Value::Null))
        || object
            .get("result")
            .and_then(Value::as_object)
            .and_then(|o| o.get("protocolVersion"))
            .and_then(Value::as_i64)
            != Some(version)
    {
        return Err(Error::Http(502, "ACP protocol negotiation failed"));
    }
    Response::json(
        200,
        &http::object(&[
            ("composition_id", Value::string(&work.composition)?),
            ("enablement", work.enablement.to_value()?),
            ("handshake", payload.try_clone()?),
        ])?,
    )
}
pub(crate) struct ReadFile {
    pub(crate) composition: String,
    path: String,
    absolute: String,
}
pub(crate) fn file_response(work: &ReadFile, payload: &Value) -> Result<Response> {
    let files = d::WireMap::<d::Bytes>::from_value(payload)?;
    let bytes = files
        .get(&work.absolute)
        .and_then(|b| b.0.as_ref())
        .ok_or(Error::Http(404, "file missing or exceeds file budget"))?;
    if bytes.len() > 1 << 20 {
        return Err(Error::Http(413, "file exceeds file budget"));
    }
    // The JSON contract is text even for binary files; preserve valid UTF-8 runs.
    let mut content = String::new();
    content
        .try_reserve(
            bytes
                .len()
                .checked_mul(3)
                .ok_or(Error::Http(413, "file exceeds file budget"))?,
        )
        .map_err(|_| d::Error::OutOfMemory)?;
    for chunk in bytes.utf8_chunks() {
        content.push_str(chunk.valid());
        if !chunk.invalid().is_empty() {
            content.push('\u{fffd}');
        }
    }
    Response::json(
        200,
        &d::engine::WorkspaceFile {
            r#ref: try_string("workspace")?,
            path: work.path.try_clone()?,
            size: bytes.len() as i64,
            binary: bytes.contains(&0),
            content,
            ..Default::default()
        },
    )
}

impl<P: Persistence> Server<P> {
    pub(crate) fn probe_route(
        &mut self,
        req: &Request<'_>,
        actor: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        let Some(id) = req
            .path
            .strip_prefix("/api/compositions/")
            .and_then(|p| p.strip_suffix("/acp/probe"))
            .filter(|id| req.method == "POST" && !id.is_empty() && !id.contains('/'))
        else {
            return Ok(None);
        };
        let composition = self.store.composition(id)?;
        if composition.operator != actor {
            return Err(Error::Http(403, "capsule belongs to another operator"));
        }
        let capsule = composition
            .runtime
            .as_ref()
            .filter(|r| r.status == "ready" && !r.stop_pending)
            .ok_or(Error::Http(409, "capsule is not running"))?;
        let enablement = composition
            .enabled
            .iter()
            .find(|e| e.name == "acp")
            .ok_or(Error::Http(409, "composition does not ENABLE acp"))?
            .try_clone()?;
        let client = capsule.client_id.try_clone()?;
        let request = http::object(&[
            ("jsonrpc", Value::string("2.0")?),
            ("id", Value::int(0)),
            ("method", Value::string("initialize")?),
            (
                "params",
                spin_core::acp::initialize(if enablement.protocol_version == 0 {
                    1
                } else {
                    enablement.protocol_version
                })?,
            ),
        ])?;
        let payload = d::protocol::EnabledPayload {
            runtime: capsule.try_clone()?,
            enablement: enablement.try_clone()?,
            request: d::RawJson(Some(request)),
        };
        Ok(Some(Outcome::Capsule(self.enqueue_call(
            crate::capsules::Action::Probe(Probe {
                composition: try_string(id)?,
                enablement,
            }),
            &client,
            d::protocol::METHOD_PROBE_ENABLED,
            &payload,
            now,
            random,
        )?)))
    }

    pub(crate) fn session_file_route(
        &mut self,
        req: &Request<'_>,
        actor: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        let Some(id) = req
            .path
            .strip_prefix("/api/sessions/")
            .and_then(|p| p.strip_suffix("/file"))
        else {
            return Ok(None);
        };
        if req.method != "GET" || id.contains('/') {
            return Ok(None);
        }
        let session = self.session_access(id, actor)?;
        let composition = self.store.composition(&session.prepared_composition_id)?;
        if composition.operator != session.operator {
            return Err(Error::Http(409, "workspace belongs to another operator"));
        }
        let capsule = composition
            .runtime
            .as_ref()
            .filter(|r| r.status == "ready" && !r.stop_pending)
            .ok_or(Error::Http(409, "session has no running workspace"))?;
        let raw = req.query("path")?;
        let path = raw.trim().trim_start_matches('/');
        let folder = req.query("folder")?;
        let folder = folder.trim();
        if path.is_empty()
            || path.len() > 4096
            || path.contains(['\\', '\0'])
            || path
                .split('/')
                .any(|s| s == ".." || s == "." || s.is_empty())
            || folder.len() > 255
            || folder.contains(['/', '\\', '\0'])
            || matches!(folder, "." | "..")
        {
            return Err(Error::Http(400, "invalid workspace file path"));
        }
        if !folder.is_empty()
            && !composition
                .git_workspaces()
                .iter()
                .any(|w| w.path == folder)
        {
            return Err(Error::Http(400, "unknown workspace folder"));
        }
        let absolute = text(format_args!(
            "/workspace/{}{}{}",
            folder,
            if folder.is_empty() { "" } else { "/" },
            path
        ))?;
        let mut paths = List::new();
        paths.push(absolute.try_clone()?)?;
        let client = capsule.client_id.try_clone()?;
        let payload = d::protocol::TrackedFilesPayload {
            runtime: capsule.try_clone()?,
            paths,
            ..Default::default()
        };
        let work = ReadFile {
            composition: composition.id.try_clone()?,
            path: try_string(path)?,
            absolute,
        };
        Ok(Some(Outcome::Capsule(self.enqueue_call(
            crate::capsules::Action::ReadFile(work),
            &client,
            d::protocol::METHOD_READ_TRACKED,
            &payload,
            now,
            random,
        )?)))
    }
    pub(crate) fn session_access(&self, id: &str, actor: &str) -> Result<&d::Session> {
        let session = self.store.session(id)?;
        if session.operator != actor
            && !self
                .store
                .job(&session.job_id)
                .is_ok_and(|j| j.owner == actor || j.assignee == actor)
        {
            return Err(Error::Http(403, "session belongs to another operator"));
        }
        Ok(session)
    }
    pub(crate) fn manage_login(&self, id: &str, user: &d::User) -> Result {
        let key = &self.store.login(id)?.key;
        self.manage_login_key(key, user)
    }
    pub(crate) fn manage_login_key(&self, key: &str, user: &d::User) -> Result {
        if user.role == d::USER_ADMIN || key.split('/').next() == Some(&user.username) {
            return Ok(());
        }
        for layer in self.store.snapshot()?.artifacts.iter() {
            if spin_store::layer_key(layer)? == key && layer.created_by == user.username {
                return Ok(());
            }
        }
        Err(Error::Http(403, "layer owner or admin required"))
    }
    fn manifest_entries(
        &mut self,
        key: &str,
        contents: Option<&d::LayerContents>,
    ) -> Result<List<d::ContentEntry>> {
        let Some(contents) = contents else {
            return Ok(List::new());
        };
        if !contents.entries.is_empty() {
            return Ok(contents.entries.try_clone()?);
        }
        let reference = text(format_args!("manifest:{key}"))?;
        match self.store.read_blob(&reference, 64 << 20) {
            Ok(bytes) => Ok(List::<d::ContentEntry>::from_json_with_limit(
                &bytes,
                64 << 20,
            )?),
            Err(spin_store::Error::NotFound) => Ok(List::new()),
            Err(error) => Err(error.into()),
        }
    }
    pub(crate) fn management_route(
        &mut self,
        req: &Request<'_>,
        user: &d::User,
    ) -> Result<Option<Response>> {
        let mut path = req.path.trim_start_matches('/').split('/');
        let route = [
            path.next(),
            path.next(),
            path.next(),
            path.next(),
            path.next(),
        ];
        if path.next().is_some() {
            return Ok(None);
        }
        let response = match (req.method, route) {
            ("GET", [Some("api"), Some("artifacts"), Some(id), Some("tree"), None]) => {
                let layer = self.store.artifact(id)?;
                let mut members = List::<Value>::new();
                for candidate in self.store.artifact_tree(id)?.iter().filter(|a| a.id != id) {
                    members.push(http::object(&[
                        ("id", Value::string(&candidate.id)?),
                        (
                            "layer",
                            Value::string(&text(format_args!(
                                "{}:{}",
                                candidate.kind, candidate.name
                            ))?)?,
                        ),
                        ("subject", Value::string(&candidate.subject)?),
                        (
                            "version",
                            Value::Bool(
                                candidate.kind == layer.kind
                                    && candidate.name == layer.name
                                    && candidate.subject == layer.subject,
                            ),
                        ),
                    ])?)?;
                }
                Response::json(200, &http::object(&[("members", members.to_value()?)])?)?
            }
            (
                "GET",
                [
                    Some("api"),
                    Some("artifacts"),
                    Some(id),
                    Some("contents"),
                    None,
                ],
            ) => {
                let layer = self.store.artifact(id)?.try_clone()?;
                let mut entries = self.manifest_entries(
                    &text(format_args!("artifact:{id}"))?,
                    layer.snapshot.contents.as_ref(),
                )?;
                for login in self
                    .store
                    .logins_for(&spin_store::layer_key(&layer)?)?
                    .iter()
                {
                    for (path, bytes) in login.files.iter() {
                        if let Some(entry) =
                            entries.as_mut_slice().iter_mut().find(|e| e.path == path)
                        {
                            entry.logins.push(login.number)?;
                        } else {
                            if entries.len() >= 100_000 {
                                return Err(Error::Http(413, "manifest exceeds entry budget"));
                            }
                            let mut logins = List::new();
                            logins.push(login.number)?;
                            entries.push(d::ContentEntry {
                                path: try_string(path)?,
                                bytes: bytes.0.as_ref().map_or(0, |b| b.len()) as i64,
                                source: try_string("login")?,
                                logins,
                            })?;
                        }
                    }
                }
                for entry in entries.as_mut_slice() {
                    entry.logins.as_mut_slice().sort_unstable();
                }
                entries
                    .as_mut_slice()
                    .sort_unstable_by(|a, b| a.path.cmp(&b.path));
                Response::json(
                    200,
                    &http::object(&[
                        ("contents", layer.snapshot.contents.to_value()?),
                        ("entries", entries.to_value()?),
                    ])?,
                )?
            }
            (
                "GET",
                [
                    Some("api"),
                    Some("compositions"),
                    Some(id),
                    Some("changes"),
                    None,
                ],
            ) => {
                let contents = self.store.composition(id)?.capsule_changes.try_clone()?;
                let entries = self.manifest_entries(
                    &text(format_args!("composition:{id}"))?,
                    contents.as_ref(),
                )?;
                Response::json(
                    200,
                    &http::object(&[
                        ("contents", contents.to_value()?),
                        ("entries", entries.to_value()?),
                    ])?,
                )?
            }
            (
                "PUT",
                [
                    Some("api"),
                    Some("artifacts"),
                    Some(id),
                    Some("enablements"),
                    Some(name),
                ],
            ) => {
                let value: Value = Self::decode(req)?;
                let command = value
                    .as_object()
                    .and_then(|o| o.get("command"))
                    .and_then(Value::as_str)
                    .ok_or(Error::Http(400, "command is required"))?;
                Response::json(
                    200,
                    &self
                        .store
                        .set_artifact_enablement_command(id, name, command)?,
                )?
            }
            (
                "PUT",
                [
                    Some("api"),
                    Some("artifacts"),
                    Some(id),
                    Some("acp"),
                    Some("settings"),
                ],
            ) => {
                let target = self.store.enabling_layer(id, "acp")?.id.try_clone()?;
                Response::json(
                    200,
                    &self
                        .store
                        .set_artifact_agent_settings(&target, Self::decode(req)?)?,
                )?
            }
            (
                "PUT",
                [
                    Some("api"),
                    Some("artifacts"),
                    Some(id),
                    Some("tracked"),
                    None,
                ],
            ) => {
                let value: Value = Self::decode(req)?;
                let fields = value
                    .as_object()
                    .ok_or(Error::Http(400, "tracked selection is required"))?;
                let paths =
                    List::<String>::from_value(fields.get("paths").unwrap_or(&Value::Null))?;
                let excludes =
                    List::<String>::from_value(fields.get("excludes").unwrap_or(&Value::Null))?;
                Response::json(
                    200,
                    &self.store.set_artifact_tracked(id, &paths, &excludes)?,
                )?
            }
            ("GET", [Some("api"), Some("logins"), Some(id), Some("files"), None]) => {
                // This returns paths and sizes, never the captured credentials.
                Response::json(200, &self.store.login_files(id)?)?
            }
            ("PUT", [Some("api"), Some("logins"), Some(id), Some("name"), None]) => {
                self.manage_login(id, user)?;
                let value: Value = Self::decode(req)?;
                let name = value
                    .as_object()
                    .and_then(|o| o.get("name"))
                    .and_then(Value::as_str)
                    .ok_or(Error::Http(400, "name is required"))?;
                self.store.rename_login(id, name)?;
                let summary = self
                    .store
                    .login_summaries()?
                    .into_vec()
                    .into_iter()
                    .find(|l| l.id == id)
                    .ok_or(Error::Http(404, "not found"))?;
                Response::json(200, &summary)?
            }
            _ => return Ok(None),
        };
        Ok(Some(response))
    }
}
