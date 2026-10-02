//! Een Job gebruikt zijn eigen templatesnapshot; oude pogingen blijven historisch.
use crate::snapshot::by_time;
use crate::{Context, Error, Persistence, Result, Store};
use alloc::string::String;
use spin_core::validation::normalized;
use spin_domain::{self as d, List, Timestamp, TryClone, state::PersistedState, try_string};

mod lifecycle;
pub(crate) use lifecycle::new_session;

/// De actuele workflowcontext met alle deliverables en vragen van dezelfde Job.
pub struct WorkflowView {
    /// De duurzame Job.
    pub job: d::Job,
    /// De vastgelegde templateversie van deze Job.
    pub template: d::WorkflowTemplate,
    /// De poging van de opgevraagde Session.
    pub run: d::PhaseRun,
    /// De fase, eventueel de ingebouwde brainstorm.
    pub phase: d::WorkflowPhase,
    /// Alle documentrevisies in tijdvolgorde.
    pub deliverables: List<d::Deliverable>,
    /// Alle vragen en besluiten in tijdvolgorde.
    pub questions: List<d::WorkflowQuestion>,
}
pub(crate) struct Parts {
    pub job: d::Job,
    pub template: d::WorkflowTemplate,
    pub run: d::PhaseRun,
    pub phase: d::WorkflowPhase,
}
pub(crate) fn phase(template: &d::WorkflowTemplate, id: &str) -> Result<d::WorkflowPhase> {
    if id == d::BRAINSTORM_PHASE_ID {
        return Ok(d::brainstorm_phase()?);
    }
    Ok(template
        .phases
        .iter()
        .find(|p| p.id == id)
        .ok_or(Error::NotFound)?
        .try_clone()?)
}
pub(crate) fn parts(state: &PersistedState, id: &str) -> Result<Parts> {
    let session = state.sessions.get(id).ok_or(Error::NotFound)?;
    let job = state.jobs.get(&session.job_id).ok_or(Error::NotFound)?;
    let template = job
        .template_snapshot
        .as_ref()
        .or_else(|| state.workflow_templates.get(&job.template_id))
        .ok_or(Error::NotFound)?;
    let run = state
        .phase_runs
        .get(&session.phase_run_id)
        .filter(|r| r.session_id == id && r.job_id == job.id)
        .ok_or(Error::NotFound)?;
    let phase = phase(template, &run.phase_id)?;
    Ok(Parts {
        job: job.try_clone()?,
        template: template.try_clone()?,
        run: run.try_clone()?,
        phase,
    })
}
fn latest_deliverable<'a>(
    state: &'a PersistedState,
    job: &str,
    name: &str,
) -> Result<Option<&'a d::Deliverable>> {
    let name = normalized(name)?;
    let mut latest: Option<&d::Deliverable> = None;
    for (_, d) in state.deliverables.iter() {
        if d.job_id == job
            && normalized(&d.name)? == name
            && latest.is_none_or(|old| d.revision > old.revision)
        {
            latest = Some(d);
        }
    }
    Ok(latest)
}
fn shape(
    definition: &d::DeliverableDefinition,
    content: &str,
    bundle: Option<&d::DeliverableBundle>,
) -> Result {
    let kind = if definition.kind.is_empty() {
        d::DELIVERABLE_KIND_MARKDOWN
    } else {
        &definition.kind
    };
    if !d::deliverable_is_bundle(kind) {
        if bundle.is_some() || content.is_empty() || content.len() > 2 << 20 {
            return Err(Error::Conflict(
                "Markdown deliverable must contain 1 to 2 MiB of text",
            ));
        }
        return Ok(());
    }
    let b = bundle
        .filter(|b| !b.r#ref.is_empty() && b.files >= 1)
        .ok_or(Error::Conflict("deliverable needs a file or folder"))?;
    let single = !b.folder && b.files == 1 && !b.entry.is_empty();
    let mime = normalized(&b.content_type)?;
    match kind {
        d::DELIVERABLE_KIND_FOLDER if !b.folder => {
            Err(Error::Conflict("deliverable must be a folder"))
        }
        d::DELIVERABLE_KIND_PDF if !single || !mime.starts_with("application/pdf") => {
            Err(Error::Conflict("deliverable must be one PDF"))
        }
        d::DELIVERABLE_KIND_IMAGE if !single || !mime.starts_with("image/") => {
            Err(Error::Conflict("deliverable must be one image"))
        }
        d::DELIVERABLE_KIND_FILE if !single => Err(Error::Conflict("deliverable must be one file")),
        _ => Ok(()),
    }
}
fn after(now: &Timestamp, seconds: u64) -> Result<Timestamp> {
    let nanos = seconds
        .checked_mul(1_000_000_000)
        .ok_or(Error::Conflict("time overflow"))?;
    Ok(Timestamp::from_time(d::Time(
        now.time()?
            .0
            .checked_add(nanos)
            .ok_or(Error::Conflict("time overflow"))?,
    ))?)
}
impl<P: Persistence> Store<P> {
    /// Leest workflowcontext zonder de Job naar een nieuwere template te verplaatsen.
    pub fn workflow_for_session(&self, id: &str) -> Result<WorkflowView> {
        let p = parts(&self.state, id)?;
        let mut deliverables = List::new();
        let mut questions = List::new();
        for (_, d) in self
            .state
            .deliverables
            .iter()
            .filter(|(_, d)| d.job_id == p.job.id)
        {
            deliverables.push(d.try_clone()?)?;
        }
        for (_, q) in self
            .state
            .workflow_questions
            .iter()
            .filter(|(_, q)| q.job_id == p.job.id)
        {
            questions.push(q.try_clone()?)?;
        }
        by_time(&mut deliverables, |d| &d.created_at, false)?;
        by_time(&mut questions, |q| &q.created_at, false)?;
        Ok(WorkflowView {
            job: p.job,
            template: p.template,
            run: p.run,
            phase: p.phase,
            deliverables,
            questions,
        })
    }
    /// Een mislukte agentstart maakt dezelfde poging opnieuw uitvoerbaar.
    pub fn requeue_workflow_phase(&mut self, id: &str) -> Result<d::PhaseRun> {
        let session = self.state.sessions.get(id).ok_or(Error::NotFound)?;
        let run = self
            .state
            .phase_runs
            .get(&session.phase_run_id)
            .ok_or(Error::NotFound)?;
        if run.status != d::PHASE_RUN_RUNNING {
            return Ok(run.try_clone()?);
        }
        let id = run.id.try_clone()?;
        self.edit(|state| {
            let run = state.phase_runs.get_mut(&id).ok_or(Error::NotFound)?;
            run.status = try_string(d::PHASE_RUN_QUEUED)?;
            Ok(run.try_clone()?)
        })
    }
    /// Starten wist de pendingreden van fase en Job samen.
    pub fn mark_workflow_phase_running(
        &mut self,
        id: &str,
        now: &Timestamp,
    ) -> Result<d::PhaseRun> {
        let session = self.state.sessions.get(id).ok_or(Error::NotFound)?;
        let run = self
            .state
            .phase_runs
            .get(&session.phase_run_id)
            .ok_or(Error::NotFound)?;
        if run.status != d::PHASE_RUN_QUEUED {
            return Ok(run.try_clone()?);
        }
        let id = run.id.try_clone()?;
        self.edit(|state| {
            let run = state.phase_runs.get_mut(&id).ok_or(Error::NotFound)?;
            run.status = try_string(d::PHASE_RUN_RUNNING)?;
            run.pending_reason.clear();
            let job = state.jobs.get_mut(&run.job_id).ok_or(Error::NotFound)?;
            job.workflow_status = try_string(d::WORKFLOW_BUSY)?;
            job.pending_reason.clear();
            job.updated_at = now.try_clone()?;
            Ok(run.try_clone()?)
        })
    }
    /// Een schrijvende poging krijgt één revisie; latere edits behouden ID en opmerkingen.
    pub fn put_workflow_deliverable(
        &mut self,
        id: &str,
        name: &str,
        content: &str,
        bundle: Option<d::DeliverableBundle>,
        context: Context<'_>,
    ) -> Result<d::Deliverable> {
        context.validate()?;
        let name = normalized(name)?;
        self.edit(|state| {
            let p = parts(state, id)?;
            if p.run.status != d::PHASE_RUN_RUNNING {
                return Err(Error::Conflict("phase is not running"));
            }
            let mut definition = None;
            for candidate in p.phase.deliverables.iter() {
                if normalized(&candidate.name)? == name {
                    definition = Some(candidate);
                    break;
                }
            }
            let definition =
                definition.ok_or(Error::Conflict("deliverable is not declared by phase"))?;
            shape(definition, content.trim(), bundle.as_ref())?;
            let content = try_string(if d::deliverable_is_bundle(&definition.kind) {
                ""
            } else {
                content.trim()
            })?;
            let latest = latest_deliverable(state, &p.job.id, &definition.name)?;
            if let Some(latest) = latest.filter(|d| d.phase_run_id == p.run.id) {
                let mut latest = latest.try_clone()?;
                latest.content = content;
                latest.bundle = bundle;
                latest.kind = definition.kind.try_clone()?;
                latest.updated_at = context.now.try_clone()?;
                state
                    .deliverables
                    .insert(latest.id.try_clone()?, latest.try_clone()?)?;
                return Ok(latest);
            }
            let revision = latest.map_or(Ok(1), |old| {
                old.revision
                    .checked_add(1)
                    .ok_or(Error::Conflict("deliverable revision exhausted"))
            })?;
            if state.deliverables.get(context.id).is_some() {
                return Err(Error::Conflict("deliverable id already exists"));
            }
            let deliverable = d::Deliverable {
                id: try_string(context.id)?,
                job_id: p.job.id,
                phase_run_id: p.run.id,
                session_id: try_string(id)?,
                name: definition.name.try_clone()?,
                description: definition.description.try_clone()?,
                content,
                kind: definition.kind.try_clone()?,
                bundle,
                revision,
                created_at: context.now.try_clone()?,
                ..Default::default()
            };
            state
                .deliverables
                .insert(deliverable.id.try_clone()?, deliverable.try_clone()?)?;
            Ok(deliverable)
        })
    }
    /// Eén opgeslagen revisie, inclusief de eventuele bundelreferentie.
    pub fn deliverable(&self, id: &str) -> Result<&d::Deliverable> {
        self.state
            .deliverables
            .get(id.trim())
            .ok_or(Error::NotFound)
    }
    /// Verlengt dezelfde sharelink met één uur; intrekken wist token en deadline.
    pub fn share_deliverable(
        &mut self,
        id: &str,
        share: bool,
        context: Context<'_>,
    ) -> Result<d::Deliverable> {
        context.validate()?;
        self.edit(|state| {
            let d = state
                .deliverables
                .get_mut(id.trim())
                .ok_or(Error::NotFound)?;
            if share {
                if d.share_token.is_empty() {
                    d.share_token =
                        try_string(context.id.strip_prefix("shr_").unwrap_or(context.id))?;
                    if d.share_token.is_empty() {
                        return Err(Error::Conflict("empty share token"));
                    }
                }
                d.share_expires_at = Some(after(context.now, d::SHARE_TOKEN_TTL)?);
            } else {
                d.share_token.clear();
                d.share_expires_at = None;
            }
            Ok(d.try_clone()?)
        })
    }
    /// Alleen geldige, niet verlopen links openen een revisie; previews accepteren ook shares.
    pub fn deliverable_by_token(
        &self,
        token: &str,
        preview: bool,
        now: &Timestamp,
    ) -> Result<&d::Deliverable> {
        let token = token.trim();
        if token.is_empty() {
            return Err(Error::NotFound);
        }
        let now = now.time()?;
        for (_, d) in self.state.deliverables.iter() {
            if spin_security::constant_time_eq(d.share_token.as_bytes(), token.as_bytes())
                && d.share_expires_at
                    .as_ref()
                    .map(Timestamp::time)
                    .transpose()?
                    .is_some_and(|t| now < t)
            {
                return Ok(d);
            }
            if preview
                && spin_security::constant_time_eq(d.preview_token.as_bytes(), token.as_bytes())
                && d.preview_expires_at
                    .as_ref()
                    .map(Timestamp::time)
                    .transpose()?
                    .is_some_and(|t| now < t)
            {
                return Ok(d);
            }
        }
        Err(Error::NotFound)
    }
    /// Vernieuwt pas in het laatste derde van de preview-TTL en behoudt dezelfde URL.
    pub fn ensure_preview_token(
        &mut self,
        id: &str,
        context: Context<'_>,
    ) -> Result<d::Deliverable> {
        context.validate()?;
        let d = self.deliverable(id)?;
        let renew_before = after(context.now, d::PREVIEW_TOKEN_TTL / 3)?.time()?;
        if !d.preview_token.is_empty()
            && d.preview_expires_at
                .as_ref()
                .map(Timestamp::time)
                .transpose()?
                .is_some_and(|t| renew_before < t)
        {
            return Ok(d.try_clone()?);
        }
        self.edit(|state| {
            let d = state
                .deliverables
                .get_mut(id.trim())
                .ok_or(Error::NotFound)?;
            if d.preview_token.is_empty() {
                d.preview_token =
                    try_string(context.id.strip_prefix("pvw_").unwrap_or(context.id))?;
                if d.preview_token.is_empty() {
                    return Err(Error::Conflict("empty preview token"));
                }
            }
            d.preview_expires_at = Some(after(context.now, d::PREVIEW_TOKEN_TTL)?);
            Ok(d.try_clone()?)
        })
    }
    /// Annotaties horen bij de nieuwste revisie; bundelcommentaar geldt voor het hele bestand.
    pub fn add_deliverable_comment(
        &mut self,
        id: &str,
        author: &str,
        mut req: d::CreateDeliverableCommentRequest,
        context: Context<'_>,
    ) -> Result<d::DeliverableComment> {
        context.validate()?;
        let author = normalized(author)?;
        let body = try_string(req.body.trim())?;
        if author.is_empty() || body.is_empty() {
            return Err(Error::Conflict("author and comment are required"));
        }
        if req.selected_text.len() > 16 << 10
            || req.prefix.len() > 512
            || req.suffix.len() > 512
            || body.len() > 8 << 10
        {
            return Err(Error::Conflict("comment selection or body is too large"));
        }
        self.edit(|state| {
            let d = state.deliverables.get(id.trim()).ok_or(Error::NotFound)?;
            if d::deliverable_is_bundle(&d.kind) {
                req.selected_text.clear();
                req.start_offset = 0;
                req.end_offset = 0;
                req.prefix.clear();
                req.suffix.clear();
            } else if req.selected_text.trim().is_empty()
                || req.start_offset < 0
                || req.end_offset <= req.start_offset
            {
                return Err(Error::Conflict(
                    "document comment needs selected text with valid offsets",
                ));
            }
            if latest_deliverable(state, &d.job_id, &d.name)?
                .is_some_and(|latest| latest.id != d.id)
            {
                return Err(Error::Conflict("deliverable revision is historical"));
            }
            if state.deliverable_comments.get(context.id).is_some() {
                return Err(Error::Conflict("comment id already exists"));
            }
            let comment = d::DeliverableComment {
                id: try_string(context.id)?,
                deliverable_id: d.id.try_clone()?,
                selected_text: req.selected_text,
                start_offset: req.start_offset,
                end_offset: req.end_offset,
                prefix: req.prefix,
                suffix: req.suffix,
                body,
                author,
                created_at: context.now.try_clone()?,
            };
            state
                .deliverable_comments
                .insert(comment.id.try_clone()?, comment.try_clone()?)?;
            Ok(comment)
        })
    }
    /// Alleen een draaiende action-fase mag haar passende externe resultaat vastleggen.
    pub fn set_workflow_action_result(
        &mut self,
        id: &str,
        mut result: d::WorkflowActionResult,
        now: &Timestamp,
    ) -> Result<d::PhaseRun> {
        result.r#type = try_string(result.r#type.trim())?;
        result.external_id = try_string(result.external_id.trim())?;
        result.url = try_string(result.url.trim())?;
        result.detail = try_string(result.detail.trim())?;
        self.edit(|state| {
            let p = parts(state, id.trim())?;
            if p.phase.executor != d::WORKFLOW_EXECUTOR_ACTION
                || p.run.status != d::PHASE_RUN_RUNNING
                || p.phase
                    .action
                    .as_ref()
                    .is_none_or(|a| a.r#type != result.r#type)
                || (result.url.is_empty() && result.r#type != d::WORKFLOW_ACTION_GIT_MERGE)
            {
                return Err(Error::Conflict("phase cannot record this action result"));
            }
            if result.created_at.time()?.is_zero() {
                result.created_at = now.try_clone()?;
            }
            let run = state.phase_runs.get_mut(&p.run.id).ok_or(Error::NotFound)?;
            run.action_result = Some(result);
            Ok(run.try_clone()?)
        })
    }
    /// Nieuwe agents vervangen het workflowtoken van hun eigen Session.
    pub fn set_workflow_token(&mut self, id: &str, hash: &str) -> Result {
        self.edit(|state| {
            if state.sessions.get(id).is_none() {
                return Err(Error::NotFound);
            }
            state
                .workflow_tokens
                .insert(try_string(id)?, try_string(hash)?)?;
            Ok(())
        })
    }
    /// De HTTP-ingang vergelijkt deze hash, nooit het ruwe bearer token.
    pub fn workflow_token(&self, id: &str) -> &str {
        self.state
            .workflow_tokens
            .get(id)
            .map_or("", String::as_str)
    }
    /// Intrekken van al afwezige tokens schrijft de database niet opnieuw.
    pub fn forget_workflow_tokens(&mut self, ids: &[String]) -> Result {
        if !ids
            .iter()
            .any(|id| self.state.workflow_tokens.get(id).is_some())
        {
            return Ok(());
        }
        self.edit(|state| {
            for id in ids {
                state.workflow_tokens.remove(id);
            }
            Ok(())
        })
    }
}
