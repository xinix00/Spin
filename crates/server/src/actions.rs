//! Workflowacties bewaren hun fase-eigendom tot het externe resultaat bevestigd is.
use super::*;
use crate::capsules::Action;
use d::{List, protocol as p, try_string};
use spin_core::validation::text;
use spin_store::Mutation;

pub(crate) struct Merge {
    pub(crate) session: String,
    workspaces: List<d::GitWorkspace>,
    requests: List<d::engine::RepositoryMerge>,
    at: usize,
    result: d::WorkflowActionResult,
}
fn single_line(value: &str) -> Result<String> {
    let mut out = String::new();
    out.try_reserve_exact(value.len())
        .map_err(|_| d::Error::OutOfMemory)?;
    for c in value.chars() {
        out.push(if matches!(c, '\n' | '\r') { ' ' } else { c });
    }
    Ok(out)
}
fn clamp(value: &str, limit: usize) -> Result<String> {
    if let Some((offset, _)) = value.char_indices().nth(limit.saturating_sub(1)) {
        Ok(text(format_args!("{}…", value[..offset].trim_end()))?)
    } else {
        Ok(try_string(value)?)
    }
}
impl<P: Persistence> Server<P> {
    pub(crate) fn launch_workflow_action(
        &mut self,
        session: &d::Session,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        let view = self.store.workflow_for_session(&session.id)?;
        if !matches!(
            view.run.status.as_str(),
            d::PHASE_RUN_QUEUED | d::PHASE_RUN_RUNNING
        ) || view.job.current_phase_run_id != view.run.id
        {
            return Ok(());
        }
        if self.calls.iter().any(|c| {
            !c.finished && matches!(&c.action, Action::Merge(w) if w.session == session.id)
        }) {
            return Ok(());
        }
        let kind = view.phase.action.as_ref().map_or("", |a| a.r#type.as_str());
        if kind == d::WORKFLOW_ACTION_GIT_PULL_REQUEST {
            return self.launch_pull(session, now, random);
        }
        if kind != d::WORKFLOW_ACTION_GIT_MERGE {
            return Err(Error::Http(
                409,
                "workflow action requires provider transport",
            ));
        }
        let workspaces = self.job_workspaces(&view.job)?;
        if workspaces.is_empty() {
            return Err(Error::Http(409, "Job has no repository to merge"));
        }
        let subject = clamp(
            &single_line(&text(format_args!(
                "Merge {}: {}",
                view.job.branch, view.job.title
            ))?)?,
            200,
        )?;
        let body = clamp(
            &text(format_args!(
                "{}\n\nSpin-Job: {}\nSpin-Merged-By: spin",
                view.job.objective.trim(),
                view.job.id
            ))?,
            4000,
        )?;
        let mut requests = List::new();
        for workspace in workspaces.iter() {
            let target = if workspace.bootstrap_ref.is_empty() {
                &view.job.base_ref
            } else {
                &workspace.bootstrap_ref
            };
            if target.is_empty() {
                return Err(Error::Http(409, "Job has no merge target branch"));
            }
            requests.push(d::engine::RepositoryMerge {
                remote_url: workspace.remote_url.try_clone()?,
                cache_key: workspace.repository_id.try_clone()?,
                source_ref: view.job.branch.try_clone()?,
                target_ref: target.try_clone()?,
                commit_subject: subject.try_clone()?,
                commit_body: body.try_clone()?,
                authentication: self.workspace_authentication(workspace, view.job.worker())?,
            })?;
        }
        let client = self.choose_runner(now)?;
        let payload = p::RepositoryMergePayload {
            merge: requests[0].try_clone()?,
        };
        let work = Merge {
            session: session.id.try_clone()?,
            workspaces,
            requests,
            at: 0,
            result: d::WorkflowActionResult {
                r#type: try_string(kind)?,
                results: d::WireMap::new(),
                created_at: now.try_clone()?,
                ..Default::default()
            },
        };
        self.store.mark_workflow_phase_running(&session.id, now)?;
        match self.enqueue_call(
            Action::Merge(work),
            &client,
            p::METHOD_MERGE_REPOSITORY,
            &payload,
            now,
            random,
        ) {
            Ok(wait) => {
                self.detach_capsule(wait);
                Ok(())
            }
            Err(error) => {
                self.store.requeue_workflow_phase(&session.id)?;
                Err(error)
            }
        }
    }
    pub(crate) fn advance_merge(
        &mut self,
        index: usize,
        client: &str,
        message: &p::WireMessage,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let Action::Merge(work) = &mut self.calls[index].action else {
            return Err(Error::Http(500, "missing workflow merge"));
        };
        let id = work.session.try_clone()?;
        let view = self.store.workflow_for_session(&id)?;
        if view.job.current_phase_run_id != view.run.id || view.run.status != d::PHASE_RUN_RUNNING {
            return Err(Error::Http(409, "workflow phase changed during merge"));
        }
        if !message.error.is_empty() {
            let conflict = message
                .error
                .as_bytes()
                .windows(8)
                .any(|b| b.eq_ignore_ascii_case(b"conflict"));
            return self.finish_action(&id,"reject",if conflict { "De Job-branch conflicteert met de basisbranch; los het conflict op." } else { "De runner kon de Job-branch niet mergen; controleer de Git-verbinding en runner." },!conflict, now, random).map(Some);
        }
        let result = d::engine::WorkspaceMergeResult::from_value(
            message.payload.0.as_ref().unwrap_or(&Value::Null),
        )?;
        if !matches!(result.head.len(), 40 | 64)
            || !result.head.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(Error::Http(502, "merge returned an invalid commit"));
        }
        let workspace = &work.workspaces[work.at];
        let request = &work.requests[work.at];
        work.result.results.insert(
            workspace.repository_id.try_clone()?,
            result.head.try_clone()?,
        )?;
        if work.at == 0 {
            work.result.external_id = result.head.try_clone()?;
            if workspace.remote_url.starts_with("https://") {
                let base = workspace.remote_url.trim_end_matches(".git");
                work.result.url = match workspace.provider.as_str() {
                    "github" => text(format_args!("{base}/commit/{}", result.head))?,
                    "gitlab" => text(format_args!("{base}/-/commit/{}", result.head))?,
                    _ => String::new(),
                };
            }
        }
        if !work.result.detail.is_empty() {
            d::try_push_str(&mut work.result.detail, "; ")?;
        }
        d::try_push_str(
            &mut work.result.detail,
            &text(format_args!(
                "{}: {} gemerged in {} met merge-commit {}",
                workspace.repository_name, request.source_ref, request.target_ref, result.head
            ))?,
        )?;
        work.at += 1;
        if let Some(next) = work.requests.get(work.at) {
            let payload = p::RepositoryMergePayload {
                merge: next.try_clone()?,
            };
            self.continue_capsule(index, client, p::METHOD_MERGE_REPOSITORY, &payload, random)?;
            return Ok(None);
        }
        let result = work.result.try_clone()?;
        let detail = result.detail.try_clone()?;
        self.store.set_workflow_action_result(&id, result, now)?;
        self.finish_action(&id, "accept", &detail, false, now, random)
            .map(Some)
    }
    pub(crate) fn fail_merge_reply(
        &mut self,
        index: usize,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        let Action::Merge(work) = &self.calls[index].action else {
            return Ok(());
        };
        if self
            .store
            .workflow_for_session(&work.session)
            .is_ok_and(|v| {
                v.run.status == d::PHASE_RUN_RUNNING && v.job.current_phase_run_id == v.run.id
            })
        {
            let session = work.session.try_clone()?;
            self.finish_action(&session, "reject", "Het merge-resultaat kon niet worden bevestigd. Controleer de Git-branch voordat je opnieuw beslist.", true, now, random)?;
        }
        Ok(())
    }
    pub(crate) fn finish_action(
        &mut self,
        id: &str,
        outcome: &str,
        detail: &str,
        ask: bool,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Response> {
        let advance = self.store.complete_workflow_phase(
            id,
            outcome,
            detail,
            ask,
            Mutation { now, ids: random },
        )?;
        if let Some(next) = &advance.next_session {
            self.schedule_session(next, now, random)?;
        }
        self.last_launch_sweep = None;
        Response::json(200, &advance)
    }
}
