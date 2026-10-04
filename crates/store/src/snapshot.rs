//! Publieke snapshots bevatten geen wachtwoordhashes, tokens of loginbestanden.
use crate::configuration::{redact_git, redact_mcp};
use crate::users::public_user;
use crate::{Error, Persistence, Result, Store};
use alloc::vec::Vec;
use spin_domain::{self as d, List, Timestamp, TryClone};

pub(crate) fn by_time<T>(
    list: &mut List<T>,
    key: fn(&T) -> &Timestamp,
    descending: bool,
) -> Result {
    let mut sorted = Vec::new();
    for value in core::mem::take(list).into_vec() {
        d::try_push(&mut sorted, (key(&value).time()?, value))?;
    }
    sorted.sort_unstable_by(|a, b| {
        if descending {
            b.0.cmp(&a.0)
        } else {
            a.0.cmp(&b.0)
        }
    });
    *list = List::new();
    for (_, value) in sorted {
        list.push(value)?;
    }
    Ok(())
}
/// Gesloten zoals de UI het ziet: afgerond, geannuleerd of workflow klaar.
pub fn job_is_closed(job: &d::Job) -> bool {
    job.status == d::JOB_DONE
        || job.status == d::JOB_CANCELLED
        || job.workflow_status == d::WORKFLOW_DONE
}
pub(crate) fn review_summary(r: &d::CodeReviewRevision) -> Result<d::CodeReviewRevisionSummary> {
    Ok(d::CodeReviewRevisionSummary {
        id: r.id.try_clone()?,
        job_id: r.job_id.try_clone()?,
        source_phase_run_id: r.source_phase_run_id.try_clone()?,
        context_phase_run_id: r.context_phase_run_id.try_clone()?,
        session_id: r.session_id.try_clone()?,
        phase_id: r.phase_id.try_clone()?,
        phase_name: r.phase_name.try_clone()?,
        attempt: r.attempt,
        scope: r.scope.try_clone()?,
        scope_key: r.scope_key.try_clone()?,
        branch: r.branch.try_clone()?,
        added: r.added,
        deleted: r.deleted,
        file_count: i64::try_from(r.files.len())
            .map_err(|_| Error::Conflict("too many review files"))?,
        created_by: r.created_by.try_clone()?,
        created_at: r.created_at.try_clone()?,
    })
}
impl<P: Persistence> Store<P> {
    /// Een consistent, geredigeerd beeld; alle lege collecties blijven JSON-arrays.
    pub fn snapshot(&self) -> Result<d::Snapshot> {
        self.snapshot_where(|_, _| true)
    }
    /// De snapshot zonder gesloten jobs en alles wat erbij hoort: wat de
    /// browser live bijhoudt. De geschiedenis komt per pagina.
    pub fn live_snapshot(&self) -> Result<d::Snapshot> {
        self.snapshot_where(|collection, id| !self.is_history(collection, id))
    }
    /// Gesloten jobs, nieuwste eerst, die `query` bevatten in titel,
    /// referentie of branch (zoals het zoekveld van de UI); `offset` en `limit`
    /// kiezen de pagina. Geeft het totaal en de snapshot van die jobs met alles
    /// wat erbij hoort.
    pub fn closed_jobs(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(usize, d::Snapshot)> {
        let query = query.trim().to_lowercase();
        let mut closed = Vec::new();
        for (_, job) in self.state.jobs.iter() {
            if job_is_closed(job)
                && (query.is_empty()
                    || [&job.title, &job.reference, &job.branch]
                        .iter()
                        .any(|v| v.to_lowercase().contains(&query)))
            {
                d::try_push(&mut closed, (job.created_at.time()?, job.id.as_str()))?;
            }
        }
        closed.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        let page: Vec<&str> = closed
            .iter()
            .skip(offset)
            .take(limit)
            .map(|(_, id)| *id)
            .collect();
        let snapshot = self.snapshot_where(|collection, id| {
            self.owner_job(collection, id)
                .is_some_and(|job| page.contains(&job))
        })?;
        Ok((closed.len(), snapshot))
    }
    /// Het aantal gesloten jobs.
    pub fn closed_job_count(&self) -> usize {
        self.state
            .jobs
            .iter()
            .filter(|(_, j)| job_is_closed(j))
            .count()
    }
    /// De job waartoe een entiteit hoort; een draaiende capsule hoort nergens
    /// bij, die blijft altijd live.
    fn owner_job<'a>(&'a self, collection: &str, id: &'a str) -> Option<&'a str> {
        let s = &self.state;
        let session = |id: &str| s.sessions.get(id).map(|x| x.job_id.as_str());
        match collection {
            "jobs" => Some(id),
            "job_attachments" => s.job_attachments.get(id).map(|x| x.job_id.as_str()),
            "phase_runs" => s.phase_runs.get(id).map(|x| x.job_id.as_str()),
            "deliverables" => s.deliverables.get(id).map(|x| x.job_id.as_str()),
            "code_review_revisions" => s.code_review_revisions.get(id).map(|x| x.job_id.as_str()),
            "workflow_questions" => s.workflow_questions.get(id).map(|x| x.job_id.as_str()),
            "sessions" => session(id),
            "results" => s.results.get(id).map(|x| x.job_id.as_str()),
            "deliverable_comments" => s
                .deliverable_comments
                .get(id)
                .and_then(|c| s.deliverables.get(&c.deliverable_id))
                .map(|x| x.job_id.as_str()),
            "code_review_comments" => s
                .code_review_comments
                .get(id)
                .and_then(|c| s.code_review_revisions.get(&c.revision_id))
                .map(|x| x.job_id.as_str()),
            "activations" => s.activations.get(id).and_then(|x| session(&x.session_id)),
            "turns" => s.turns.get(id).and_then(|x| session(&x.session_id)),
            "checkpoints" => s.checkpoints.get(id).and_then(|x| session(&x.session_id)),
            "compositions" => s
                .compositions
                .get(id)
                .filter(|c| c.runtime.as_ref().is_none_or(|r| r.status == "stopped"))
                .and_then(|c| session(&c.session_id)),
            _ => None,
        }
    }
    /// Of een entiteit bij een gesloten job hoort.
    pub fn is_history(&self, collection: &str, id: &str) -> bool {
        self.owner_job(collection, id)
            .and_then(|job| self.state.jobs.get(job))
            .is_some_and(job_is_closed)
    }
    fn snapshot_where(&self, keep: impl Fn(&str, &str) -> bool) -> Result<d::Snapshot> {
        let mut out = d::Snapshot {
            artifacts: List::new(),
            recordings: List::new(),
            compositions: List::new(),
            jobs: List::new(),
            job_attachments: List::new(),
            workflow_templates: List::new(),
            phase_runs: List::new(),
            deliverables: List::new(),
            deliverable_comments: List::new(),
            code_review_revisions: List::new(),
            code_review_comments: List::new(),
            workflow_questions: List::new(),
            sessions: List::new(),
            activations: List::new(),
            turns: List::new(),
            checkpoints: List::new(),
            results: List::new(),
            clients: List::new(),
            mcp_servers: List::new(),
            git_repositories: List::new(),
            git_accounts: List::new(),
            users: List::new(),
            logins: List::new(),
        };
        for (id, value) in self.state.artifacts.iter() {
            if !keep("artifacts", id) {
                continue;
            }
            out.artifacts.push(value.try_clone()?)?;
        }
        by_time(&mut out.artifacts, |v| &v.created_at, true)?;
        for (id, value) in self.state.recordings.iter() {
            if !keep("recordings", id) {
                continue;
            }
            out.recordings.push(value.try_clone()?)?;
        }
        by_time(&mut out.recordings, |v| &v.started_at, true)?;
        for (id, value) in self.state.compositions.iter() {
            if !keep("compositions", id) {
                continue;
            }
            out.compositions.push(value.try_clone()?)?;
        }
        by_time(&mut out.compositions, |v| &v.created_at, true)?;
        for (id, value) in self.state.jobs.iter() {
            if !keep("jobs", id) {
                continue;
            }
            out.jobs.push(value.try_clone()?)?;
        }
        by_time(&mut out.jobs, |v| &v.created_at, true)?;
        for (id, value) in self.state.job_attachments.iter() {
            if !keep("job_attachments", id) {
                continue;
            }
            if value.job_id.is_empty() {
                continue;
            }
            out.job_attachments.push(value.try_clone()?)?;
        }
        by_time(&mut out.job_attachments, |v| &v.created_at, false)?;
        for (id, value) in self.state.workflow_templates.iter() {
            if !keep("workflow_templates", id) {
                continue;
            }
            out.workflow_templates.push(value.try_clone()?)?;
        }
        out.workflow_templates
            .as_mut_slice()
            .sort_unstable_by(|a, b| a.name.cmp(&b.name));
        for (id, value) in self.state.phase_runs.iter() {
            if !keep("phase_runs", id) {
                continue;
            }
            out.phase_runs.push(value.try_clone()?)?;
        }
        by_time(&mut out.phase_runs, |v| &v.started_at, false)?;
        for (id, value) in self.state.deliverables.iter() {
            if !keep("deliverables", id) {
                continue;
            }
            out.deliverables.push(value.try_clone()?)?;
        }
        by_time(&mut out.deliverables, |v| &v.created_at, false)?;
        for (id, value) in self.state.deliverable_comments.iter() {
            if !keep("deliverable_comments", id) {
                continue;
            }
            out.deliverable_comments.push(value.try_clone()?)?;
        }
        by_time(&mut out.deliverable_comments, |v| &v.created_at, false)?;
        for (id, value) in self.state.code_review_revisions.iter() {
            if !keep("code_review_revisions", id) {
                continue;
            }
            out.code_review_revisions.push(review_summary(value)?)?;
        }
        by_time(&mut out.code_review_revisions, |v| &v.created_at, false)?;
        for (id, value) in self.state.code_review_comments.iter() {
            if !keep("code_review_comments", id) {
                continue;
            }
            out.code_review_comments.push(value.try_clone()?)?;
        }
        by_time(&mut out.code_review_comments, |v| &v.created_at, false)?;
        for (id, value) in self.state.workflow_questions.iter() {
            if !keep("workflow_questions", id) {
                continue;
            }
            out.workflow_questions.push(value.try_clone()?)?;
        }
        by_time(&mut out.workflow_questions, |v| &v.created_at, false)?;
        for (id, value) in self.state.sessions.iter() {
            if !keep("sessions", id) {
                continue;
            }
            out.sessions.push(value.try_clone()?)?;
        }
        by_time(&mut out.sessions, |v| &v.created_at, false)?;
        for (id, value) in self.state.activations.iter() {
            if !keep("activations", id) {
                continue;
            }
            out.activations.push(value.try_clone()?)?;
        }
        by_time(&mut out.activations, |v| &v.started_at, false)?;
        for (id, value) in self.state.turns.iter() {
            if !keep("turns", id) {
                continue;
            }
            out.turns.push(value.try_clone()?)?;
        }
        by_time(&mut out.turns, |v| &v.started_at, false)?;
        for (id, value) in self.state.checkpoints.iter() {
            if !keep("checkpoints", id) {
                continue;
            }
            out.checkpoints.push(value.try_clone()?)?;
        }
        by_time(&mut out.checkpoints, |v| &v.created_at, false)?;
        for (id, value) in self.state.results.iter() {
            if !keep("results", id) {
                continue;
            }
            out.results.push(value.try_clone()?)?;
        }
        by_time(&mut out.results, |v| &v.created_at, false)?;
        for (id, value) in self.state.clients.iter() {
            if !keep("clients", id) {
                continue;
            }
            out.clients.push(value.try_clone()?)?;
        }
        out.clients
            .as_mut_slice()
            .sort_unstable_by(|a, b| a.name.cmp(&b.name));
        for (id, value) in self.state.mcp_servers.iter() {
            if !keep("mcp_servers", id) {
                continue;
            }
            out.mcp_servers.push(redact_mcp(value.try_clone()?))?;
        }
        by_time(&mut out.mcp_servers, |v| &v.created_at, true)?;
        for (id, value) in self.state.git_repositories.iter() {
            if !keep("git_repositories", id) {
                continue;
            }
            out.git_repositories.push(value.try_clone()?)?;
        }
        out.git_repositories
            .as_mut_slice()
            .sort_unstable_by(|a, b| a.name.cmp(&b.name));
        for (id, value) in self.state.git_accounts.iter() {
            if !keep("git_accounts", id) {
                continue;
            }
            out.git_accounts.push(redact_git(value.try_clone()?))?;
        }
        out.git_accounts.as_mut_slice().sort_unstable_by(|a, b| {
            (&a.operator, &a.provider, &a.login).cmp(&(&b.operator, &b.provider, &b.login))
        });
        for (id, value) in self.state.users.iter() {
            if !keep("users", id) {
                continue;
            }
            out.users.push(public_user(value)?)?;
        }
        out.users
            .as_mut_slice()
            .sort_unstable_by(|a, b| a.username.cmp(&b.username));
        if keep("logins", "") {
            out.logins = self.login_summaries()?;
        }
        Ok(out)
    }
}

impl<P: Persistence> Store<P> {
    /// Eén entiteit zoals [`Store::live_snapshot`] hem toont, met dezelfde
    /// redactie; `None` als hij er niet (meer) in staat of geschiedenis werd. Collecties buiten de snapshot
    /// (en `logins`, een afgeleide lijst) geven altijd `None`.
    pub fn snapshot_entity(&self, collection: &str, id: &str) -> Result<Option<d::json::Value>> {
        use d::Wire;
        fn value<T: Wire>(item: Option<&T>) -> Result<Option<d::json::Value>> {
            Ok(match item {
                Some(item) => Some(item.to_value()?),
                None => None,
            })
        }
        if self.is_history(collection, id) {
            return Ok(None);
        }
        let s = &self.state;
        match collection {
            "artifacts" => value(s.artifacts.get(id)),
            "recordings" => value(s.recordings.get(id)),
            "compositions" => value(s.compositions.get(id)),
            "jobs" => value(s.jobs.get(id)),
            "job_attachments" => value(s.job_attachments.get(id).filter(|a| !a.job_id.is_empty())),
            "workflow_templates" => value(s.workflow_templates.get(id)),
            "phase_runs" => value(s.phase_runs.get(id)),
            "deliverables" => value(s.deliverables.get(id)),
            "deliverable_comments" => value(s.deliverable_comments.get(id)),
            "code_review_revisions" => match s.code_review_revisions.get(id) {
                Some(r) => Ok(Some(review_summary(r)?.to_value()?)),
                None => Ok(None),
            },
            "code_review_comments" => value(s.code_review_comments.get(id)),
            "workflow_questions" => value(s.workflow_questions.get(id)),
            "sessions" => value(s.sessions.get(id)),
            "activations" => value(s.activations.get(id)),
            "turns" => value(s.turns.get(id)),
            "checkpoints" => value(s.checkpoints.get(id)),
            "results" => value(s.results.get(id)),
            "clients" => value(s.clients.get(id)),
            "mcp_servers" => match s.mcp_servers.get(id) {
                Some(m) => Ok(Some(redact_mcp(m.try_clone()?).to_value()?)),
                None => Ok(None),
            },
            "git_repositories" => value(s.git_repositories.get(id)),
            "git_accounts" => match s.git_accounts.get(id) {
                Some(g) => Ok(Some(redact_git(g.try_clone()?).to_value()?)),
                None => Ok(None),
            },
            "users" => match s.users.get(id) {
                Some(u) => Ok(Some(public_user(u)?.to_value()?)),
                None => Ok(None),
            },
            _ => Ok(None),
        }
    }
    /// Of een collectie van de opgeslagen state ook in de snapshot staat.
    pub fn in_snapshot(collection: &str) -> bool {
        matches!(
            collection,
            "artifacts"
                | "recordings"
                | "compositions"
                | "jobs"
                | "job_attachments"
                | "workflow_templates"
                | "phase_runs"
                | "deliverables"
                | "deliverable_comments"
                | "code_review_revisions"
                | "code_review_comments"
                | "workflow_questions"
                | "sessions"
                | "activations"
                | "turns"
                | "checkpoints"
                | "results"
                | "clients"
                | "mcp_servers"
                | "git_repositories"
                | "git_accounts"
                | "users"
        )
    }
    /// De aanbevelingen; die lezen alleen jobs en sessions, in snapshotvolgorde.
    pub fn recommendations(&self) -> Result<List<d::Recommendation>> {
        let mut partial = d::Snapshot::default();
        for (_, job) in self.state.jobs.iter() {
            partial.jobs.push(job.try_clone()?)?;
        }
        by_time(&mut partial.jobs, |v| &v.created_at, true)?;
        for (_, session) in self.state.sessions.iter() {
            partial.sessions.push(session.try_clone()?)?;
        }
        by_time(&mut partial.sessions, |v| &v.created_at, false)?;
        Ok(spin_core::orchestrator::recommend(&partial)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Memory;
    use core::cell::Cell;
    use d::{Wire, state::PersistedState};
    #[test]
    fn closed_jobs_leave_the_live_snapshot_and_come_back_per_page() {
        let fail = Cell::new(false);
        let state = PersistedState::from_json(br#"{
            "jobs":{
                "open":{"id":"open","status":"active","title":"Open","created_at":"2026-10-01T00:00:00Z"},
                "old":{"id":"old","status":"done","title":"Oud werk","created_at":"2026-09-01T00:00:00Z"},
                "new":{"id":"new","status":"cancelled","title":"Nieuw","created_at":"2026-09-20T00:00:00Z"},
                "flow":{"id":"flow","status":"active","workflow_status":"done","title":"Klaar","created_at":"2026-09-10T00:00:00Z"}
            },
            "sessions":{"s-open":{"id":"s-open","job_id":"open"},"s-old":{"id":"s-old","job_id":"old"}},
            "phase_runs":{"r-old":{"id":"r-old","job_id":"old","session_id":"s-old"}},
            "deliverables":{"d-old":{"id":"d-old","job_id":"old"}},
            "deliverable_comments":{"c-old":{"id":"c-old","deliverable_id":"d-old"}},
            "compositions":{
                "stopped":{"id":"stopped","session_id":"s-old","runtime":{"status":"stopped"}},
                "running":{"id":"running","session_id":"s-old","runtime":{"status":"ready"}}
            }
        }"#).unwrap();
        let store = Store::new(state, Memory(&fail));
        let live = store.live_snapshot().unwrap();
        fn ids(list: &List<d::Job>) -> Vec<alloc::string::String> {
            list.iter().map(|j| j.id.clone()).collect()
        }
        assert_eq!(ids(&live.jobs), ["open"]);
        assert_eq!(live.sessions.len(), 1);
        assert!(live.phase_runs.is_empty() && live.deliverable_comments.is_empty());
        // Een draaiende capsule blijft live, ook bij een gesloten job.
        assert_eq!(live.compositions.len(), 1);
        assert_eq!(live.compositions[0].id, "running");
        assert!(
            store
                .snapshot_entity("sessions", "s-old")
                .unwrap()
                .is_none()
        );
        assert_eq!(store.closed_job_count(), 3);
        let (total, page) = store.closed_jobs("", 0, 2).unwrap();
        assert_eq!(total, 3);
        assert_eq!(ids(&page.jobs), ["new", "flow"]);
        let (_, page) = store.closed_jobs("", 2, 2).unwrap();
        assert_eq!(ids(&page.jobs), ["old"]);
        assert_eq!(page.sessions.len(), 1);
        assert_eq!(page.deliverable_comments.len(), 1);
        assert_eq!(page.compositions.len(), 1);
        assert!(page.logins.is_empty() && page.users.is_empty());
        let (total, page) = store.closed_jobs("OUD", 0, 25).unwrap();
        assert_eq!(
            (total, ids(&page.jobs)),
            (1, alloc::vec![alloc::string::String::from("old")])
        );
    }
    #[test]
    fn snapshot_never_exports_secrets_and_empty_collections_are_arrays() {
        let fail = Cell::new(false);
        let store = Store::new(PersistedState::default(), Memory(&fail));
        let snapshot = store.snapshot().unwrap().to_value().unwrap();
        for (_, value) in snapshot.as_object().unwrap().iter() {
            assert!(value.as_array().is_some());
        }
        let state=PersistedState::from_json(br#"{
            "worker_token":"hidden-worker","workflow_tokens":{"t":"hidden-workflow"},
            "users":{"u":{"username":"derek","password_hash":"hidden-password"}},
            "auth_sessions":{"s":{"token_hash":"hidden-session","csrf_hash":"hidden-csrf"}},
            "mcp_servers":{"m":{"env":[{"name":"TOKEN","value":"hidden-env"}],"headers":[{"name":"Auth","value":"hidden-header"}]}},
            "git_accounts":{"g":{"access_token":"hidden-access","refresh_token":"hidden-refresh"}},
            "git_oauth_configurations":{"github":{"client_secret":"hidden-oauth"}},
            "logins":{"l":{"files":{"/auth":"aGlkZGVuLWxvZ2lu"}}},
            "job_attachments":{"staged":{"id":"staged","job_id":""},"attached":{"id":"attached","job_id":"j"}}
        }"#).unwrap();
        let store = Store::new(state, Memory(&fail));
        let snapshot = store.snapshot().unwrap();
        let encoded = snapshot.to_json().unwrap();
        assert!(!encoded.contains("hidden-"));
        assert!(!encoded.contains("aGlkZGVuLWxvZ2lu"));
        assert_eq!(snapshot.job_attachments.len(), 1);
        assert_eq!(snapshot.job_attachments[0].id, "attached");
        assert_eq!(snapshot.logins[0].bytes, 12);
        assert_eq!(
            store.state.mcp_servers.get("m").unwrap().env[0].value,
            "hidden-env"
        );
    }
}
