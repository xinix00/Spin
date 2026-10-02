//! Een Job bewaart zijn repositorykeuze en template; iedere Session krijgt een eigen branch.
use crate::{Context, Error, Mutation, Persistence, Result, Store};
use alloc::string::String;
use spin_core::validation::{
    normalized, selector, selectors, text, valid_git_base_ref, valid_job_reference,
};
use spin_domain::{self as d, List, Map, Timestamp, TryClone, state::PersistedState, try_string};

fn unique(values: impl IntoIterator<Item = impl AsRef<str>>) -> Result<List<String>> {
    let mut out = List::new();
    for value in values {
        let value = value.as_ref().trim();
        if !value.is_empty() && !out.iter().any(|v| v == value) {
            out.push(try_string(value)?)?;
        }
    }
    Ok(out)
}
fn slug(value: &str) -> Result<String> {
    let mut out = String::new();
    let mut dash = false;
    for c in normalized(value)?.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            let mut buf = [0; 4];
            d::try_push_str(&mut out, c.encode_utf8(&mut buf))?;
            dash = false;
        } else if !out.is_empty() && !dash {
            d::try_push_str(&mut out, "-")?;
            dash = true;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out = try_string("work")?;
    }
    Ok(out)
}
fn suffix(id: &str) -> &str {
    let id = id.strip_prefix("job_").unwrap_or(id);
    let mut end = id.len().min(6);
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    &id[..end]
}
fn ensure_branch(job: &mut d::Job) -> Result {
    if !job.branch.starts_with("jobs/") || !job.branch.ends_with("/main") {
        job.branch = text(format_args!(
            "jobs/{}-{}/main",
            slug(&job.title)?,
            suffix(&job.id)
        ))?;
    }
    Ok(())
}
fn git_ref(job: &d::Job, id: &str) -> Result<String> {
    Ok(text(format_args!(
        "{}/sessions/{id}",
        job.branch.strip_suffix("/main").unwrap_or(&job.branch)
    ))?)
}
fn mcp(state: &PersistedState, operator: &str, ids: &[String]) -> Result {
    for id in ids {
        if state
            .mcp_servers
            .get(id)
            .is_none_or(|s| s.operator != operator)
        {
            return Err(Error::NotFound);
        }
    }
    Ok(())
}
fn repositories(
    state: &PersistedState,
    requests: &[d::JobRepositoryRequest],
    owner: &str,
) -> Result<List<d::JobRepository>> {
    let mut out = List::<d::JobRepository>::new();
    for req in requests {
        let repository = state
            .git_repositories
            .get(req.repository_id.trim())
            .ok_or(Error::NotFound)?;
        if out.iter().any(|r| r.repository_id == repository.id) {
            return Err(Error::Conflict("repository named twice"));
        }
        let mut mode = normalized(&req.mode)?;
        if mode.is_empty() {
            mode = try_string(d::REPOSITORY_MODE_CHANGE)?;
        }
        if !matches!(
            mode.as_str(),
            d::REPOSITORY_MODE_CHANGE | d::REPOSITORY_MODE_REFERENCE
        ) {
            return Err(Error::Conflict(
                "repository mode must be change or reference",
            ));
        }
        let base = if req.base_ref.trim().is_empty() {
            &repository.default_ref
        } else {
            req.base_ref.trim()
        };
        if !valid_git_base_ref(base) {
            return Err(Error::Conflict("invalid repository base ref"));
        }
        if repository.credential_scope != d::CREDENTIAL_SCOPE_PUBLIC {
            crate::git_accounts::resolve(
                state,
                &repository.remote_url,
                &repository.provider,
                &repository.credential_scope,
                owner,
            )?;
        }
        crate::composition::project_layers(state, owner, &repository.layer_selectors, "default")?;
        out.push(d::JobRepository {
            repository_id: repository.id.try_clone()?,
            name: repository.name.try_clone()?,
            remote_url: repository.remote_url.try_clone()?,
            provider: repository.provider.try_clone()?,
            credential_scope: repository.credential_scope.try_clone()?,
            base_ref: try_string(base)?,
            mode,
            ..Default::default()
        })?;
    }
    // Stabiele partitie zonder verborgen allocatie: de eerste schrijfrepo leidt.
    let mut write_index = 0;
    for index in 0..out.len() {
        if out[index].mode == d::REPOSITORY_MODE_CHANGE {
            out.as_mut_slice()[write_index..=index].rotate_right(1);
            write_index += 1;
        }
    }
    if write_index == 0 {
        return Err(Error::Conflict("a Job must change at least one repository"));
    }
    if out.len() > 1 {
        let mut paths = Map::new();
        for r in out.as_mut_slice() {
            let base = slug(&r.name)?;
            let mut candidate = base.try_clone()?;
            let mut suffix = 2_u64;
            while paths.contains_key(&candidate) {
                candidate = text(format_args!("{base}-{suffix}"))?;
                suffix += 1;
            }
            paths.insert(candidate.try_clone()?, true)?;
            r.path = candidate;
        }
    }
    Ok(out)
}
impl<P: Persistence> Store<P> {
    /// Leent een Job voor app-autorisatie zonder een volledige Snapshot te kopiëren.
    pub fn job(&self, id: &str) -> Result<&d::Job> {
        self.state.jobs.get(id).ok_or(Error::NotFound)
    }
    /// Maakt Job, root-Session, templateversie en bijlagen in één duurzame mutatie.
    pub fn create_job(
        &mut self,
        mut req: d::CreateJobRequest,
        mut context: Mutation<'_>,
    ) -> Result<d::CreateJobResponse> {
        let raw_selector =
            if req.environment_selector.trim().is_empty() && !req.tool.trim().is_empty() {
                text(format_args!("tool:{}", normalized(&req.tool)?))?
            } else {
                normalized(&req.environment_selector)?
            };
        let selection = selector(&raw_selector)?;
        if req.title.trim().is_empty()
            || req.objective.trim().is_empty()
            || (req.git_repository_id.trim().is_empty()
                && req.repositories.is_empty()
                && req.forked_from_job_id.trim().is_empty())
            || (req.brainstorm && req.template_id.trim().is_empty())
        {
            return Err(Error::Conflict(
                "title, objective, repository and environment are required; brainstorm needs a template",
            ));
        }
        let requested_with = selectors(&req.with_selectors)?;
        let operator = normalized(if req.operator.trim().is_empty() {
            &req.owner
        } else {
            &req.operator
        })?;
        let mut owner = normalized(if req.owner.trim().is_empty() {
            &operator
        } else {
            &req.owner
        })?;
        if operator.is_empty() {
            return Err(Error::Conflict("operator is required"));
        }
        let source_id = req.forked_from_job_id.trim();
        let mut reference = try_string(req.reference.trim())?;
        if !source_id.is_empty() {
            let source = self.state.jobs.get(source_id).ok_or(Error::NotFound)?;
            if !matches!(source.status.as_str(), d::JOB_DONE | d::JOB_CANCELLED)
                || source.branch.trim().is_empty()
            {
                return Err(Error::Conflict(
                    "only a closed Job with a remote branch can be forked",
                ));
            }
            if req.repositories.is_empty() {
                req.git_repository_id = source.git_repository_id.try_clone()?;
                req.base_ref = source.base_ref.try_clone()?;
                for repo in source.job_repositories()?.iter() {
                    req.repositories.push(d::JobRepositoryRequest {
                        repository_id: repo.repository_id.try_clone()?,
                        mode: repo.mode.try_clone()?,
                        base_ref: repo.base_ref.try_clone()?,
                    })?;
                }
            }
            if reference.is_empty() {
                reference = source.reference.try_clone()?;
            }
            owner = operator.try_clone()?;
        }
        if req.repositories.is_empty() {
            req.repositories.push(d::JobRepositoryRequest {
                repository_id: req.git_repository_id.try_clone()?,
                mode: try_string(d::REPOSITORY_MODE_CHANGE)?,
                base_ref: req.base_ref.try_clone()?,
            })?;
        }
        let repositories = repositories(&self.state, &req.repositories, &owner)?;
        let key = req.idempotency_key.trim();
        if key.len() > 128 {
            return Err(Error::Conflict("idempotency key exceeds 128 bytes"));
        }
        let request_key = if key.is_empty() {
            String::new()
        } else {
            text(format_args!("{operator}\0{key}"))?
        };
        if !request_key.is_empty()
            && let Some(id) = self.state.job_request_keys.get(&request_key)
        {
            let job = self
                .state
                .jobs
                .get(id)
                .ok_or(Error::Conflict("idempotency key references missing Job"))?;
            let session = job
                .session_ids
                .first()
                .and_then(|id| self.state.sessions.get(id))
                .ok_or(Error::Conflict("idempotent Job has no root Session"))?;
            return Ok(d::CreateJobResponse {
                job: job.try_clone()?,
                session: session.try_clone()?,
                replayed: true,
                ..Default::default()
            });
        }
        if !reference.is_empty() && !valid_job_reference(&reference) {
            return Err(Error::Conflict("invalid Job reference"));
        }
        self.edit(|state| {
            let main = repositories
                .first()
                .ok_or(Error::Conflict("missing main repository"))?;
            let repository = state
                .git_repositories
                .get(&main.repository_id)
                .ok_or(Error::NotFound)?;
            crate::composition::project_layers(
                state,
                &operator,
                &repository.layer_selectors,
                "default",
            )?;
            let attachment_ids = unique(req.attachment_ids.iter())?;
            let mut bytes = 0_i64;
            if attachment_ids.len() > 8 {
                return Err(Error::Conflict("Job may contain at most eight attachments"));
            }
            for id in attachment_ids.iter() {
                let a = state
                    .job_attachments
                    .get(id)
                    .filter(|a| a.job_id.is_empty() && a.created_by == operator)
                    .ok_or(Error::NotFound)?;
                bytes = bytes
                    .checked_add(a.size)
                    .ok_or(Error::Conflict("attachment size overflow"))?;
                if a.size < 0 || bytes > 40 << 20 {
                    return Err(Error::Conflict("Job attachments exceed 40 MiB"));
                }
            }
            let mcp_ids = unique(req.mcp_server_ids.iter())?;
            mcp(state, &operator, &mcp_ids)?;
            let template = if req.template_id.trim().is_empty() {
                None
            } else {
                Some(
                    state
                        .workflow_templates
                        .get(req.template_id.trim())
                        .filter(|t| !t.phases.is_empty())
                        .ok_or(Error::NotFound)?
                        .try_clone()?,
                )
            };
            let mut with = List::new();
            if let Some(t) = template.as_ref().filter(|t| !t.git_selector.is_empty()) {
                with.push(t.git_selector.try_clone()?)?;
            }
            for value in repository
                .layer_selectors
                .iter()
                .chain(requested_with.iter())
            {
                if !with.contains(value) {
                    with.push(value.try_clone()?)?;
                }
            }
            crate::composition::session_environment(
                state, &operator, &selection, &with, "default",
            )?;
            if let Some(t) = &template {
                for phase in t
                    .phases
                    .iter()
                    .filter(|p| p.executor != d::WORKFLOW_EXECUTOR_ACTION)
                {
                    let (base, extra) = crate::composition::phase_environment(
                        state, &operator, phase, &selection, &with,
                    )?;
                    crate::composition::session_environment(
                        state, &operator, &base, &extra, "default",
                    )?;
                }
            }
            let id = context.id("job")?;
            if state.jobs.get(&id).is_some() {
                return Err(Error::Conflict("Job id already exists"));
            }
            let name = text(format_args!("{}-{}", slug(&req.title)?, suffix(&id)))?;
            let namespace = if reference.is_empty() {
                text(format_args!("jobs/{name}"))?
            } else {
                let base = text(format_args!("jobs/{reference}"))?;
                let branch = text(format_args!("{base}/main"))?;
                if state.jobs.iter().any(|(_, j)| j.branch == branch) {
                    text(format_args!("{base}/{name}"))?
                } else {
                    base
                }
            };
            let mut job = d::Job {
                id,
                forked_from_job_id: try_string(source_id)?,
                title: try_string(req.title.trim())?,
                reference,
                objective: try_string(req.objective.trim())?,
                acceptance_criteria: req.acceptance_criteria,
                assignee: owner.try_clone()?,
                owner,
                git_repository_id: main.repository_id.try_clone()?,
                git_repository_name: main.name.try_clone()?,
                git_remote_url: main.remote_url.try_clone()?,
                git_provider: main.provider.try_clone()?,
                git_credential_scope: main.credential_scope.try_clone()?,
                base_ref: main.base_ref.try_clone()?,
                branch: text(format_args!("{namespace}/main"))?,
                repositories,
                with_selectors: with,
                mcp_server_ids: mcp_ids,
                attachment_ids,
                template_id: try_string(template.as_ref().map_or("", |t| t.id.as_str()))?,
                template_snapshot: template.try_clone()?,
                environment_selector: selection,
                model: req.model,
                phase_run_ids: List::new(),
                status: try_string(d::JOB_ACTIVE)?,
                session_ids: List::new(),
                candidate_result_ids: List::new(),
                created_at: context.now.try_clone()?,
                updated_at: context.now.try_clone()?,
                ..Default::default()
            };
            let session = if let Some(t) = template {
                let phase = if req.brainstorm {
                    d::brainstorm_phase()?
                } else {
                    t.phases.first().ok_or(Error::NotFound)?.try_clone()?
                };
                let (session, run) =
                    crate::workflow::new_session(state, &mut job, &t, &phase, "", &mut context)?;
                state.phase_runs.insert(run.id.try_clone()?, run)?;
                session
            } else {
                let id = context.id("ses")?;
                if state.sessions.get(&id).is_some() {
                    return Err(Error::Conflict("Session id already exists"));
                }
                let tool = if req.tool.trim().is_empty() {
                    try_string(
                        job.environment_selector
                            .split_once(':')
                            .map_or("", |(_, name)| name),
                    )?
                } else {
                    normalized(&req.tool)?
                };
                let session = d::Session {
                    git_ref: git_ref(&job, &id)?,
                    id,
                    job_id: job.id.try_clone()?,
                    fork_mode: try_string(d::FORK_ROOT)?,
                    tool,
                    executor: try_string(d::WORKFLOW_EXECUTOR_AGENT)?,
                    environment_selector: job.environment_selector.try_clone()?,
                    with_selectors: job.with_selectors.try_clone()?,
                    mcp_server_ids: job.mcp_server_ids.try_clone()?,
                    role: try_string("primary")?,
                    model: job.model.try_clone()?,
                    operator,
                    git_repository_id: job.git_repository_id.try_clone()?,
                    base_ref: job.branch.try_clone()?,
                    target_branch: job.branch.try_clone()?,
                    status: try_string(d::SESSION_QUEUED)?,
                    turn_ids: List::new(),
                    checkpoint_ids: List::new(),
                    continuity_level: try_string("job_root")?,
                    continuity_score: 10,
                    created_at: context.now.try_clone()?,
                    updated_at: context.now.try_clone()?,
                    ..Default::default()
                };
                job.session_ids.push(session.id.try_clone()?)?;
                session
            };
            for id in job.attachment_ids.iter() {
                state
                    .job_attachments
                    .get_mut(id)
                    .ok_or(Error::NotFound)?
                    .job_id = job.id.try_clone()?;
            }
            state
                .sessions
                .insert(session.id.try_clone()?, session.try_clone()?)?;
            state.jobs.insert(job.id.try_clone()?, job.try_clone()?)?;
            if !request_key.is_empty() {
                state
                    .job_request_keys
                    .insert(request_key, job.id.try_clone()?)?;
            }
            Ok(d::CreateJobResponse {
                job,
                session,
                ..Default::default()
            })
        })
    }
    /// Wijzigt de omgeving voor volgende stappen; bestaande sessies houden hun keuze.
    pub fn update_job_environment(
        &mut self,
        id: &str,
        operator: &str,
        req: d::UpdateJobEnvironmentRequest,
        now: &Timestamp,
    ) -> Result<d::Job> {
        let operator = normalized(operator)?;
        let selection = selector(&req.environment_selector)?;
        let mcp_ids = unique(req.mcp_server_ids.iter())?;
        self.edit(|state| {
            let job = state.jobs.get(id.trim()).ok_or(Error::NotFound)?;
            if !job.allows_operator(&operator)
                || matches!(job.status.as_str(), d::JOB_DONE | d::JOB_CANCELLED)
            {
                return Err(Error::Conflict(
                    "only owner or assignee may change an active Job environment",
                ));
            }
            crate::composition::resolve(state, &selection, job.worker(), "default")?;
            mcp(state, job.worker(), &mcp_ids)?;
            let job = state.jobs.get_mut(id.trim()).ok_or(Error::NotFound)?;
            job.environment_selector = selection;
            job.mcp_server_ids = mcp_ids;
            job.updated_at = now.try_clone()?;
            Ok(job.try_clone()?)
        })
    }
    /// Een ingelogde collega kan werk overdragen aan een bekende actieve gebruiker.
    pub fn assign_job(
        &mut self,
        id: &str,
        operator: &str,
        assignee: &str,
        now: &Timestamp,
    ) -> Result<d::Job> {
        let operator = normalized(operator)?;
        let assignee = normalized(assignee)?;
        if operator.is_empty() || assignee.is_empty() {
            return Err(Error::Conflict("operator and assignee are required"));
        }
        self.edit(|state| {
            let job = state.jobs.get(id.trim()).ok_or(Error::NotFound)?;
            let mut known = assignee == job.owner;
            for (_, user) in state.users.iter().filter(|(_, u)| u.archived_at.is_none()) {
                known |= normalized(&user.username)? == assignee;
            }
            if !known {
                return Err(Error::NotFound);
            }
            let job = state.jobs.get_mut(id.trim()).ok_or(Error::NotFound)?;
            job.assignee = assignee;
            job.updated_at = now.try_clone()?;
            Ok(job.try_clone()?)
        })
    }
    /// Sluit werk en alle open workflowbesluiten samen, met behoud van historie.
    pub fn close_job(&mut self, id: &str, operator: &str, now: &Timestamp) -> Result<d::Job> {
        let operator = normalized(operator)?;
        let job = self.state.jobs.get(id.trim()).ok_or(Error::NotFound)?;
        if !job.allows_operator(&operator) {
            return Err(Error::Conflict("Job belongs to another operator"));
        }
        if matches!(job.status.as_str(), d::JOB_DONE | d::JOB_CANCELLED) {
            return Ok(job.try_clone()?);
        }
        self.edit(|state| {
            let job = state.jobs.get_mut(id.trim()).ok_or(Error::NotFound)?;
            let reason = text(format_args!("Job closed by {operator}"))?;
            if let Some(run) = state
                .phase_runs
                .get_mut(&job.current_phase_run_id)
                .filter(|r| {
                    matches!(
                        r.status.as_str(),
                        d::PHASE_RUN_QUEUED | d::PHASE_RUN_RUNNING | d::PHASE_RUN_PENDING
                    )
                })
            {
                run.status = try_string(d::PHASE_RUN_REJECTED)?;
                run.pending_reason.clear();
                run.pending_outcome.clear();
                run.reject_reason = reason.try_clone()?;
                run.completed_at = Some(now.try_clone()?);
                if let Some(session) = state.sessions.get_mut(&run.session_id) {
                    session.status = try_string(d::SESSION_CANCELLED)?;
                    session.updated_at = now.try_clone()?;
                }
            }
            for (_, q) in state
                .workflow_questions
                .iter_mut()
                .filter(|(_, q)| q.job_id == job.id && q.status == "open")
            {
                q.answer = try_string("closed")?;
                q.reason = reason.try_clone()?;
                q.answered_by = operator.try_clone()?;
                q.status = try_string("answered")?;
                q.answered_at = Some(now.try_clone()?);
            }
            job.status = try_string(d::JOB_CANCELLED)?;
            job.workflow_status = try_string(d::WORKFLOW_DONE)?;
            job.pending_reason.clear();
            job.current_phase_run_id.clear();
            job.updated_at = now.try_clone()?;
            Ok(job.try_clone()?)
        })
    }
    /// Levert de composities die de runtime moet stoppen voordat verwijderen kan.
    pub fn prepare_job_deletion(
        &self,
        id: &str,
        operator: &str,
    ) -> Result<(d::Job, List<d::Composition>)> {
        let job = self.state.jobs.get(id.trim()).ok_or(Error::NotFound)?;
        if !job.allows_operator(&normalized(operator)?) {
            return Err(Error::Conflict("Job belongs to another operator"));
        }
        let mut compositions = List::new();
        for (_, c) in self.state.compositions.iter() {
            if self
                .state
                .sessions
                .get(&c.session_id)
                .is_some_and(|s| s.job_id == job.id)
            {
                compositions.push(c.try_clone()?)?;
            }
        }
        crate::snapshot::by_time(&mut compositions, |c| &c.created_at, false)?;
        Ok((job.try_clone()?, compositions))
    }
    /// Verwijdert een gestopte Job-graaf; forks behouden hun bron als context.
    pub fn delete_job(&mut self, id: &str, operator: &str) -> Result<d::Job> {
        self.delete_job_with_blobs(id, operator, &[])
    }
    /// Graph deletion and the cleanup obligation are one commit.
    pub fn delete_job_with_blobs(
        &mut self,
        id: &str,
        operator: &str,
        garbage: &[String],
    ) -> Result<d::Job> {
        let operator = normalized(operator)?;
        self.edit(|state| {
            let job = state
                .jobs
                .get(id.trim())
                .ok_or(Error::NotFound)?
                .try_clone()?;
            if !job.allows_operator(&operator) {
                return Err(Error::Conflict("Job belongs to another operator"));
            }
            if state
                .jobs
                .iter()
                .any(|(_, j)| j.forked_from_job_id == job.id)
            {
                return Err(Error::Conflict("Job is context for a fork"));
            }
            let mut sessions = Map::new();
            for (id, _) in state.sessions.iter().filter(|(_, s)| s.job_id == job.id) {
                sessions.insert(try_string(id)?, true)?;
            }
            for (_, c) in state.compositions.iter() {
                if sessions.contains_key(&c.session_id)
                    && c.runtime.as_ref().is_some_and(|r| r.status != "stopped")
                {
                    return Err(Error::Conflict("Job still has a running composition"));
                }
            }
            state
                .compositions
                .retain(|_, c| !sessions.contains_key(&c.session_id));
            state
                .activations
                .retain(|_, a| !sessions.contains_key(&a.session_id));
            state
                .turns
                .retain(|_, t| !sessions.contains_key(&t.session_id));
            state
                .checkpoints
                .retain(|_, c| !sessions.contains_key(&c.session_id));
            state
                .results
                .retain(|_, r| r.job_id != job.id && !sessions.contains_key(&r.session_id));
            state.phase_runs.retain(|_, r| r.job_id != job.id);
            state.deliverable_comments.retain(|_, c| {
                state
                    .deliverables
                    .get(&c.deliverable_id)
                    .is_none_or(|d| d.job_id != job.id)
            });
            state.deliverables.retain(|_, d| d.job_id != job.id);
            state.code_review_comments.retain(|_, c| {
                state
                    .code_review_revisions
                    .get(&c.revision_id)
                    .is_none_or(|r| r.job_id != job.id)
            });
            state
                .code_review_revisions
                .retain(|_, r| r.job_id != job.id);
            state.workflow_questions.retain(|_, q| q.job_id != job.id);
            for id in job.attachment_ids.iter() {
                state.job_attachments.remove(id);
            }
            state.sessions.retain(|id, _| !sessions.contains_key(id));
            state
                .workflow_tokens
                .retain(|id, _| !sessions.contains_key(id));
            state.job_request_keys.retain(|_, value| value != &job.id);
            for reference in garbage {
                crate::blobs::queue_garbage(state, reference)?;
            }
            state.jobs.remove(&job.id);
            Ok(job)
        })
    }
}

