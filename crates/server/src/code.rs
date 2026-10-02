//! Explore leest op aanvraag via een runner; tokens blijven buiten de browser.
use super::*;
use crate::capsules::Action;
use d::{List, engine::*, protocol as p, try_string};
pub(crate) struct Browse {
    pub(crate) repository: String,
    mode: String,
    reference: String,
    default_ref: String,
}
pub(crate) struct Inspection {
    pub(crate) composition: String,
    capsule: d::CapsuleRuntime,
    workspaces: List<d::GitWorkspace>,
    at: usize,
    bytes: usize,
    total: WorkspaceChanges,
    remote: List<RepositoryComparison>,
    review: Option<d::CodeReviewRevision>,
    comment: Option<(String, d::CreateCodeReviewCommentRequest)>,
}
impl<P: Persistence> Server<P> {
    pub(crate) fn job_workspaces(&self, job: &d::Job) -> Result<List<d::GitWorkspace>> {
        let snapshot = self.store.snapshot()?;
        let mut workspaces = List::new();
        for repository in job.changed_repositories()?.iter() {
            if workspaces.len() >= 32 {
                return Err(Error::Http(413, "too many job repositories"));
            }
            let mut workspace = d::GitWorkspace {
                repository_id: repository.repository_id.try_clone()?,
                repository_name: repository.name.try_clone()?,
                remote_url: repository.remote_url.try_clone()?,
                provider: repository.provider.try_clone()?,
                credential_scope: repository.credential_scope.try_clone()?,
                path: repository.path.try_clone()?,
                bootstrap_ref: repository.base_ref.try_clone()?,
                base_ref: job.branch.try_clone()?,
                target_ref: job.branch.try_clone()?,
                ..Default::default()
            };
            if let Some(known) = snapshot
                .git_repositories
                .iter()
                .find(|r| r.id == repository.repository_id)
            {
                if workspace.remote_url.is_empty() {
                    workspace.remote_url = known.remote_url.try_clone()?;
                }
                if workspace.provider.is_empty() {
                    workspace.provider = known.provider.try_clone()?;
                }
                if workspace.credential_scope.is_empty() {
                    workspace.credential_scope = known.credential_scope.try_clone()?;
                }
                if workspace.bootstrap_ref.is_empty() {
                    workspace.bootstrap_ref = known.default_ref.try_clone()?;
                }
            }
            if workspace.repository_name.is_empty() {
                workspace.repository_name = workspace.repository_id.try_clone()?;
            }
            workspaces.push(workspace)?;
        }
        Ok(workspaces)
    }

