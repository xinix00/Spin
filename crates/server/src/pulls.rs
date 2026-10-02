//! Eén pull request per gewijzigde repository; lookup maakt herstart en herhaling idempotent.
use super::*;
use crate::external::{NetworkRequest, NetworkResponse, Work, request};
use crate::oauth::{encode, field};
use alloc::vec::Vec;
use d::{List, try_string};
use spin_core::validation::text;

struct Repository {
    id: String,
    name: String,
    owner: String,
    endpoint: String,
    token: String,
    head: String,
    base: String,
}
pub(crate) struct Pull {
    pub(crate) session: String,
    repositories: List<Repository>,
    at: usize,
    create: bool,
    title: String,
    body: String,
    result: d::WorkflowActionResult,
}
fn next_request(work: &Pull) -> Result<NetworkRequest> {
    let repository = &work.repositories[work.at];
    let (method, url, body) = if work.create {
        (
            "POST",
            repository.endpoint.try_clone()?,
            http::object(&[
                ("title", Value::string(&work.title)?),
                ("body", Value::string(&work.body)?),
                ("head", Value::string(&repository.head)?),
                ("base", Value::string(&repository.base)?),
            ])?
            .to_json()?
            .into_bytes(),
        )
    } else {
        (
            "GET",
            text(format_args!(
                "{}?state=open&head={}&base={}",
                repository.endpoint,
                encode(&text(format_args!(
                    "{}:{}",
                    repository.owner, repository.head
                ))?)?,
                encode(&repository.base)?
            ))?,
            Vec::new(),
        )
    };
    let mut request = request(method, &url, &repository.token, body, "application/json")?;
    request.headers.push((
        try_string("X-GitHub-Api-Version")?,
        try_string("2022-11-28")?,
    ))?;
    Ok(request)
}
impl<P: Persistence> Server<P> {
    pub(crate) fn launch_pull(
        &mut self,
        session: &d::Session,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        if self
            .network
            .iter()
            .any(|c| matches!(&c.work,Work::Pull(w) if w.session==session.id))
        {
            return Ok(());
        }
        let view = self.store.workflow_for_session(&session.id)?;
        let workspaces = self.job_workspaces(&view.job)?;
        if workspaces.is_empty() {
            return Err(Error::Http(409, "Job has no repository for pull request"));
        }
        let mut repositories = List::new();
        for workspace in workspaces.iter() {
            let account = self
                .store
                .resolve_git_workspace_account(workspace, view.job.worker())?;
            self.credential_ready(account)?;
            if account.provider != "github" || !account.host.eq_ignore_ascii_case("github.com") {
                return Err(Error::Http(
                    409,
                    "pull requests require a github.com account",
                ));
            }
            let path = workspace
                .remote_url
                .strip_prefix("https://github.com/")
                .ok_or(Error::Http(
                    409,
                    "pull requests require an HTTPS github.com repository",
                ))?
                .trim_end_matches('/')
                .trim_end_matches(".git");
            let (owner, name) = path
                .split_once('/')
                .ok_or(Error::Http(409, "invalid GitHub repository"))?;
            if owner.is_empty() || name.is_empty() || name.contains('/') {
                return Err(Error::Http(409, "invalid GitHub repository"));
            }
            let base = if workspace.bootstrap_ref.is_empty() {
                &view.job.base_ref
            } else {
                &workspace.bootstrap_ref
            };
            repositories.push(Repository {
                id: workspace.repository_id.try_clone()?,
                name: workspace.repository_name.try_clone()?,
                owner: try_string(owner)?,
                endpoint: text(format_args!(
                    "https://api.github.com/repos/{}/{}/pulls",
                    encode(owner)?,
                    encode(name)?
                ))?,
                token: account.access_token.try_clone()?,
                head: try_string(view.job.branch.trim_start_matches("refs/heads/"))?,
                base: try_string(
                    base.trim_start_matches("refs/heads/")
                        .trim_start_matches("origin/"),
                )?,
            })?;
        }
        let work = Pull {
            session: session.id.try_clone()?,
            repositories,
            at: 0,
            create: false,
            title: view.job.title,
            body: view.job.objective,
            result: d::WorkflowActionResult {
                r#type: try_string(d::WORKFLOW_ACTION_GIT_PULL_REQUEST)?,
                results: d::WireMap::new(),
                created_at: now.try_clone()?,
                ..Default::default()
            },
        };
        let request = next_request(&work)?;
        self.store.mark_workflow_phase_running(&session.id, now)?;
        if let Err(error) = self.queue_network(request, Work::Pull(work), now, random) {
            self.store.requeue_workflow_phase(&session.id)?;
            return Err(error);
        }
        Ok(())
    }
    pub(crate) fn advance_pull(
        &mut self,
        index: usize,
        reply: Result<NetworkResponse>,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let reply = reply?;
        let call = &mut self.network[index];
        let Work::Pull(work) = &mut call.work else {
            return Err(Error::Http(500, "missing pull request operation"));
        };
        let view = self.store.workflow_for_session(&work.session)?;
        if view.job.current_phase_run_id != view.run.id || view.run.status != d::PHASE_RUN_RUNNING {
            return Err(Error::Http(
                409,
                "workflow phase changed during pull request",
            ));
        }
        if reply.status != if work.create { 201 } else { 200 } {
            return Err(Error::Http(502, "GitHub pull request operation failed"));
        }
        let value = Value::from_json(&reply.body)?;
        let pull = if work.create {
            Some(&value)
        } else {
            value
                .as_array()
                .ok_or(Error::Http(502, "invalid pull request lookup"))?
                .first()
        };
        if let Some(pull) = pull {
            let number = field(pull, "number")
                .as_i64()
                .filter(|n| *n > 0)
                .ok_or(Error::Http(502, "invalid pull request number"))?;
            let url = field(pull, "html_url")
                .as_str()
                .filter(|u| u.starts_with("https://github.com/"))
                .ok_or(Error::Http(502, "invalid pull request URL"))?;
            let repository = &work.repositories[work.at];
            work.result
                .results
                .insert(repository.id.try_clone()?, try_string(url)?)?;
            if work.at == 0 {
                work.result.external_id = text(format_args!("{number}"))?;
                work.result.url = try_string(url)?;
            }
            if !work.result.detail.is_empty() {
                d::try_push_str(&mut work.result.detail, "; ")?;
            }
            d::try_push_str(
                &mut work.result.detail,
                &text(format_args!(
                    "{}: {} pull request: {url}",
                    repository.name,
                    if work.create { "Nieuwe" } else { "Bestaande" }
                ))?,
            )?;
            work.at += 1;
            work.create = false;
            if work.at == work.repositories.len() {
                let id = work.session.try_clone()?;
                let result = work.result.try_clone()?;
                let detail = result.detail.try_clone()?;
                self.store.set_workflow_action_result(&id, result, now)?;
                return self
                    .finish_action(&id, "accept", &detail, false, now, random)
                    .map(Some);
            }
        } else {
            work.create = true;
        }
        let mut request = next_request(work)?;
        request.id = call.id.try_clone()?;
        call.request = Some(request);
        Ok(None)
    }
}
