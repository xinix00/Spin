//! Welke entiteiten een mutatie veranderde: de basis voor delta's naar de
//! browser (en straks voor het schrijven per rij).
use crate::Result;
use alloc::{collections::VecDeque, string::String};
use spin_domain::{List, WireMap, state::PersistedState, try_string};

/// Eén gewijzigde, toegevoegde of verwijderde entiteit.
#[derive(Debug, PartialEq)]
pub struct Change {
    /// De collectie in de opgeslagen state, bijvoorbeeld `jobs`.
    pub collection: &'static str,
    /// De sleutel in die collectie; leeg voor een losse waarde.
    pub id: String,
}
fn map<T: PartialEq>(
    collection: &'static str,
    before: &WireMap<T>,
    after: &WireMap<T>,
    out: &mut List<Change>,
) -> Result {
    for (id, value) in after.iter() {
        if before.get(id) != Some(value) {
            out.push(Change {
                collection,
                id: try_string(id)?,
            })?;
        }
    }
    for (id, _) in before.iter() {
        if after.get(id).is_none() {
            out.push(Change {
                collection,
                id: try_string(id)?,
            })?;
        }
    }
    Ok(())
}
/// Vergelijkt twee states per entiteit; geen serialisatie.
pub(crate) fn diff(before: &PersistedState, after: &PersistedState) -> Result<List<Change>> {
    let mut out = List::new();
    let o = &mut out;
    map("artifacts", &before.artifacts, &after.artifacts, o)?;
    map("recordings", &before.recordings, &after.recordings, o)?;
    map("compositions", &before.compositions, &after.compositions, o)?;
    map("jobs", &before.jobs, &after.jobs, o)?;
    map(
        "job_attachments",
        &before.job_attachments,
        &after.job_attachments,
        o,
    )?;
    map(
        "workflow_templates",
        &before.workflow_templates,
        &after.workflow_templates,
        o,
    )?;
    map("phase_runs", &before.phase_runs, &after.phase_runs, o)?;
    map("deliverables", &before.deliverables, &after.deliverables, o)?;
    map(
        "deliverable_comments",
        &before.deliverable_comments,
        &after.deliverable_comments,
        o,
    )?;
    map(
        "code_review_revisions",
        &before.code_review_revisions,
        &after.code_review_revisions,
        o,
    )?;
    map(
        "code_review_comments",
        &before.code_review_comments,
        &after.code_review_comments,
        o,
    )?;
    map(
        "workflow_questions",
        &before.workflow_questions,
        &after.workflow_questions,
        o,
    )?;
    map(
        "job_request_keys",
        &before.job_request_keys,
        &after.job_request_keys,
        o,
    )?;
    map("sessions", &before.sessions, &after.sessions, o)?;
    map("activations", &before.activations, &after.activations, o)?;
    map("turns", &before.turns, &after.turns, o)?;
    map("checkpoints", &before.checkpoints, &after.checkpoints, o)?;
    map("results", &before.results, &after.results, o)?;
    map("clients", &before.clients, &after.clients, o)?;
    map("mcp_servers", &before.mcp_servers, &after.mcp_servers, o)?;
    map(
        "git_repositories",
        &before.git_repositories,
        &after.git_repositories,
        o,
    )?;
    map("git_accounts", &before.git_accounts, &after.git_accounts, o)?;
    map("users", &before.users, &after.users, o)?;
    map(
        "auth_sessions",
        &before.auth_sessions,
        &after.auth_sessions,
        o,
    )?;
    map(
        "git_oauth_configurations",
        &before.git_oauth_configurations,
        &after.git_oauth_configurations,
        o,
    )?;
    map("logins", &before.logins, &after.logins, o)?;
    map(
        "workflow_tokens",
        &before.workflow_tokens,
        &after.workflow_tokens,
        o,
    )?;
    map("login_states", &before.login_states, &after.login_states, o)?;
    for (collection, changed) in [
        ("garbage_refs", before.garbage_refs != after.garbage_refs),
        ("worker_token", before.worker_token != after.worker_token),
    ] {
        if changed {
            o.push(Change {
                collection,
                id: String::new(),
            })?;
        }
    }
    Ok(out)
}
/// Zoveel mutaties onthoudt de Store; een browser die verder achterloopt,
/// krijgt het hele document.
const LOG_LEN: usize = 512;
/// De wijzigingen van de laatste mutaties, met hun versie.
#[derive(Default)]
pub(crate) struct Log(VecDeque<(u64, List<Change>)>);
impl Log {
    /// Kan het log niet groeien, dan begint het opnieuw: een browser krijgt
    /// dan het hele document in plaats van een gemiste wijziging.
    pub(crate) fn push(&mut self, version: u64, changes: List<Change>) {
        if self.0.len() >= LOG_LEN {
            self.0.pop_front();
        }
        if self.0.try_reserve(1).is_err() {
            self.0.clear();
            return;
        }
        self.0.push_back((version, changes));
    }
    /// Na een vervanging van de hele state is er geen aaneengesloten log meer.
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
    /// De entiteiten die na `version` veranderden, of `None` als het log daar
    /// niet meer (of nog niet) bij kan.
    pub(crate) fn since(&self, version: u64, current: u64) -> Result<Option<List<Change>>> {
        if version > current {
            return Ok(None);
        }
        if version < current && self.0.front().is_none_or(|(first, _)| *first > version + 1) {
            return Ok(None);
        }
        let mut out = List::<Change>::new();
        for (_, changes) in self.0.iter().filter(|(v, _)| *v > version) {
            for change in changes.iter() {
                if !out
                    .iter()
                    .any(|c| c.collection == change.collection && c.id == change.id)
                {
                    out.push(Change {
                        collection: change.collection,
                        id: try_string(&change.id)?,
                    })?;
                }
            }
        }
        Ok(Some(out))
    }
}
