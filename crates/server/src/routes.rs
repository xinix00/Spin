//! Duurzame API-opdrachten worden onder de ene Store-eigenaar uitgevoerd.
use super::*;
use spin_core::validation::text;
use spin_domain::{List, try_string};
use spin_store::{Context, Mutation};
impl<P: Persistence> Server<P> {
    pub(super) fn state_for(&self, user: &d::User) -> Result<Value> {
        let mut state = self.store.snapshot()?;
        state
            .artifacts
            .retain(|a| a.scope != d::SCOPE_USER || a.subject == user.username);
        state.recordings.retain(|r| r.actor == user.username);
        state.mcp_servers.retain(|s| s.operator == user.username);
        state.git_accounts.retain(|a| {
            a.credential_scope == d::CREDENTIAL_SCOPE_GLOBAL || a.operator == user.username
        });
        let mut recommendations = spin_core::orchestrator::recommend(&state)?;
        if recommendations.is_empty() {
            recommendations = List::new();
        }
        let Value::Object(mut value) = state.to_value()? else {
            return Err(Error::Http(500, "invalid snapshot"));
        };
        let public = d::PublicUser {
            id: user.id.try_clone()?,
            username: user.username.try_clone()?,
            display_name: user.display_name.try_clone()?,
            role: user.role.try_clone()?,
            archived_at: user.archived_at.try_clone()?,
            created_at: user.created_at.try_clone()?,
        };
        value.push("current_user", public.to_value()?)?;
        value.push("version", Value::uint(self.version()))?;
        value.push("recommendations", recommendations.to_value()?)?;
        value.push("preparing", self.preparation_state()?)?;
        value.push("git_oauth_providers", self.oauth_providers()?)?;
        value.push("engine", self.runner_info()?.to_value()?)?;
        value.push("storage", self.storage_report.try_clone()?)?;
        Ok(Value::Object(value))
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
                self.refresh_storage(now)?;
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
            ("GET", "/api/state") => return Response::json(200, &self.state_for(&user)?),
            ("GET", "/api/artifacts") => {
                let mut artifacts = self.store.snapshot()?.artifacts;
                artifacts.retain(|a| a.scope != d::SCOPE_USER || a.subject == operator);
                return Response::json(200, &artifacts);
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