    fn inspect_session(
        &mut self,
        id: &str,
        review: Option<d::CodeReviewRevision>,
        comment: Option<(String, d::CreateCodeReviewCommentRequest)>,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Outcome> {
        let session = self.store.session(id)?;
        let composition = self.store.composition(&session.prepared_composition_id)?;
        let capsule = composition
            .runtime
            .as_ref()
            .filter(|r| r.status == "ready" && !r.stop_pending)
            .ok_or(Error::Http(409, "session has no running workspace"))?
            .try_clone()?;
        let mut workspaces = List::new();
        for workspace in composition.changed_workspaces() {
            if workspaces.len() >= 32 {
                return Err(Error::Http(413, "too many workspace repositories"));
            }
            workspaces.push(workspace.try_clone()?)?;
        }
        if workspaces.is_empty() {
            workspaces.push(d::GitWorkspace::default())?;
        }
        let work = Inspection {
            remote: List::new(),
            review,
            comment,
            composition: composition.id.try_clone()?,
            capsule: capsule.try_clone()?,
            workspaces,
            at: 0,
            bytes: 0,
            total: WorkspaceChanges {
                files: List::new(),
                ..Default::default()
            },
        };
        let payload = p::WorkspacePathPayload {
            runtime: capsule.try_clone()?,
            path: work.workspaces.as_slice()[0].path.try_clone()?,
        };
        let method = if payload.path.is_empty() {
            p::METHOD_INSPECT_WORKSPACE
        } else {
            p::METHOD_INSPECT_WORKSPACE_AT
        };
        Ok(Outcome::Capsule(self.enqueue_call(
            Action::Inspect(work),
            &capsule.client_id,
            method,
            &payload,
            now,
            runtime,
        )?))
    }
    pub(crate) fn advance_inspection(
        &mut self,
        index: usize,
        client: &str,
        payload: &Value,
        random: &mut impl Runtime,
        now: &Timestamp,
    ) -> Result<Option<Response>> {
        let changes = WorkspaceChanges::from_value(payload)?;
        let Action::Inspect(work) = &mut self.calls[index].action else {
            return Err(Error::Http(500, "missing inspection"));
        };
        let workspace = &work.workspaces.as_slice()[work.at];
        if work.total.branch.is_empty() {
            work.total.branch = changes.branch;
        }
        if work.total.head.is_empty() {
            work.total.head = changes.head.try_clone()?;
        }
        work.total.added = work.total.added.saturating_add(changes.added);
        work.total.deleted = work.total.deleted.saturating_add(changes.deleted);
        for mut file in changes.files.into_vec() {
            work.bytes = work
                .bytes
                .saturating_add(file.patch.len())
                .saturating_add(file.path.len());
            if work.bytes > 8 << 20 || work.total.files.len() >= 8000 {
                return Err(Error::Http(413, "workspace changes exceed response budget"));
            }
            if !workspace.path.is_empty() {
                file.path = spin_core::validation::text(format_args!(
                    "{}/{}",
                    workspace.path,
                    file.path.trim_start_matches("./")
                ))?;
            }
            file.repository = workspace.repository_id.try_clone()?;
            file.folder = workspace.path.try_clone()?;
            if file.head.is_empty() {
                file.head = changes.head.try_clone()?;
            }
            work.total.files.push(file)?;
        }
        work.at += 1;
        if let Some(next) = work.remote.as_slice().get(work.at) {
            let payload = p::RepositoryComparePayload {
                comparison: next.try_clone()?,
            };
            self.continue_capsule(
                index,
                client,
                p::METHOD_COMPARE_REPOSITORY,
                &payload,
                random,
            )?;
            Ok(None)
        } else if work.remote.is_empty()
            && let Some(next) = work.workspaces.as_slice().get(work.at)
        {
            let payload = p::WorkspacePathPayload {
                runtime: work.capsule.try_clone()?,
                path: next.path.try_clone()?,
            };
            self.continue_capsule(
                index,
                client,
                p::METHOD_INSPECT_WORKSPACE_AT,
                &payload,
                random,
            )?;
            Ok(None)
        } else if let Some(mut revision) = work.review.take() {
            revision.branch = work.total.branch.try_clone()?;
            revision.added = work.total.added;
            revision.deleted = work.total.deleted;
            revision.files = List::new();
            for file in work.total.files.iter() {
                revision
                    .files
                    .push(d::CodeReviewFile::from_value(&file.to_value()?)?)?;
            }
            let digest = http::object(&[
                ("branch", revision.branch.to_value()?),
                ("added", revision.added.to_value()?),
                ("deleted", revision.deleted.to_value()?),
                ("files", revision.files.to_value()?),
            ])?
            .to_json()?;
            for byte in spin_security::sha256(digest.as_bytes()) {
                d::try_push_str(
                    &mut revision.digest,
                    &spin_core::validation::text(format_args!("{byte:02x}"))?,
                )?;
            }
            let id = random.next("rev")?;
            let revision = self
                .store
                .save_code_review_revision(revision, spin_store::Context { now, id: &id })?;
            if let Some((expected, request)) = &work.comment {
                if revision.id != *expected {
                    return Err(Error::Http(
                        409,
                        "changes moved while this review was open; reopen the latest revision",
                    ));
                }
                let id = random.next("com")?;
                let comment = self.store.add_code_review_comment(
                    expected,
                    &request.operator,
                    request.try_clone()?,
                    spin_store::Context { now, id: &id },
                )?;
                Ok(Some(Response::json(201, &comment)?))
            } else {
                Ok(Some(Response::json(
                    201,
                    &self.store.code_review_bundle(&revision.id)?,
                )?))
            }
        } else {
            Ok(Some(Response::json(200, &work.total)?))
        }
    }
    fn review_metadata(
        &self,
        job_id: &str,
        session_id: &str,
        live: bool,
        actor: &str,
    ) -> Result<d::CodeReviewRevision> {
        let job = self.store.job(job_id)?;
        let mut review = d::CodeReviewRevision {
            job_id: job.id.try_clone()?,
            context_phase_run_id: job.current_phase_run_id.try_clone()?,
            scope: try_string("job")?,
            scope_key: spin_core::validation::text(format_args!("job:{}", job.id))?,
            created_by: try_string(actor)?,
            ..Default::default()
        };
        if !session_id.is_empty() {
            let view = self.store.workflow_for_session(session_id)?;
            if view.job.id != job_id {
                return Err(Error::Http(404, "session does not belong to Job"));
            }
            review.session_id = try_string(session_id)?;
            review.source_phase_run_id = view.run.id;
            review.phase_id = view.run.phase_id;
            review.phase_name = view.run.phase_name;
            review.attempt = view.run.attempt;
            review.scope = try_string("phase")?;
            review.scope_key = spin_core::validation::text(format_args!(
                "job:{job_id}:phase:{}",
                review.phase_id
            ))?;
            review.live = live;
        }
        Ok(review)
    }
    fn inspect_job(
        &mut self,
        job_id: &str,
        session_id: &str,
        review: Option<d::CodeReviewRevision>,
        comment: Option<(String, d::CreateCodeReviewCommentRequest)>,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Outcome> {
        let job = self.store.job(job_id)?.try_clone()?;
        let workspaces = self.job_workspaces(&job)?;
        if workspaces.is_empty() {
            return Err(Error::Http(409, "this Job changes no repository"));
        }
        let mut remote = List::new();
        let snapshot = self.store.snapshot()?;
        let mut phase = String::new();
        if !session_id.is_empty() {
            let view = self.store.workflow_for_session(session_id)?;
            if view.job.id != job_id {
                return Err(Error::Http(404, "session does not belong to Job"));
            }
            phase = view.run.phase_name;
        }
        for workspace in workspaces.iter() {
            let authentication = self.workspace_authentication(workspace, job.worker())?;
            let mut merge = String::new();
            if session_id.is_empty() {
                for run in snapshot.phase_runs.iter().filter(|r| r.job_id == job_id) {
                    if let Some(result) = &run.action_result
                        && result.r#type == d::WORKFLOW_ACTION_GIT_MERGE
                    {
                        if let Some(head) = result.results.get(&workspace.repository_id) {
                            merge = head.try_clone()?;
                        } else if workspaces
                            .first()
                            .is_some_and(|w| w.repository_id == workspace.repository_id)
                            && !result.external_id.is_empty()
                        {
                            merge = result.external_id.try_clone()?;
                        }
                    }
                }
            }
            remote.push(RepositoryComparison {
                remote_url: workspace.remote_url.try_clone()?,
                cache_key: workspace.repository_id.try_clone()?,
                comparison: WorkspaceComparison {
                    path: workspace.path.try_clone()?,
                    base_ref: if workspace.bootstrap_ref.is_empty() {
                        job.base_ref.try_clone()?
                    } else {
                        workspace.bootstrap_ref.try_clone()?
                    },
                    head_ref: job.branch.try_clone()?,
                    authentication,
                    merge_commit: merge,
                    commit_message_match: if session_id.is_empty() {
                        String::new()
                    } else {
                        spin_core::validation::text(format_args!("Spin-Session: {session_id}"))?
                    },
                },
            })?;
        }
        let branch = if !phase.is_empty() {
            spin_core::validation::text(format_args!("{phase} · {session_id}"))?
        } else if remote.iter().any(|r| !r.comparison.merge_commit.is_empty()) {
            spin_core::validation::text(format_args!(
                "{} ← {} · gemerged",
                job.base_ref, job.branch
            ))?
        } else {
            spin_core::validation::text(format_args!("{} ← {}", job.branch, job.base_ref))?
        };
        let payload = p::RepositoryComparePayload {
            comparison: remote[0].try_clone()?,
        };
        let client = self.choose_runner(now)?;
        let work = Inspection {
            composition: spin_core::validation::text(format_args!("review:{job_id}:{session_id}"))?,
            capsule: Default::default(),
            workspaces,
            at: 0,
            bytes: 0,
            total: WorkspaceChanges {
                branch,
                files: List::new(),
                ..Default::default()
            },
            remote,
            review,
            comment,
        };
        Ok(Outcome::Capsule(self.enqueue_call(
            Action::Inspect(work),
            &client,
            p::METHOD_COMPARE_REPOSITORY,
            &payload,
            now,
            random,
        )?))
    }
    pub(crate) fn code_route(
        &mut self,
        req: &Request<'_>,
        actor: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        if req.method == "POST" {
            if let Some(job) = req
                .path
                .strip_prefix("/api/jobs/")
                .and_then(|s| s.strip_suffix("/code-reviews"))
            {
                let value = d::CreateCodeReviewRequest::from_json(req.body)?;
                let review = self.review_metadata(job, &value.session_id, value.live, actor)?;
                return Ok(Some(if review.live {
                    self.inspect_session(&value.session_id, Some(review), None, now, runtime)?
                } else {
                    self.inspect_job(job, &value.session_id, Some(review), None, now, runtime)?
                }));
            }
            if let Some(id) = req
                .path
                .strip_prefix("/api/code-reviews/")
                .and_then(|s| s.strip_suffix("/comments"))
            {
                let mut value = d::CreateCodeReviewCommentRequest::from_json(req.body)?;
                value.operator = try_string(actor)?;
                let old = self.store.code_review_revision(id)?.try_clone()?;
                let review = self.review_metadata(&old.job_id, &old.session_id, old.live, actor)?;
                let comment = Some((try_string(id)?, value));
                return Ok(Some(if review.live {
                    self.inspect_session(&old.session_id, Some(review), comment, now, runtime)?
                } else {
                    self.inspect_job(
                        &old.job_id,
                        &old.session_id,
                        Some(review),
                        comment,
                        now,
                        runtime,
                    )?
                }));
            }
        }
        if req.method != "GET" {
            return Ok(None);
        }
        if let Some(job) = req
            .path
            .strip_prefix("/api/jobs/")
            .and_then(|s| s.strip_suffix("/changes"))
        {
            return Ok(Some(self.inspect_job(
                job,
                &req.query("session_id")?,
                None,
                None,
                now,
                runtime,
            )?));
        }
        if let Some(id) = req
            .path
            .strip_prefix("/api/sessions/")
            .and_then(|p| p.strip_suffix("/changes"))
            .filter(|id| !id.contains('/'))
        {
            return Ok(Some(self.inspect_session(id, None, None, now, runtime)?));
        }
        let Some(rest) = req.path.strip_prefix("/api/git/repositories/") else {
            return Ok(None);
        };
        let Some((id, mode)) = rest.split_once("/code/") else {
            return Ok(None);
        };
        if id.contains('/') || !matches!(mode, "refs" | "tree" | "file") {
            return Err(Error::Http(404, "not found"));
        }
        let snapshot = self.store.snapshot()?;
        let repository = snapshot
            .git_repositories
            .iter()
            .find(|r| r.id == id)
            .ok_or(Error::Http(404, "repository not found"))?;
        let workspace = d::GitWorkspace {
            repository_id: repository.id.try_clone()?,
            repository_name: repository.name.try_clone()?,
            remote_url: repository.remote_url.try_clone()?,
            provider: repository.provider.try_clone()?,
            credential_scope: repository.credential_scope.try_clone()?,
            ..Default::default()
        };
        let authentication = self.workspace_authentication(&workspace, actor)?;
        let reference = req.query("ref")?;
        let reference = if reference.trim().is_empty() {
            repository.default_ref.try_clone()?
        } else {
            try_string(reference.trim())?
        };
        let browse = RepositoryBrowse {
            remote_url: repository.remote_url.try_clone()?,
            cache_key: repository.id.try_clone()?,
            mode: try_string(mode)?,
            r#ref: reference.try_clone()?,
            path: try_string(req.query("path")?.trim())?,
            authentication,
        };
        let work = Browse {
            repository: repository.id.try_clone()?,
            default_ref: repository.default_ref.try_clone()?,
            mode: try_string(mode)?,
            reference,
        };
        let client = self.choose_runner(now)?;
        let wait = self.enqueue_call(
            Action::Browse(work),
            &client,
            p::METHOD_BROWSE_REPOSITORY,
            &p::RepositoryBrowsePayload { browse },
            now,
            runtime,
        )?;
        Ok(Some(Outcome::Capsule(wait)))
    }
}
pub(crate) fn browse_response(work: &Browse, payload: &Value) -> Result<Response> {
    let result = RepositoryBrowseResult::from_value(payload)?;
    match work.mode.as_str() {
        "refs" => Response::json(
            200,
            &http::object(&[
                (
                    "refs",
                    if result.refs.is_empty() {
                        List::<RepositoryRef>::new().to_value()?
                    } else {
                        result.refs.to_value()?
                    },
                ),
                ("default_ref", Value::string(&work.default_ref)?),
            ])?,
        ),
        "tree" => Response::json(
            200,
            &result.tree.unwrap_or(WorkspaceTree {
                r#ref: work.reference.try_clone()?,
                entries: List::new(),
            }),
        ),
        _ => Response::json(200, &result.file),
    }
}
