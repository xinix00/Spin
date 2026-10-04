//! Duurzame API-opdrachten worden onder de ene Store-eigenaar uitgevoerd.
use super::*;
use spin_core::validation::text;
use spin_domain::{List, json, try_push_str, try_string};
use spin_store::{Context, Mutation};

/// Het gebruikersonafhankelijke deel van het staatdocument, één keer per versie
/// opgebouwd en door elke browser en elke `GET /api/state` van die versie gedeeld.
/// De eigenaar is single-threaded: één serialisatie van het hele document kost
/// hem honderden milliseconden, dus hij doet die niet per kijker.
pub(crate) struct StateCache {
    version: u64,
    /// De gedeelde collecties en `recommendations` als JSON-leden, elk met een
    /// komma ervoor: `,"jobs":[...],"sessions":[...]`.
    shared: String,
    // De vier collecties met een per-gebruiker filter blijven gesorteerde waarden.
    artifacts: List<d::Artifact>,
    recordings: List<d::Recording>,
    mcp_servers: List<d::MCPServer>,
    git_accounts: List<d::GitAccount>,
}
const USER_KEYS: [&str; 4] = ["artifacts", "recordings", "mcp_servers", "git_accounts"];

/// Een al geserialiseerd staatdocument; `to_json` levert de tekst zonder hertolking.
#[derive(Default)]
pub(crate) struct StateDocument(pub(crate) String);
impl Wire for StateDocument {
    fn from_value(value: &Value) -> d::Fallible<Self> {
        Ok(Self(json::to_string(value)?))
    }
    fn to_value(&self) -> d::Fallible<Value> {
        json::parse_str(&self.0)
    }
    fn to_json(&self) -> d::Fallible<String> {
        try_string(&self.0)
    }
}
/// Schrijft `,"key":` als volgend lid van het object in opbouw.
fn member(out: &mut String, key: &str) -> Result {
    try_push_str(out, ",")?;
    json::write_string(key, out)?;
    Ok(try_push_str(out, ":")?)
}
/// Schrijft een JSON-array van de gegeven elementen zonder ze te kopiëren.
fn array<'a, T: Wire + 'a>(out: &mut String, items: impl Iterator<Item = &'a T>) -> Result {
    try_push_str(out, "[")?;
    for (index, item) in items.enumerate() {
        if index > 0 {
            try_push_str(out, ",")?;
        }
        json::write(&item.to_value()?, out)?;
    }
    Ok(try_push_str(out, "]")?)
}
impl<P: Persistence> Server<P> {
    /// Bouwt het gedeelde deel opnieuw zodra de versie (Store plus weergave) verschilt.
    fn ensure_state_cache(&mut self) -> Result {
        let version = self.version();
        if self
            .state_cache
            .as_ref()
            .is_some_and(|c| c.version == version)
        {
            return Ok(());
        }
        // Eerst het oude geheugen vrij, dan pas de nieuwe kopie van de staat.
        self.state_cache = None;
        let mut snapshot = self.store.snapshot()?;
        let mut recommendations = spin_core::orchestrator::recommend(&snapshot)?;
        if recommendations.is_empty() {
            recommendations = List::new();
        }
        let artifacts = core::mem::take(&mut snapshot.artifacts);
        let recordings = core::mem::take(&mut snapshot.recordings);
        let mcp_servers = core::mem::take(&mut snapshot.mcp_servers);
        let git_accounts = core::mem::take(&mut snapshot.git_accounts);
        let Value::Object(object) = snapshot.to_value()? else {
            return Err(Error::Http(500, "invalid snapshot"));
        };
        drop(snapshot);
        let mut shared = String::new();
        for (key, value) in object.iter() {
            if USER_KEYS.contains(&key) {
                continue;
            }
            member(&mut shared, key)?;
            json::write(value, &mut shared)?;
        }
        member(&mut shared, "recommendations")?;
        json::write(&recommendations.to_value()?, &mut shared)?;
        self.state_cache = Some(StateCache {
            version,
            shared,
            artifacts,
            recordings,
            mcp_servers,
            git_accounts,
        });
        Ok(())
    }
    fn state_cache(&mut self) -> Result<&StateCache> {
        self.ensure_state_cache()?;
        self.state_cache
            .as_ref()
            .ok_or(Error::Http(500, "state cache unavailable"))
    }
    /// Het staatdocument voor één gebruiker: de gedeelde tekst plus zijn eigen
    /// artefacten, opnames, MCP-servers, git-accounts en de vluchtige velden.
    pub(super) fn state_for(&mut self, user: &d::User) -> Result<StateDocument> {
        let public = d::PublicUser {
            id: user.id.try_clone()?,
            username: user.username.try_clone()?,
            display_name: user.display_name.try_clone()?,
            role: user.role.try_clone()?,
            archived_at: user.archived_at.try_clone()?,
            created_at: user.created_at.try_clone()?,
        };
        let volatile = [
            ("version", Value::uint(self.version())),
            ("preparing", self.preparation_state()?),
            ("git_oauth_providers", self.oauth_providers()?),
            ("engine", self.runner_info()?.to_value()?),
            ("storage", self.storage_report.try_clone()?),
        ];
        let cache = self.state_cache()?;
        let mut out = String::new();
        out.try_reserve(cache.shared.len() + (64 << 10))
            .map_err(|_| d::Error::OutOfMemory)?;
        try_push_str(&mut out, "{\"current_user\":")?;
        json::write(&public.to_value()?, &mut out)?;
        try_push_str(&mut out, &cache.shared)?;
        let me = user.username.as_str();
        member(&mut out, "artifacts")?;
        array(
            &mut out,
            cache
                .artifacts
                .iter()
                .filter(|a| a.scope != d::SCOPE_USER || a.subject == me),
        )?;
        member(&mut out, "recordings")?;
        array(&mut out, cache.recordings.iter().filter(|r| r.actor == me))?;
        member(&mut out, "mcp_servers")?;
        array(
            &mut out,
            cache.mcp_servers.iter().filter(|s| s.operator == me),
        )?;
        member(&mut out, "git_accounts")?;
        array(
            &mut out,
            cache
                .git_accounts
                .iter()
                .filter(|a| a.credential_scope == d::CREDENTIAL_SCOPE_GLOBAL || a.operator == me),
        )?;
        for (key, value) in &volatile {
            member(&mut out, key)?;
            json::write(value, &mut out)?;
        }
        try_push_str(&mut out, "}")?;
        Ok(StateDocument(out))
    }
    pub(super) fn route(
        &mut self,
        req: Request<'_>,
        user: d::User,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Response> {
        let operator = user.username.as_str();
        match (req.method, req.path) {
            ("GET", "/api/storage") => {
                // Het rapport van de onderhoudsronde volstaat; alleen vóór de
                // eerste ronde is er nog geen.
                if self.storage_report.is_null() {
                    self.refresh_storage(now)?;
                }
                return Response::json(200, &self.storage_report);
            }
            ("GET" | "POST", "/api/runners/token") => {
                if user.role != d::USER_ADMIN {
                    return Err(Error::Http(403, "admin role required"));
                }
                if req.method == "POST" {
                    self.store.replace_worker_token(&text(format_args!(
                        "spw_{}",
                        auth::token(runtime)?
                    ))?)?;
                }
                return Response::json(
                    200,
                    &http::object(&[("token", Value::string(self.store.worker_token())?)])?,
                );
            }
            ("GET", "/api/state") => {
                // De tekst is al JSON; geen tweede kopie via `Response::json`.
                let mut response = Response::empty(200)?;
                response.header("Content-Type", "application/json")?;
                response.body = self.state_for(&user)?.0.into_bytes();
                return Ok(response);
            }
            ("GET", "/api/artifacts") => {
                let mut body = String::new();
                array(
                    &mut body,
                    self.state_cache()?
                        .artifacts
                        .iter()
                        .filter(|a| a.scope != d::SCOPE_USER || a.subject == operator),
                )?;
                let mut response = Response::empty(200)?;
                response.header("Content-Type", "application/json")?;
                response.body = body.into_bytes();
                return Ok(response);
            }
            ("POST", "/api/jobs") => {
                let mut value: d::CreateJobRequest = Self::decode(&req)?;
                value.operator = try_string(operator)?;
                if value.owner.is_empty() {
                    value.owner = try_string(operator)?;
                }
                let mut created = self
                    .store
                    .create_job(value, Mutation { now, ids: runtime })?;
                // Zoals in Go is Run alleen nog een legacy veld: een Job krijgt
                // altijd een capsule en een remote Job-branch via de achtergrondstart.
                if let Err(error) = self.schedule_session(&created.session, now, runtime) {
                    created.run_error = text(format_args!("{error}"))?;
                }
                return Response::json(202, &created);
            }
            ("POST", "/api/workflow-templates") => {
                let mut value: d::CreateWorkflowTemplateRequest = Self::decode(&req)?;
                value.operator = try_string(operator)?;
                let id = runtime.next("tpl")?;
                return Response::json(
                    201,
                    &self
                        .store
                        .create_workflow_template(value, Context { now, id: &id })?,
                );
            }
            ("POST", "/api/mcp-servers") => {
                let mut value: d::CreateMCPServerRequest = Self::decode(&req)?;
                value.operator = try_string(operator)?;
                let id = runtime.next("mcp")?;
                return Response::json(
                    201,
                    &self
                        .store
                        .create_mcp_server(value, Context { now, id: &id })?,
                );
            }
            ("POST", "/api/git/repositories") => {
                let mut value: d::CreateGitRepositoryRequest = Self::decode(&req)?;
                value.operator = try_string(operator)?;
                let id = runtime.next("repo")?;
                return Response::json(
                    201,
                    &self
                        .store
                        .create_git_repository(value, Context { now, id: &id })?,
                );
            }
            ("POST", "/api/git/accounts") => {
                let value: d::CreateGitAccountRequest = Self::decode(&req)?;
                let account = d::GitAccount {
                    operator: try_string(operator)?,
                    provider: value.provider,
                    host: value.host,
                    provider_id: value.provider_id,
                    login: value.login,
                    name: value.name,
                    email: value.email,
                    access_token: value.access_token,
                    credential_scope: value.credential_scope,
                    ..Default::default()
                };
                let id = runtime.next("git")?;
                return Response::json(
                    201,
                    &self
                        .store
                        .save_git_account(account, Context { now, id: &id })?,
                );
            }
            _ => {}
        }
        let mut path = req
            .path
            .strip_prefix("/api/")
            .ok_or(Error::Http(404, "not found"))?
            .split('/');
        let route = [
            path.next(),
            path.next(),
            path.next(),
            path.next(),
            path.next(),
        ];
        if path.next().is_some() {
            return Err(Error::Http(404, "not found"));
        }
        match (req.method, route) {
            (
                "POST",
                [
                    Some("clients"),
                    Some(id),
                    Some(action @ ("drain" | "resume")),
                    None,
                    None,
                ],
            ) => {
                if user.role != d::USER_ADMIN {
                    return Err(Error::Http(403, "admin role required"));
                }
                let connected = self
                    .runners
                    .iter()
                    .any(|p| p.client().id == id && p.is_connected());
                let client = self
                    .store
                    .set_client_draining(id, action == "drain", connected)?;
                if let Some(peer) = self.runners.iter_mut().find(|p| p.client().id == id) {
                    peer.update_client(client.try_clone()?)?;
                }
                Response::json(200, &client)
            }
            ("DELETE", [Some("clients"), Some(id), None, None, None]) => {
                if user.role != d::USER_ADMIN {
                    return Err(Error::Http(403, "admin role required"));
                }
                if self
                    .runners
                    .iter()
                    .any(|p| p.client().id == id && p.is_connected())
                {
                    return Err(Error::Http(409, "runner is connected"));
                }
                self.store.remove_client(id)?;
                self.runners.retain(|p| p.client().id != id);
                Response::empty(204)
            }
            ("PUT", [Some("workflow-templates"), Some(id), None, None, None]) => {
                let mut value: d::CreateWorkflowTemplateRequest = Self::decode(&req)?;
                value.operator = try_string(operator)?;
                Response::json(200, &self.store.update_workflow_template(id, value, now)?)
            }
            ("DELETE", [Some("workflow-templates"), Some(id), None, None, None]) => {
                Response::json(200, &self.store.delete_workflow_template(id, operator)?)
            }
            ("DELETE", [Some("mcp-servers"), Some(id), None, None, None]) => {
                Response::json(200, &self.store.delete_mcp_server(id, operator)?)
            }
            ("PUT", [Some("git"), Some("repositories"), Some(id), None, None]) => {
                let mut value: d::UpdateGitRepositoryRequest = Self::decode(&req)?;
                value.operator = try_string(operator)?;
                Response::json(200, &self.store.update_git_repository(id, value, now)?)
            }
            ("DELETE", [Some("git"), Some("repositories"), Some(id), None, None]) => {
                Response::json(200, &self.store.delete_git_repository(id, operator)?)
            }
            ("DELETE", [Some("git"), Some("accounts"), Some(id), None, None]) => {
                Response::json(200, &self.store.delete_git_account(id, operator)?)
            }
            (
                "PUT",
                [
                    Some("git"),
                    Some("oauth"),
                    Some(provider),
                    Some("configuration"),
                    None,
                ],
            ) => {
                let mut value: d::SaveGitOAuthConfigurationRequest = Self::decode(&req)?;
                value.provider = try_string(provider)?;
                Response::json(
                    200,
                    &self
                        .store
                        .save_git_oauth_configuration(&user.id, value, now)?,
                )
            }
            (
                "DELETE",
                [
                    Some("git"),
                    Some("oauth"),
                    Some(provider),
                    Some("configuration"),
                    None,
                ],
            ) => Response::json(
                200,
                &self
                    .store
                    .delete_git_oauth_configuration(&user.id, provider)?,
            ),
            ("PUT", [Some("jobs"), Some(id), Some("assignee"), None, None]) => {
                let value: d::AssignJobRequest = Self::decode(&req)?;
                Response::json(
                    200,
                    &self.store.assign_job(id, operator, &value.assignee, now)?,
                )
            }
            ("PUT", [Some("jobs"), Some(id), Some("environment"), None, None]) => Response::json(
                200,
                &self
                    .store
                    .update_job_environment(id, operator, Self::decode(&req)?, now)?,
            ),
            ("POST", [Some("jobs"), Some(id), Some("select-result"), None, None]) => {
                let value: d::SelectResultRequest = Self::decode(&req)?;
                Response::json(200, &self.store.select_result(id, &value, now)?)
            }
            (
                "POST",
                [
                    Some("workflow"),
                    Some("questions"),
                    Some(id),
                    Some("answer"),
                    None,
                ],
            ) => {
                let value: d::AnswerWorkflowQuestionRequest = Self::decode(&req)?;
                if value.action.eq_ignore_ascii_case("answer") {
                    let question =
                        self.store
                            .answer_workflow_questions(id, operator, &value.answers, now)?;
                    let prompt = spin_core::prompts::answers(&question)?;
                    // Het antwoord is duurzaam; een offline runner wordt door de launch-sweep
                    // hervat met dezelfde antwoorden in de volledige fasecontext.
                    let _ = self.queue_workflow_prompt(&question.session_id, prompt, now, runtime);
                    self.last_launch_sweep = None;
                    return Response::json(
                        200,
                        &d::WorkflowAdvance {
                            question: Some(question),
                            ..Default::default()
                        },
                    );
                }
                let advance = self.store.answer_workflow_question(
                    id,
                    operator,
                    &value.action,
                    &value.reason,
                    Mutation { now, ids: runtime },
                )?;
                if let Some(session) = &advance.next_session {
                    let _ = self.schedule_session(session, now, runtime);
                }
                Response::json(200, &advance)
            }
            ("GET", [Some("code-reviews"), Some(id), None, None, None]) => {
                Response::json(200, &self.store.code_review_bundle(id)?)
            }
            ("POST", [Some("code-reviews"), Some(id), Some("comments"), None, None]) => {
                let value = Self::decode(&req)?;
                let comment_id = runtime.next("com")?;
                Response::json(
                    201,
                    &self.store.add_code_review_comment(
                        id,
                        operator,
                        value,
                        Context {
                            now,
                            id: &comment_id,
                        },
                    )?,
                )
            }
            ("POST", [Some("deliverables"), Some(id), Some("comments"), None, None]) => {
                let value = Self::decode(&req)?;
                let comment_id = runtime.next("com")?;
                Response::json(
                    201,
                    &self.store.add_deliverable_comment(
                        id,
                        operator,
                        value,
                        Context {
                            now,
                            id: &comment_id,
                        },
                    )?,
                )
            }
            ("POST", [Some("deliverables"), Some(id), Some("share"), None, None]) => {
                let value = if req.body.is_empty() {
                    Value::Null
                } else {
                    d::json::parse(req.body)?
                };
                let share = match value.as_object().and_then(|v| v.get("share")) {
                    None | Some(Value::Null) => true,
                    Some(Value::Bool(value)) => *value,
                    _ => return Err(Error::Http(400, "share must be boolean")),
                };
                let token = runtime.next("shr")?;
                let delivery =
                    self.store
                        .share_deliverable(id, share, Context { now, id: &token })?;
                let url = if delivery.share_token.is_empty() {
                    String::new()
                } else {
                    text(format_args!(
                        "{}://{}/share/{}/",
                        if req.secure { "https" } else { "http" },
                        req.header("Host"),
                        delivery.share_token
                    ))?
                };
                Response::json(
                    200,
                    &http::object(&[
                        ("share_token", Value::string(&delivery.share_token)?),
                        ("url", Value::string(&url)?),
                        (
                            "expires_at",
                            Value::string(
                                delivery
                                    .share_expires_at
                                    .as_ref()
                                    .map_or("", Timestamp::as_str),
                            )?,
                        ),
                    ])?,
                )
            }
            ("POST", [Some("deliverables"), Some(id), Some("preview"), None, None]) => {
                let token = runtime.next("pvw")?;
                let delivery = self
                    .store
                    .ensure_preview_token(id, Context { now, id: &token })?;
                Response::json(
                    200,
                    &http::object(&[(
                        "url",
                        Value::string(&text(format_args!(
                            "/preview/{}/",
                            delivery.preview_token
                        ))?)?,
                    )])?,
                )
            }
            _ => Err(Error::Http(404, "not found")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use d::state::PersistedState;
    struct Memory;
    impl Persistence for Memory {
        fn save(&mut self, _: &PersistedState) -> spin_store::Result {
            Ok(())
        }
    }
    fn user(name: &str) -> d::User {
        d::User {
            id: try_string(name).unwrap(),
            username: try_string(name).unwrap(),
            ..Default::default()
        }
    }
    #[test]
    fn shared_state_is_serialised_once_per_version_and_user_parts_stay_private() {
        let state = PersistedState::from_json(br#"{
            "artifacts":{
                "pub":{"id":"pub","kind":"tool","name":"shared","created_at":"2026-09-30T10:00:00Z"},
                "a":{"id":"a","kind":"tool","name":"mine","scope":"user","subject":"anna","created_at":"2026-09-30T11:00:00Z"},
                "b":{"id":"b","kind":"tool","name":"theirs","scope":"user","subject":"bram","created_at":"2026-09-30T12:00:00Z"}},
            "recordings":{"r":{"id":"r","actor":"anna","started_at":"2026-09-30T12:00:00Z"}},
            "mcp_servers":{"m":{"id":"m","operator":"bram","created_at":"2026-09-30T12:00:00Z"}},
            "git_accounts":{
                "g":{"id":"g","operator":"bram","provider":"github","login":"bram-gh","credential_scope":"user"},
                "s":{"id":"s","operator":"bram","provider":"github","login":"shared-gh","credential_scope":"global"}},
            "jobs":{"j":{"id":"j","title":"Job","status":"running","created_at":"2026-09-30T12:00:00Z"}}
        }"#).unwrap();
        let mut server = Server::new(Store::new(state, Memory));
        let anna = server.state_for(&user("anna")).unwrap().to_json().unwrap();
        let parsed = d::json::parse_str(&anna).unwrap();
        let object = parsed.as_object().unwrap();
        for key in [
            "artifacts",
            "recordings",
            "mcp_servers",
            "git_accounts",
            "jobs",
            "logins",
            "recommendations",
        ] {
            assert!(object.get(key).unwrap().as_array().is_some(), "{key}");
        }
        assert!(anna.contains("\"mine\"") && anna.contains("\"shared\""));
        assert!(!anna.contains("\"theirs\"") && !anna.contains("bram-gh"));
        assert!(anna.contains("\"shared-gh\"") && anna.contains("\"actor\":\"anna\""));
        assert!(anna.contains("\"mcp_servers\":[]"));
        assert_eq!(object.get("version"), Some(&Value::uint(0)));
        let bram = server.state_for(&user("bram")).unwrap().0;
        assert!(bram.contains("\"theirs\"") && bram.contains("bram-gh"));
        assert!(bram.contains("\"mcp_servers\":[{"));
        assert!(!bram.contains("\"mine\"") && !bram.contains("\"actor\":\"anna\""));
        // Dezelfde versie hergebruikt de gedeelde tekst letterlijk; een marker
        // in de cache komt bij elke kijker terug zonder nieuwe serialisatie.
        let cache = server.state_cache.as_mut().unwrap();
        try_push_str(&mut cache.shared, ",\"marker\":true").unwrap();
        let again = server.state_for(&user("anna")).unwrap().0;
        assert!(again.contains("\"marker\":true"));
        assert!(d::json::parse_str(&again).is_ok());
        // Een nieuwe versie bouwt opnieuw op.
        server.display_changed();
        let fresh = server.state_for(&user("anna")).unwrap().0;
        assert!(!fresh.contains("\"marker\""));
        assert!(fresh.contains("\"version\":1"));
        assert_eq!(server.state_cache.as_ref().unwrap().version, 1);
    }
}