impl<P: Persistence> Store<P> {
    /// Voegt een zelfstandige worker toe aan een open Job, met gevalideerde capabilities.
    pub fn create_job_session(
        &mut self,
        id: &str,
        req: d::CreateJobSessionRequest,
        context: Context<'_>,
    ) -> Result<d::Session> {
        context.validate()?;
        let operator = normalized(&req.operator)?;
        let selection = selector(&req.environment_selector)?;
        if operator.is_empty() || req.objective_delta.trim().is_empty() {
            return Err(Error::Conflict("operator and objective_delta are required"));
        }
        let requested_with = selectors(&req.with_selectors)?;
        self.edit(|state| {
            if state.sessions.get(context.id).is_some() {
                return Err(Error::Conflict("Session id already exists"));
            }
            let mut job = state.jobs.get(id).ok_or(Error::NotFound)?.try_clone()?;
            if matches!(job.status.as_str(), d::JOB_DONE | d::JOB_CANCELLED) {
                return Err(Error::Conflict("closed Job cannot accept sessions"));
            }
            if state.git_repositories.get(&job.git_repository_id).is_none() {
                return Err(Error::NotFound);
            }
            if !req.spawned_by_session_id.is_empty()
                && state
                    .sessions
                    .get(&req.spawned_by_session_id)
                    .is_none_or(|s| s.job_id != job.id)
            {
                return Err(Error::Conflict("spawning Session must belong to Job"));
            }
            let with = unique(job.with_selectors.iter().chain(requested_with.iter()))?;
            crate::composition::session_environment(
                state, &operator, &selection, &with, "default",
            )?;
            let mut mcp_ids = unique(req.mcp_server_ids.iter())?;
            if mcp_ids.is_empty() {
                mcp_ids = job.mcp_server_ids.try_clone()?;
            }
            mcp(state, &operator, &mcp_ids)?;
            ensure_branch(&mut job)?;
            let mut role = normalized(&req.role)?;
            if role.is_empty() {
                role = try_string("worker")?;
            }
            let session = d::Session {
                id: try_string(context.id)?,
                job_id: job.id.try_clone()?,
                parent_session_id: req.spawned_by_session_id.try_clone()?,
                spawned_by_session_id: req.spawned_by_session_id,
                fork_mode: try_string(d::FORK_ROOT)?,
                tool: try_string(selection.split_once(':').map_or("", |(_, name)| name))?,
                executor: try_string(d::WORKFLOW_EXECUTOR_AGENT)?,
                environment_selector: selection,
                with_selectors: with,
                mcp_server_ids: mcp_ids,
                role,
                model: req.model,
                operator,
                objective_delta: try_string(req.objective_delta.trim())?,
                git_repository_id: job.git_repository_id.try_clone()?,
                base_ref: job.branch.try_clone()?,
                git_ref: git_ref(&job, context.id)?,
                target_branch: job.branch.try_clone()?,
                status: try_string(d::SESSION_QUEUED)?,
                turn_ids: List::new(),
                checkpoint_ids: List::new(),
                continuity_level: try_string("job_session")?,
                continuity_score: 10,
                created_at: context.now.try_clone()?,
                updated_at: context.now.try_clone()?,
                ..Default::default()
            };
            job.session_ids.push(session.id.try_clone()?)?;
            job.status = try_string(d::JOB_ACTIVE)?;
            job.updated_at = context.now.try_clone()?;
            state
                .sessions
                .insert(session.id.try_clone()?, session.try_clone()?)?;
            state.jobs.insert(job.id.try_clone()?, job)?;
            Ok(session)
        })
    }
    /// Forkt alleen checkpoints en resultaten van de bron-Session en haar Job.
    pub fn fork_session(
        &mut self,
        parent: &str,
        mut req: d::ForkSessionRequest,
        context: Context<'_>,
    ) -> Result<d::Session> {
        context.validate()?;
        self.edit(|state| {
            if state.sessions.get(context.id).is_some() {
                return Err(Error::Conflict("Session id already exists"));
            }
            let parent = state.sessions.get(parent).ok_or(Error::NotFound)?;
            if req.fork_mode.is_empty() {
                req.fork_mode = try_string(d::FORK_FULL)?;
            }
            if req.checkpoint_id.is_empty() {
                req.checkpoint_id = parent.current_checkpoint_id.try_clone()?;
            }
            let checkpoint = if req.fork_mode == d::FORK_ROOT {
                None
            } else {
                Some(
                    state
                        .checkpoints
                        .get(&req.checkpoint_id)
                        .filter(|c| c.session_id == parent.id)
                        .ok_or(Error::NotFound)?,
                )
            };
            for result in req.input_result_ids.iter() {
                if state
                    .results
                    .get(result)
                    .is_none_or(|r| r.job_id != parent.job_id)
                {
                    return Err(Error::NotFound);
                }
            }
            let (tool, environment) = if req.tool.trim().is_empty() {
                (
                    parent.tool.try_clone()?,
                    parent.environment_selector.try_clone()?,
                )
            } else {
                let tool = normalized(&req.tool)?;
                let environment = text(format_args!("tool:{tool}"))?;
                (tool, environment)
            };
            if req.fork_mode == d::FORK_FULL
                && (checkpoint.is_none_or(|c| !c.capsule.restorable) || tool != parent.tool)
            {
                return Err(Error::Conflict(
                    "full fork requires restorable checkpoint and same tool",
                ));
            }
            let operator = if req.operator.trim().is_empty() {
                parent.operator.try_clone()?
            } else {
                normalized(&req.operator)?
            };
            crate::composition::session_environment(
                state,
                &operator,
                &environment,
                &parent.with_selectors,
                "default",
            )?;
            let mut job = state
                .jobs
                .get(&parent.job_id)
                .ok_or(Error::NotFound)?
                .try_clone()?;
            ensure_branch(&mut job)?;
            let (level, score) = match req.fork_mode.as_str() {
                d::FORK_FULL => ("full_checkpoint", if tool == parent.tool { 95 } else { 30 }),
                d::FORK_FILESYSTEM => ("filesystem", 30),
                d::FORK_RESULT | d::FORK_CRITIC | d::FORK_SYNTHESIS => ("result_handoff", 20),
                _ => ("job_root", 10),
            };
            let session = d::Session {
                id: try_string(context.id)?,
                job_id: job.id.try_clone()?,
                parent_session_id: parent.id.try_clone()?,
                spawned_by_session_id: parent.id.try_clone()?,
                parent_checkpoint_id: req.checkpoint_id,
                input_result_ids: req.input_result_ids,
                fork_mode: req.fork_mode,
                tool,
                executor: try_string(d::WORKFLOW_EXECUTOR_AGENT)?,
                environment_selector: environment,
                with_selectors: parent.with_selectors.try_clone()?,
                mcp_server_ids: parent.mcp_server_ids.try_clone()?,
                role: parent.role.try_clone()?,
                model: if req.model.is_empty() {
                    parent.model.try_clone()?
                } else {
                    req.model
                },
                operator,
                objective_delta: req.objective_delta,
                git_repository_id: parent.git_repository_id.try_clone()?,
                base_ref: job.branch.try_clone()?,
                git_ref: git_ref(&job, context.id)?,
                target_branch: job.branch.try_clone()?,
                status: try_string(d::SESSION_QUEUED)?,
                turn_ids: List::new(),
                checkpoint_ids: List::new(),
                continuity_level: try_string(level)?,
                continuity_score: score,
                created_at: context.now.try_clone()?,
                updated_at: context.now.try_clone()?,
                ..Default::default()
            };
            job.session_ids.push(session.id.try_clone()?)?;
            job.status = try_string(d::JOB_ACTIVE)?;
            job.updated_at = context.now.try_clone()?;
            state
                .sessions
                .insert(session.id.try_clone()?, session.try_clone()?)?;
            state.jobs.insert(job.id.try_clone()?, job)?;
            Ok(session)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IdSource, tests::Memory};
    use core::cell::Cell;
    use d::Wire;
    struct Ids(u64);
    impl IdSource for Ids {
        fn next(&mut self, prefix: &str) -> Result<String> {
            self.0 += 1;
            Ok(text(format_args!("{prefix}_{:06}", self.0))?)
        }
    }
    fn fixture(fail: &Cell<bool>) -> (Store<Memory<'_>>, Timestamp) {
        let state = PersistedState::from_json(br#"{
            "artifacts":{"env":{"id":"env","kind":"tool","name":"agent","scope":"global","profile":"default","enables":[{"name":"acp"},{"name":"git"}]}},
            "git_repositories":{"repo":{"id":"repo","name":"Spin","remote_url":"https://example.test/spin.git","default_ref":"main","credential_scope":"public"},"ref":{"id":"ref","name":"Spin","remote_url":"https://example.test/ref.git","default_ref":"main","credential_scope":"public"}},
            "job_attachments":{"att":{"id":"att","name":"brief.txt","size":8,"created_by":"derek"}},
            "users":{"user":{"id":"user","username":"colleague"}},
            "workflow_templates":{"tpl":{"id":"tpl","revision":1,"name":"Process","phases":[{"id":"build","name":"Build","executor":"agent","accept":{"target":"NEXT","ask_user":true},"reject":{"target":"SELF"}},{"id":"review","name":"Review","executor":"agent","accept":{"target":"DONE"}}]}}
        }"#).unwrap();
        (
            Store::new(state, Memory(fail)),
            Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap(),
        )
    }
    fn request() -> d::CreateJobRequest {
        d::CreateJobRequest::from_json(br##"{"operator":"Derek","title":"A useful change","objective":"Ship it","reference":"#42","git_repository_id":"repo","environment_selector":"tool:agent","attachment_ids":["att"],"idempotency_key":"click-once"}"##).unwrap()
    }
    #[test]
    fn create_replay_close_fork_and_delete_preserve_job_boundaries() {
        let fail = Cell::new(true);
        let (mut store, now) = fixture(&fail);
        let mut ids = Ids(0);
        assert_eq!(
            store
                .create_job(
                    request(),
                    Mutation {
                        now: &now,
                        ids: &mut ids
                    }
                )
                .unwrap_err(),
            Error::Storage(10)
        );
        assert!(store.state.jobs.is_empty());
        assert!(store.state.sessions.is_empty());
        assert!(
            store
                .state
                .job_attachments
                .get("att")
                .unwrap()
                .job_id
                .is_empty()
        );
        fail.set(false);
        let created = store
            .create_job(
                request(),
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        assert_eq!(created.job.branch, "jobs/#42/main");
        assert_eq!(created.session.base_ref, created.job.branch);
        assert_eq!(
            store.state.job_attachments.get("att").unwrap().job_id,
            created.job.id
        );
        let version = store.version();
        fail.set(true);
        let replay = store
            .create_job(
                request(),
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.job.id, created.job.id);
        assert_eq!(store.version(), version);
        fail.set(false);
        assert!(store.delete_job(&created.job.id, "").is_err());
        let c =
            d::Composition::from_json(br#"{"id":"cmp","runtime":{"status":"running"}}"#).unwrap();
        let mut c = c;
        c.session_id = created.session.id.try_clone().unwrap();
        store
            .state
            .compositions
            .insert(c.id.try_clone().unwrap(), c)
            .unwrap();
        assert!(store.delete_job(&created.job.id, "derek").is_err());
        assert_eq!(
            store
                .prepare_job_deletion(&created.job.id, "derek")
                .unwrap()
                .1
                .len(),
            1
        );
        let closed = store.close_job(&created.job.id, "derek", &now).unwrap();
        assert_eq!(closed.status, d::JOB_CANCELLED);
        let mut fork = request();
        fork.forked_from_job_id = created.job.id.try_clone().unwrap();
        fork.attachment_ids = List::new();
        fork.idempotency_key.clear();
        let fork = store
            .create_job(
                fork,
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        assert_eq!(fork.job.base_ref, "main");
        assert!(fork.job.branch.starts_with("jobs/#42/a-useful-change-"));
        assert!(store.delete_job(&created.job.id, "derek").is_err());
        store.delete_job(&fork.job.id, "derek").unwrap();
        store
            .state
            .compositions
            .get_mut("cmp")
            .unwrap()
            .runtime
            .as_mut()
            .unwrap()
            .status = try_string("stopped").unwrap();
        store.delete_job(&created.job.id, "derek").unwrap();
        assert!(store.state.jobs.is_empty());
        assert!(store.state.sessions.is_empty());
        assert!(store.state.compositions.is_empty());
        assert!(store.state.job_request_keys.is_empty());
        assert!(store.state.job_attachments.is_empty());
    }
    #[test]
    fn repository_order_paths_and_session_fork_require_actual_checkpoint() {
        let fail = Cell::new(false);
        let (mut store, now) = fixture(&fail);
        let mut ids = Ids(0);
        let mut req = request();
        req.repositories = List::from_json(br#"[{"repository_id":"ref","mode":"reference"},{"repository_id":"repo","mode":"change"}]"#).unwrap();
        let created = store
            .create_job(
                req,
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        assert_eq!(created.job.repositories[0].repository_id, "repo");
        assert_eq!(created.job.repositories[0].path, "spin");
        assert_eq!(created.job.repositories[1].path, "spin-2");
        assert!(
            store
                .fork_session(
                    &created.session.id,
                    d::ForkSessionRequest::default(),
                    Context {
                        now: &now,
                        id: "ses_fork"
                    }
                )
                .is_err()
        );
        let mut cp =
            d::Checkpoint::from_json(br#"{"id":"chk","capsule":{"restorable":true}}"#).unwrap();
        cp.session_id = created.session.id.try_clone().unwrap();
        store
            .state
            .checkpoints
            .insert(try_string("chk").unwrap(), cp)
            .unwrap();
        store
            .state
            .sessions
            .get_mut(&created.session.id)
            .unwrap()
            .current_checkpoint_id = try_string("chk").unwrap();
        let fork = store
            .fork_session(
                &created.session.id,
                d::ForkSessionRequest::default(),
                Context {
                    now: &now,
                    id: "ses_fork",
                },
            )
            .unwrap();
        assert_eq!(fork.continuity_score, 95);
        assert_eq!(fork.parent_checkpoint_id, "chk");
        assert_eq!(fork.base_ref, created.job.branch);
        let req = d::CreateJobSessionRequest::from_json(br#"{"operator":"derek","environment_selector":"tool:agent","objective_delta":"review separately"}"#).unwrap();
        let extra = store
            .create_job_session(
                &created.job.id,
                req,
                Context {
                    now: &now,
                    id: "ses_extra",
                },
            )
            .unwrap();
        assert_eq!(extra.role, "worker");
        assert_eq!(extra.job_id, created.job.id);
    }
    #[test]
    fn brainstorm_assignment_frozen_template_and_adoption_form_one_lifecycle() {
        let fail = Cell::new(false);
        let (mut store, now) = fixture(&fail);
        let mut ids = Ids(0);
        let mut req = request();
        req.template_id = try_string("tpl").unwrap();
        req.brainstorm = true;
        let created = store
            .create_job(
                req,
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        assert_eq!(
            store
                .workflow_for_session(&created.session.id)
                .unwrap()
                .phase
                .id,
            d::BRAINSTORM_PHASE_ID
        );
        store
            .mark_workflow_phase_running(&created.session.id, &now)
            .unwrap();
        let (process, _) = store
            .start_process(
                &created.session.id,
                "Goal from discussion",
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        assert_eq!(process.job.objective, "Goal from discussion");
        assert_eq!(
            store
                .workflow_for_session(&process.session.id)
                .unwrap()
                .phase
                .id,
            "build"
        );
        store
            .assign_job(&process.job.id, "derek", "colleague", &now)
            .unwrap();
        store
            .mark_workflow_phase_running(&process.session.id, &now)
            .unwrap();
        let q = store
            .complete_workflow_phase(
                &process.session.id,
                "accept",
                "ready",
                false,
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap()
            .question
            .unwrap();
        let template = store.state.workflow_templates.get_mut("tpl").unwrap();
        template.revision = 2;
        template.phases.as_mut_slice()[1].name = try_string("New Review").unwrap();
        assert_eq!(
            store
                .workflow_for_session(&process.session.id)
                .unwrap()
                .template
                .revision,
            1
        );
        let advance = store
            .answer_workflow_question(
                &q.id,
                "colleague",
                "accept",
                "",
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        let review = advance.next_session.unwrap();
        assert_eq!(review.operator, "colleague");
        assert_eq!(review.role, "Review");
        let (adopted, _) = store
            .adopt_workflow_template(
                &advance.job.id,
                "colleague",
                "review",
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        assert_eq!(adopted.session.role, "New Review");
        assert_eq!(adopted.job.template_snapshot.unwrap().revision, 2);
        let run = store.workflow_for_session(&adopted.session.id).unwrap().run;
        assert_eq!(run.attempt, 2);
        store
            .mark_workflow_phase_running(&adopted.session.id, &now)
            .unwrap();
        let items =
            List::<d::WorkflowQuestionItem>::from_json(br#"[{"question":"Wait?"}]"#).unwrap();
        let q = store
            .ask_workflow_questions(
                &adopted.session.id,
                &items,
                Context {
                    now: &now,
                    id: "ask_close",
                },
            )
            .unwrap();
        store.close_job(&advance.job.id, "colleague", &now).unwrap();
        assert_eq!(
            store.state.workflow_questions.get(&q.id).unwrap().answer,
            "closed"
        );
        assert_eq!(
            store
                .state
                .sessions
                .get(&adopted.session.id)
                .unwrap()
                .status,
            d::SESSION_CANCELLED
        );
        assert!(
            store
                .answer_workflow_questions(&q.id, "colleague", &[], &now)
                .is_err()
        );
    }
}
