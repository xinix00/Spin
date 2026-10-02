//! Reviewrevisies zijn historisch; annotaties horen uitsluitend bij de huidige poging.
use crate::snapshot::{by_time, review_summary};
use crate::{Context, Error, Persistence, Result, Store};
use spin_core::validation::normalized;
use spin_domain::{self as d, List, TryClone, state::PersistedState, try_string};

fn latest<'a>(state: &'a PersistedState, reference: &d::CodeReviewRevision) -> Result<&'a str> {
    let attempt = if reference.scope == "phase" {
        state
            .phase_runs
            .iter()
            .filter(|(_, r)| r.job_id == reference.job_id && r.phase_id == reference.phase_id)
            .map(|(_, r)| r.attempt)
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    let mut id = "";
    let mut time = d::Time::default();
    for (_, candidate) in state.code_review_revisions.iter() {
        if candidate.scope_key != reference.scope_key
            || (reference.scope == "phase" && candidate.attempt != attempt)
        {
            continue;
        }
        let created = candidate.created_at.time()?;
        if id.is_empty() || created > time {
            id = &candidate.id;
            time = created;
        }
    }
    Ok(id)
}
impl<P: Persistence> Store<P> {
    /// Dezelfde digest en broncontext leveren dezelfde revisie terug.
    pub fn save_code_review_revision(
        &mut self,
        mut revision: d::CodeReviewRevision,
        context: Context<'_>,
    ) -> Result<d::CodeReviewRevision> {
        context.validate()?;
        revision.created_by = normalized(&revision.created_by)?;
        revision.job_id = try_string(revision.job_id.trim())?;
        revision.session_id = try_string(revision.session_id.trim())?;
        revision.scope = normalized(&revision.scope)?;
        revision.scope_key = try_string(revision.scope_key.trim())?;
        revision.digest = try_string(revision.digest.trim())?;
        if revision.created_by.is_empty()
            || revision.job_id.is_empty()
            || revision.scope_key.is_empty()
            || revision.digest.is_empty()
            || !matches!(revision.scope.as_str(), "job" | "phase")
        {
            return Err(Error::Conflict(
                "author, Job, scope and digest are required",
            ));
        }
        if revision.files.is_empty() {
            revision.files = List::new();
        }
        if self.state.jobs.get(&revision.job_id).is_none() {
            return Err(Error::NotFound);
        }
        for id in [
            &revision.source_phase_run_id,
            &revision.context_phase_run_id,
        ] {
            if !id.is_empty()
                && self
                    .state
                    .phase_runs
                    .get(id)
                    .is_none_or(|run| run.job_id != revision.job_id)
            {
                return Err(Error::NotFound);
            }
        }
        if let Some((_, old)) = self.state.code_review_revisions.iter().find(|(_, old)| {
            old.scope_key == revision.scope_key
                && old.digest == revision.digest
                && old.source_phase_run_id == revision.source_phase_run_id
                && old.context_phase_run_id == revision.context_phase_run_id
                && old.live == revision.live
        }) {
            return Ok(old.try_clone()?);
        }
        self.edit(|state| {
            if state.code_review_revisions.get(context.id).is_some() {
                return Err(Error::Conflict("revision id already exists"));
            }
            revision.id = try_string(context.id)?;
            revision.created_at = context.now.try_clone()?;
            state
                .code_review_revisions
                .insert(revision.id.try_clone()?, revision.try_clone()?)?;
            Ok(revision)
        })
    }
    /// Leent de volledige revisie voor het tonen van een diff.
    pub fn code_review_revision(&self, id: &str) -> Result<&d::CodeReviewRevision> {
        self.state
            .code_review_revisions
            .get(id.trim())
            .ok_or(Error::NotFound)
    }
    /// Historie en annotaties staan in tijdvolgorde; fasepogingen gaan vóór datum.
    pub fn code_review_bundle(&self, id: &str) -> Result<d::CodeReviewBundle> {
        let revision = self.code_review_revision(id)?;
        let mut history = List::new();
        let mut comments = List::new();
        for (_, candidate) in self
            .state
            .code_review_revisions
            .iter()
            .filter(|(_, r)| r.scope_key == revision.scope_key)
        {
            history.push(review_summary(candidate)?)?;
        }
        for (_, comment) in self
            .state
            .code_review_comments
            .iter()
            .filter(|(_, c)| c.revision_id == revision.id)
        {
            comments.push(comment.try_clone()?)?;
        }
        by_time(&mut history, |r| &r.created_at, false)?;
        // Insertion sort behoudt de reeds bepaalde tijdvolgorde zonder verborgen allocatie.
        if revision.scope == "phase" {
            let values = history.as_mut_slice();
            for i in 1..values.len() {
                let mut j = i;
                while j > 0
                    && values
                        .get(j - 1)
                        .zip(values.get(j))
                        .is_some_and(|(a, b)| a.attempt > b.attempt)
                {
                    values.swap(j - 1, j);
                    j -= 1;
                }
            }
        }
        by_time(&mut comments, |c| &c.created_at, false)?;
        let latest = latest(&self.state, revision)?;
        Ok(d::CodeReviewBundle {
            revision: revision.try_clone()?,
            history,
            comments,
            latest_revision_id: try_string(latest)?,
            annotatable: latest == revision.id,
        })
    }
    /// Historische revisies blijven leesbaar, maar krijgen geen nieuwe annotaties.
    pub fn add_code_review_comment(
        &mut self,
        revision_id: &str,
        author: &str,
        req: d::CreateCodeReviewCommentRequest,
        context: Context<'_>,
    ) -> Result<d::CodeReviewComment> {
        context.validate()?;
        let author = normalized(author)?;
        let path = req.path.trim();
        let side = normalized(&req.side)?;
        let selected = req.selected_text.trim();
        let body = req.body.trim();
        if author.is_empty()
            || path.is_empty()
            || !matches!(side.as_str(), "old" | "new")
            || req.start_line < 1
            || req.end_line < req.start_line
            || selected.is_empty()
            || body.is_empty()
        {
            return Err(Error::Conflict(
                "path, side, line selection and comment are required",
            ));
        }
        if selected.len() > 32 * 1024 || body.len() > 8 * 1024 {
            return Err(Error::Conflict("code selection or comment is too large"));
        }
        self.edit(|state| {
            let revision = state
                .code_review_revisions
                .get(revision_id.trim())
                .ok_or(Error::NotFound)?;
            if latest(state, revision)? != revision.id {
                return Err(Error::Conflict(
                    "code review revision is historical; open the latest changes",
                ));
            }
            if !revision.files.iter().any(|f| f.path == path) {
                return Err(Error::NotFound);
            }
            if state.code_review_comments.get(context.id).is_some() {
                return Err(Error::Conflict("comment id already exists"));
            }
            let comment = d::CodeReviewComment {
                id: try_string(context.id)?,
                revision_id: revision.id.try_clone()?,
                path: try_string(path)?,
                side,
                start_line: req.start_line,
                end_line: req.end_line,
                selected: try_string(selected)?,
                body: try_string(body)?,
                author,
                created_at: context.now.try_clone()?,
            };
            state
                .code_review_comments
                .insert(comment.id.try_clone()?, comment.try_clone()?)?;
            Ok(comment)
        })
    }
    /// Review is collaboratief; de HTTP-ingang controleert de authenticatie.
    pub fn job_workspace_history(&self, id: &str) -> Result<(d::Job, List<d::Composition>)> {
        let job = self.state.jobs.get(id.trim()).ok_or(Error::NotFound)?;
        let mut compositions = List::new();
        for (_, composition) in self.state.compositions.iter() {
            if self
                .state
                .sessions
                .get(&composition.session_id)
                .is_some_and(|s| s.job_id == job.id)
            {
                compositions.push(composition.try_clone()?)?;
            }
        }
        by_time(&mut compositions, |c| &c.created_at, false)?;
        Ok((job.try_clone()?, compositions))
    }
}
