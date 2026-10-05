//! Faseovergangen zijn één Store-transactie, ook wanneer meerdere objecten ontstaan.
use super::*;
use crate::Mutation;
use spin_core::validation::text;

pub(crate) fn new_session(
    state: &PersistedState,
    job: &mut d::Job,
    template: &d::WorkflowTemplate,
    phase: &d::WorkflowPhase,
    parent: &str,
    context: &mut Mutation<'_>,
) -> Result<(d::Session, d::PhaseRun)> {
    let id = context.id("ses")?;
    let run_id = context.id("run")?;
    if state.sessions.get(&id).is_some() || state.phase_runs.get(&run_id).is_some() {
        return Err(Error::Conflict(
            "generated workflow identity already exists",
        ));
    }
    let mut attempt = 1;
    for (_, old) in state
        .phase_runs
        .iter()
        .filter(|(_, r)| r.job_id == job.id && r.phase_id == phase.id)
    {
        if old.attempt >= attempt {
            attempt = old
                .attempt
                .checked_add(1)
                .ok_or(Error::Conflict("phase attempt exhausted"))?;
        }
    }
    let worker = job.worker();
    let (mut environment, mut with) = crate::composition::phase_environment(
        state,
        worker,
        phase,
        &job.environment_selector,
        &job.with_selectors,
    )?;
    let mut tool = crate::composition::agent_tool(state, worker, &environment, &with)?;
    // Een pull request is API-werk. Alleen de merge-action heeft een workspace.
    if phase.executor == d::WORKFLOW_EXECUTOR_ACTION
        && phase
            .action
            .as_ref()
            .is_none_or(|a| a.r#type != d::WORKFLOW_ACTION_GIT_MERGE)
    {
        environment.clear();
        with = List::default();
        tool.clear();
    }
    let namespace = job.branch.strip_suffix("/main").unwrap_or(&job.branch);
    let session = d::Session {
        git_ref: text(format_args!("{namespace}/sessions/{id}"))?,
        id,
        job_id: job.id.try_clone()?,
        phase_run_id: run_id.try_clone()?,
        parent_session_id: try_string(parent)?,
        spawned_by_session_id: try_string(parent)?,
        fork_mode: try_string(d::FORK_ROOT)?,
        tool,
        executor: phase.executor.try_clone()?,
        environment_selector: environment,
        with_selectors: with,
        mcp_server_ids: job.mcp_server_ids.try_clone()?,
        role: phase.name.try_clone()?,
        model: job.model.try_clone()?,
        operator: try_string(worker)?,
        objective_delta: phase.instructions.try_clone()?,
        git_repository_id: job.git_repository_id.try_clone()?,
        base_ref: job.branch.try_clone()?,
        target_branch: job.branch.try_clone()?,
        status: try_string(d::SESSION_QUEUED)?,
        turn_ids: List::new(),
        checkpoint_ids: List::new(),
        continuity_level: try_string("workflow_phase")?,
        continuity_score: 10,
        created_at: context.now.try_clone()?,
        updated_at: context.now.try_clone()?,
        ..Default::default()
    };
    let run = d::PhaseRun {
        id: run_id,
        job_id: job.id.try_clone()?,
        template_id: template.id.try_clone()?,
        phase_id: phase.id.try_clone()?,
        phase_name: phase.name.try_clone()?,
        attempt,
        session_id: session.id.try_clone()?,
        status: try_string(d::PHASE_RUN_QUEUED)?,
        started_at: context.now.try_clone()?,
        ..Default::default()
    };
    job.session_ids.push(session.id.try_clone()?)?;
    job.phase_run_ids.push(run.id.try_clone()?)?;
    job.current_phase_run_id = run.id.try_clone()?;
    job.status = try_string(d::JOB_ACTIVE)?;
    job.workflow_status = try_string(d::WORKFLOW_BUSY)?;
    job.pending_reason.clear();
    job.updated_at = context.now.try_clone()?;
    Ok((session, run))
}
fn target<'a>(template: &'a d::WorkflowTemplate, current: &'a str, raw: &'a str) -> Result<String> {
    let raw = raw.trim();
    let result = if raw.is_empty() || raw.eq_ignore_ascii_case(d::WORKFLOW_TARGET_NEXT) {
        template
            .phases
            .iter()
            .position(|p| p.id == current)
            .and_then(|i| template.phases.get(i + 1))
            .map_or(d::WORKFLOW_TARGET_DONE, |p| p.id.as_str())
    } else if raw.eq_ignore_ascii_case(d::WORKFLOW_TARGET_SELF) {
        current
    } else if raw.eq_ignore_ascii_case(d::WORKFLOW_TARGET_DONE) {
        d::WORKFLOW_TARGET_DONE
    } else if raw.eq_ignore_ascii_case(d::WORKFLOW_TARGET_ASK_USER) {
        d::WORKFLOW_TARGET_ASK_USER
    } else {
        return Ok(normalized(raw)?);
    };
    Ok(try_string(result)?)
}
fn human_target(
    template: &d::WorkflowTemplate,
    current: &str,
    raw: &str,
    fallback: &str,
) -> Result<String> {
    let value = target(template, current, raw)?;
    if value == d::WORKFLOW_TARGET_ASK_USER {
        target(template, current, fallback)
    } else {
        Ok(value)
    }
}
fn rejections(state: &PersistedState, job: &str, phase: &str) -> Result<i64> {
    let mut count = 0_i64;
    for (_, r) in state
        .phase_runs
        .iter()
        .filter(|(_, r)| r.job_id == job && r.phase_id == phase)
    {
        let n = if r.agent_outcomes.is_empty() {
            usize::from(r.status == d::PHASE_RUN_REJECTED)
        } else {
            r.agent_outcomes
                .iter()
                .filter(|o| o.outcome == "reject")
                .count()
        };
        count = count
            .checked_add(i64::try_from(n).map_err(|_| Error::Conflict("rejection count overflow"))?)
            .ok_or(Error::Conflict("rejection count overflow"))?;
    }
    Ok(count)
}
fn transition(
    state: &PersistedState,
    p: &Parts,
    outcome: &str,
) -> Result<(d::WorkflowTransition, i64)> {
    match outcome {
        "accept" => Ok((p.phase.accept.try_clone()?, 0)),
        "reject" => {
            let count = rejections(state, &p.job.id, &p.phase.id)?
                .checked_add(1)
                .ok_or(Error::Conflict("rejection count overflow"))?;
            let mut t = p.phase.reject.try_clone()?;
            if t.max > 0 && count >= t.max {
                t.target = if t.exhausted.is_empty() {
                    try_string(d::WORKFLOW_TARGET_ASK_USER)?
                } else {
                    t.exhausted.try_clone()?
                };
            }
            Ok((t, count))
        }
        _ => Err(Error::Conflict("outcome must be accept or reject")),
    }
}
fn injection(
    state: &PersistedState,
    job: &str,
    template: &d::WorkflowTemplate,
    current: &str,
    raw: &str,
) -> Result {
    let next = target(template, current, raw)?;
    if next == d::WORKFLOW_TARGET_DONE || next == d::WORKFLOW_TARGET_ASK_USER {
        return Ok(());
    }
    let phase = phase(template, &next)?;
    for name in phase.inject.iter() {
        if latest_deliverable(state, job, name)?.is_none() {
            return Err(Error::Conflict("next phase requires a missing deliverable"));
        }
    }
    Ok(())
}
fn required(state: &PersistedState, p: &Parts) -> Result {
    for item in p.phase.deliverables.iter().filter(|i| i.required) {
        let mut found = false;
        for (_, d) in state
            .deliverables
            .iter()
            .filter(|(_, d)| d.phase_run_id == p.run.id)
        {
            found |= normalized(&d.name)? == normalized(&item.name)?;
        }
        if !found {
            return Err(Error::Conflict("required deliverable is missing"));
        }
    }
    Ok(())
}
fn supersede(state: &mut PersistedState, run: &str, now: &Timestamp) -> Result {
    for (_, q) in state
        .workflow_questions
        .iter_mut()
        .filter(|(_, q)| q.phase_run_id == run && q.status == "open")
    {
        q.status = try_string("superseded")?;
        q.answered_at = Some(now.try_clone()?);
    }
    Ok(())
}
fn open_question<'a>(state: &'a PersistedState, run: &str) -> Option<&'a d::WorkflowQuestion> {
    state
        .workflow_questions
        .iter()
        .map(|(_, q)| q)
        .find(|q| q.phase_run_id == run && q.status == "open")
}
fn save_parts(state: &mut PersistedState, job: &d::Job, run: &d::PhaseRun) -> Result {
    state.jobs.insert(job.id.try_clone()?, job.try_clone()?)?;
    state
        .phase_runs
        .insert(run.id.try_clone()?, run.try_clone()?)?;
    Ok(())
}
fn await_decision(
    state: &mut PersistedState,
    mut p: Parts,
    outcome: &str,
    count: i64,
    context: &mut Mutation<'_>,
) -> Result<d::WorkflowAdvance> {
    let mut message = if p.phase.executor == d::WORKFLOW_EXECUTOR_EXPOSE {
        text(format_args!(
            "De test-app van {} staat klaar.",
            p.phase.name
        ))?
    } else {
        text(format_args!("AI accepted {}.", p.phase.name))?
    };
    if !p.run.summary.is_empty() {
        d::try_push_str(&mut message, " ")?;
        d::try_push_str(&mut message, &p.run.summary)?;
    }
    if outcome == "reject" {
        message = text(format_args!(
            "AI rejected {} {} keer. {}",
            p.phase.name,
            count.max(1),
            p.run.reject_reason
        ))?;
    }
    let mut kind = "approval";
    let mut accept = human_target(
        &p.template,
        &p.phase.id,
        &p.phase.accept.target,
        d::WORKFLOW_TARGET_NEXT,
    )?;
    let mut reject = human_target(
        &p.template,
        &p.phase.id,
        &p.phase.reject.target,
        d::WORKFLOW_TARGET_SELF,
    )?;
    // Een mislukte action kan niet goedgekeurd worden alsof zij geslaagd is.
    // ACCEPT probeert dezelfde action opnieuw, REJECT volgt de herstelroute.
    if p.phase.executor == d::WORKFLOW_EXECUTOR_ACTION && outcome == "reject" {
        kind = "action";
        accept = p.phase.id.try_clone()?;
        if reject.is_empty() || reject == d::WORKFLOW_TARGET_DONE {
            reject = p.phase.id.try_clone()?;
        }
    }
    let id = context.id("ask")?;
    if state.workflow_questions.get(&id).is_some() {
        return Err(Error::Conflict("question id already exists"));
    }
    let question = d::WorkflowQuestion {
        id,
        job_id: p.job.id.try_clone()?,
        phase_run_id: p.run.id.try_clone()?,
        session_id: p.run.session_id.try_clone()?,
        kind: try_string(kind)?,
        question: try_string(message.trim())?,
        outcome: try_string(outcome)?,
        agent_detail: if outcome == "reject" {
            p.run.reject_reason.try_clone()?
        } else {
            p.run.summary.try_clone()?
        },
        agent_outcome_id: try_string(p.run.agent_outcomes.last().map_or("", |o| o.id.as_str()))?,
        accept_target: accept,
        reject_target: reject,
        status: try_string("open")?,
        created_at: context.now.try_clone()?,
        ..Default::default()
    };
    p.run.status = try_string(d::PHASE_RUN_PENDING)?;
    p.run.pending_reason = try_string("user")?;
    p.run.pending_outcome = try_string(outcome)?;
    p.run.completed_at = None;
    p.job.workflow_status = try_string(d::WORKFLOW_PENDING)?;
    p.job.pending_reason = try_string("user")?;
    p.job.updated_at = context.now.try_clone()?;
    state
        .workflow_questions
        .insert(question.id.try_clone()?, question.try_clone()?)?;
    save_parts(state, &p.job, &p.run)?;
    Ok(d::WorkflowAdvance {
        job: p.job,
        phase_run: p.run,
        question: Some(question),
        next_session: None,
    })
}
fn advance(
    state: &mut PersistedState,
    mut p: Parts,
    raw: &str,
    outcome: &str,
    context: &mut Mutation<'_>,
) -> Result<d::WorkflowAdvance> {
    let next = target(&p.template, &p.phase.id, raw)?;
    if next == d::WORKFLOW_TARGET_ASK_USER {
        let count = rejections(state, &p.job.id, &p.phase.id)?;
        return await_decision(state, p, outcome, count, context);
    }
    if next == d::WORKFLOW_TARGET_DONE {
        p.job.status = try_string(d::JOB_DONE)?;
        p.job.workflow_status = try_string(d::WORKFLOW_DONE)?;
        p.job.pending_reason.clear();
        p.job.current_phase_run_id.clear();
        p.job.updated_at = context.now.try_clone()?;
        save_parts(state, &p.job, &p.run)?;
        return Ok(d::WorkflowAdvance {
            job: p.job,
            phase_run: p.run,
            ..Default::default()
        });
    }
    let phase = phase(&p.template, &next)?;
    let (session, run) = new_session(
        state,
        &mut p.job,
        &p.template,
        &phase,
        &p.run.session_id,
        context,
    )?;
    state
        .sessions
        .insert(session.id.try_clone()?, session.try_clone()?)?;
    save_parts(state, &p.job, &run)?;
    Ok(d::WorkflowAdvance {
        job: p.job,
        phase_run: run,
        next_session: Some(session),
        question: None,
    })
}
fn decision_parts(state: &PersistedState, id: &str) -> Result<(d::WorkflowQuestion, Parts)> {
    let q = state
        .workflow_questions
        .get(id)
        .filter(|q| q.status == "open")
        .ok_or(Error::NotFound)?;
    let p = parts(state, &q.session_id)?;
    if q.phase_run_id != p.run.id || q.job_id != p.job.id {
        return Err(Error::Conflict(
            "question does not belong to current session phase",
        ));
    }
    Ok((q.try_clone()?, p))
}
fn decision_target(q: &d::WorkflowQuestion, p: &Parts, action: &str) -> Result<String> {
    let (value, raw, fallback) = match action {
        "accept" => (
            &q.accept_target,
            &p.phase.accept.target,
            d::WORKFLOW_TARGET_NEXT,
        ),
        "reject" => (
            &q.reject_target,
            &p.phase.reject.target,
            d::WORKFLOW_TARGET_SELF,
        ),
        _ => return Err(Error::Conflict("action must be accept or reject")),
    };
    if value.is_empty() {
        human_target(&p.template, &p.phase.id, raw, fallback)
    } else {
        Ok(value.try_clone()?)
    }
}
impl<P: Persistence> Store<P> {
    /// Controleert invoer voor een directe faseovergang vóór extern werk begint.
    pub fn validate_workflow_phase_transition(&self, id: &str, outcome: &str) -> Result {
        let p = parts(&self.state, id)?;
        let (t, _) = transition(&self.state, &p, &normalized(outcome)?)?;
        if t.ask_user
            || p.phase.executor == d::WORKFLOW_EXECUTOR_EXPOSE
            || target(&p.template, &p.phase.id, &t.target)? == d::WORKFLOW_TARGET_ASK_USER
        {
            return Ok(());
        }
        injection(&self.state, &p.job.id, &p.template, &p.phase.id, &t.target)
    }
    /// Een menselijke beslissing valideert de concrete doelstap, inclusief injecties.
    pub fn validate_workflow_question_transition(&self, id: &str, action: &str) -> Result {
        let (q, p) = decision_parts(&self.state, id)?;
        let next = decision_target(&q, &p, &normalized(action)?)?;
        injection(&self.state, &p.job.id, &p.template, &p.phase.id, &next)
    }
    /// Legt het agentoordeel vast; infrastructuurfouten vragen altijd menselijke keuze.
    pub fn complete_workflow_phase(
        &mut self,
        id: &str,
        outcome: &str,
        detail: &str,
        always_ask: bool,
        mut context: Mutation<'_>,
    ) -> Result<d::WorkflowAdvance> {
        let outcome = normalized(outcome)?;
        let detail = detail.trim();
        if outcome == "reject" && detail.is_empty() {
            return Err(Error::Conflict("reject requires a reason"));
        }
        self.edit(|state| {
            let mut p = parts(state, id)?;
            if p.run.status != d::PHASE_RUN_RUNNING {
                return Err(Error::Conflict("phase is not running"));
            }
            let (t, count) = transition(state, &p, &outcome)?;
            let ask = always_ask
                || t.ask_user
                || p.phase.executor == d::WORKFLOW_EXECUTOR_EXPOSE
                || target(&p.template, &p.phase.id, &t.target)? == d::WORKFLOW_TARGET_ASK_USER;
            if !ask {
                injection(state, &p.job.id, &p.template, &p.phase.id, &t.target)?;
            }
            if outcome == "accept" {
                required(state, &p)?;
            }
            supersede(state, &p.run.id, context.now)?;
            let outcome_id = context.id("out")?;
            if state
                .phase_runs
                .iter()
                .any(|(_, r)| r.agent_outcomes.iter().any(|o| o.id == outcome_id))
            {
                return Err(Error::Conflict("outcome id already exists"));
            }
            p.run.agent_outcomes.push(d::WorkflowAgentOutcome {
                id: outcome_id,
                outcome: outcome.try_clone()?,
                detail: try_string(detail)?,
                created_at: context.now.try_clone()?,
            })?;
            if outcome == "accept" {
                p.run.status = try_string(d::PHASE_RUN_ACCEPTED)?;
                p.run.summary = try_string(detail)?;
            } else {
                p.run.status = try_string(d::PHASE_RUN_REJECTED)?;
                p.run.reject_reason = try_string(detail)?;
            }
            p.run.completed_at = Some(context.now.try_clone()?);
            state
                .phase_runs
                .insert(p.run.id.try_clone()?, p.run.try_clone()?)?;
            if ask {
                await_decision(state, p, &outcome, count, &mut context)
            } else {
                advance(state, p, &t.target, &outcome, &mut context)
            }
        })
    }
    /// Beantwoordt precies één nog open beslissing; dubbel uitvoeren is niet mogelijk.
    pub fn answer_workflow_question(
        &mut self,
        id: &str,
        operator: &str,
        action: &str,
        reason: &str,
        mut context: Mutation<'_>,
    ) -> Result<d::WorkflowAdvance> {
        let operator = normalized(operator)?;
        let action = normalized(action)?;
        let reason = reason.trim();
        if operator.is_empty()
            || (action != "accept" && action != "reject")
            || (action == "reject" && reason.is_empty())
        {
            return Err(Error::Conflict(
                "operator and accept/reject with a rejection reason are required",
            ));
        }
        self.edit(|state| {
            let (mut q, mut p) = decision_parts(state, id)?;
            if p.run.status != d::PHASE_RUN_PENDING {
                return Err(Error::Conflict("phase is not pending"));
            }
            if action == "accept" {
                required(state, &p)?;
            }
            let next = decision_target(&q, &p, &action)?;
            injection(state, &p.job.id, &p.template, &p.phase.id, &next)?;
            q.answer = action.try_clone()?;
            q.reason = try_string(reason)?;
            q.answered_by = operator;
            q.status = try_string("answered")?;
            q.answered_at = Some(context.now.try_clone()?);
            p.run.status = try_string(if action == "accept" {
                d::PHASE_RUN_ACCEPTED
            } else {
                d::PHASE_RUN_REJECTED
            })?;
            if p.run.summary.is_empty() {
                p.run.summary = try_string("Accepted by user")?;
            }
            if action == "reject" {
                p.run.reject_reason = try_string(reason)?;
            }
            p.run.pending_reason.clear();
            p.run.pending_outcome.clear();
            p.run.completed_at = Some(context.now.try_clone()?);
            state
                .workflow_questions
                .insert(q.id.try_clone()?, q.try_clone()?)?;
            state
                .phase_runs
                .insert(p.run.id.try_clone()?, p.run.try_clone()?)?;
            let mut result = advance(state, p, &next, &action, &mut context)?;
            result.question = Some(q);
            Ok(result)
        })
    }
}

fn reset_question(
    q: &mut d::WorkflowQuestion,
    operator: &str,
    reason: &str,
    now: &Timestamp,
) -> Result {
    q.status = try_string("answered")?;
    q.answer = try_string("retry")?;
    q.reason = try_string(reason)?;
    q.answered_by = try_string(operator)?;
    q.answered_at = Some(now.try_clone()?);
    Ok(())
}
fn pending(p: &mut Parts, reason: &str, outcome: &str, now: &Timestamp) -> Result {
    p.run.status = try_string(d::PHASE_RUN_PENDING)?;
    p.run.pending_reason = try_string(reason)?;
    p.run.pending_outcome = try_string(outcome)?;
    p.job.workflow_status = try_string(d::WORKFLOW_PENDING)?;
    p.job.pending_reason = try_string(reason)?;
    p.job.updated_at = now.try_clone()?;
    Ok(())
}
fn resume(p: &mut Parts, now: &Timestamp) -> Result {
    p.run.status = try_string(d::PHASE_RUN_RUNNING)?;
    p.run.pending_reason.clear();
    p.run.completed_at = None;
    p.job.workflow_status = try_string(d::WORKFLOW_BUSY)?;
    p.job.pending_reason.clear();
    p.job.updated_at = now.try_clone()?;
    Ok(())
}
fn restore_decision(
    state: &mut PersistedState,
    id: &str,
    context: &mut Mutation<'_>,
) -> Result<bool> {
    let p = parts(state, id)?;
    let Some(last) = p.run.agent_outcomes.last() else {
        return Ok(false);
    };
    let outcome = last.outcome.try_clone()?;
    let count = if outcome == "reject" {
        rejections(state, &p.job.id, &p.phase.id)?
    } else {
        0
    };
    await_decision(state, p, &outcome, count, context)?;
    Ok(true)
}
impl<P: Persistence> Store<P> {
    /// Zet een stap opzij waarvan de agent niet wil starten: pending met de reden,
    /// tot iemand hem opnieuw start (retry). Alleen de huidige, lopende of wachtende poging.
    pub fn park_workflow_phase(
        &mut self,
        id: &str,
        reason: &str,
        now: &Timestamp,
    ) -> Result<d::PhaseRun> {
        let reason = reason.trim();
        if reason.is_empty() || reason.len() > 4000 {
            return Err(Error::Conflict("a reason of 1 to 4000 bytes is required"));
        }
        self.edit(|state| {
            let mut p = parts(state, id)?;
            if p.job.current_phase_run_id != p.run.id
                || !matches!(
                    p.run.status.as_str(),
                    d::PHASE_RUN_QUEUED | d::PHASE_RUN_RUNNING
                )
            {
                return Ok(p.run);
            }
            pending(&mut p, "agent", reason, now)?;
            p.run.completed_at = None;
            save_parts(state, &p.job, &p.run)?;
            Ok(p.run)
        })
    }
    /// Pauzeert de agent met één begrensd formulier; de gebruiker mag vrij antwoorden.
    pub fn ask_workflow_questions(
        &mut self,
        id: &str,
        items: &[d::WorkflowQuestionItem],
        context: Context<'_>,
    ) -> Result<d::WorkflowQuestion> {
        context.validate()?;
        if items.is_empty() || items.len() > 6 {
            return Err(Error::Conflict(
                "between one and six questions are required",
            ));
        }
        let mut cleaned = List::new();
        let mut headline = String::new();
        for (index, item) in items.iter().enumerate() {
            let question = item.question.trim();
            if question.is_empty() || question.len() > 4000 || item.options.len() > 8 {
                return Err(Error::Conflict("question or options exceed their bounds"));
            }
            let mut options = List::new();
            for option in item.options.iter() {
                let option = option.trim();
                if option.is_empty() || option.len() > 400 {
                    return Err(Error::Conflict("option must contain 1 to 400 bytes"));
                }
                if !options.iter().any(|v| v == option) {
                    options.push(try_string(option)?)?;
                }
            }
            if index > 0 {
                d::try_push_str(&mut headline, " · ")?;
            }
            d::try_push_str(&mut headline, question)?;
            cleaned.push(d::WorkflowQuestionItem {
                id: text(format_args!("q{}", index + 1))?,
                question: try_string(question)?,
                options,
                ..Default::default()
            })?;
        }
        self.edit(|state| {
            let mut p = parts(state, id)?;
            if p.run.status != d::PHASE_RUN_RUNNING {
                return Err(Error::Conflict("phase is not running"));
            }
            if state.workflow_questions.get(context.id).is_some() {
                return Err(Error::Conflict("question id already exists"));
            }
            supersede(state, &p.run.id, context.now)?;
            let question = d::WorkflowQuestion {
                id: try_string(context.id)?,
                job_id: p.job.id.try_clone()?,
                phase_run_id: p.run.id.try_clone()?,
                session_id: try_string(id)?,
                kind: try_string("agent")?,
                question: headline,
                items: cleaned,
                outcome: try_string("ask")?,
                accept_target: human_target(
                    &p.template,
                    &p.phase.id,
                    &p.phase.accept.target,
                    d::WORKFLOW_TARGET_NEXT,
                )?,
                reject_target: human_target(
                    &p.template,
                    &p.phase.id,
                    &p.phase.reject.target,
                    d::WORKFLOW_TARGET_SELF,
                )?,
                status: try_string("open")?,
                created_at: context.now.try_clone()?,
                ..Default::default()
            };
            pending(&mut p, "ask", "ask", context.now)?;
            state
                .workflow_questions
                .insert(question.id.try_clone()?, question.try_clone()?)?;
            save_parts(state, &p.job, &p.run)?;
            Ok(question)
        })
    }
    /// Een ingevuld formulier hervat dezelfde Session en kiest geen volgende fase.
    pub fn answer_workflow_questions(
        &mut self,
        id: &str,
        operator: &str,
        answers: &[d::WorkflowQuestionAnswer],
        now: &Timestamp,
    ) -> Result<d::WorkflowQuestion> {
        let operator = normalized(operator)?;
        if operator.is_empty() {
            return Err(Error::Conflict("operator is required"));
        }
        self.edit(|state| {
            let (mut q, mut p) = decision_parts(state, id)?;
            if q.kind != "agent" || q.items.is_empty() || p.run.status != d::PHASE_RUN_PENDING {
                return Err(Error::Conflict("question is not an open agent form"));
            }
            for item in q.items.as_mut_slice() {
                let answer = answers
                    .iter()
                    .rev()
                    .find(|a| a.item_id.trim() == item.id)
                    .map_or("", |a| a.answer.trim());
                if answer.is_empty() || answer.len() > 4000 {
                    return Err(Error::Conflict(
                        "every question needs an answer of 1 to 4000 bytes",
                    ));
                }
                item.other = !item.options.iter().any(|v| v == answer);
                item.answer = try_string(answer)?;
            }
            q.answer = try_string("answered")?;
            q.reason.clear();
            q.answered_by = operator;
            q.status = try_string("answered")?;
            q.answered_at = Some(now.try_clone()?);
            resume(&mut p, now)?;
            p.run.pending_outcome.clear();
            state
                .workflow_questions
                .insert(q.id.try_clone()?, q.try_clone()?)?;
            save_parts(state, &p.job, &p.run)?;
            Ok(q)
        })
    }
    /// Een chat is geen oordeel: de bestaande beslissing blijft open tijdens de beurt.
    pub fn resume_workflow_phase_for_chat(
        &mut self,
        id: &str,
        operator: &str,
        now: &Timestamp,
    ) -> Result<bool> {
        let Some(session) = self
            .state
            .sessions
            .get(id)
            .filter(|s| !s.phase_run_id.is_empty())
        else {
            return Ok(false);
        };
        let operator = normalized(operator)?;
        if operator.is_empty() || session.operator != operator {
            return Err(Error::Conflict("Session belongs to another operator"));
        }
        if self
            .state
            .phase_runs
            .get(&session.phase_run_id)
            .is_none_or(|r| r.status != d::PHASE_RUN_PENDING)
            || open_question(&self.state, &session.phase_run_id).is_none()
        {
            return Ok(false);
        }
        self.edit(|state| {
            let mut p = parts(state, id)?;
            resume(&mut p, now)?;
            save_parts(state, &p.job, &p.run)?;
            Ok(true)
        })
    }
    /// Na de chat wordt dezelfde beslissing weer zichtbaar; oud gedrag wordt hersteld.
    pub fn settle_workflow_chat_turn(
        &mut self,
        id: &str,
        mut context: Mutation<'_>,
    ) -> Result<bool> {
        let Some(session) = self
            .state
            .sessions
            .get(id)
            .filter(|s| !s.phase_run_id.is_empty())
        else {
            return Ok(false);
        };
        let Some(run) = self
            .state
            .phase_runs
            .get(&session.phase_run_id)
            .filter(|r| r.status == d::PHASE_RUN_RUNNING)
        else {
            return Ok(false);
        };
        if open_question(&self.state, &run.id).is_none() && run.agent_outcomes.is_empty() {
            return Ok(false);
        }
        self.edit(|state| {
            let mut p = parts(state, id)?;
            let Some(q) = open_question(state, &p.run.id) else {
                return restore_decision(state, id, &mut context);
            };
            let reason = if q.kind == "agent" { "ask" } else { "user" };
            pending(&mut p, reason, &q.outcome, context.now)?;
            save_parts(state, &p.job, &p.run)?;
            Ok(true)
        })
    }
    /// Herstelt besluiten van oudere builds, behalve bij nog actieve agentbeurten.
    pub fn repair_standing_decisions(&mut self, mut context: Mutation<'_>) -> Result<usize> {
        let mut ids = List::new();
        for (_, session) in self
            .state
            .sessions
            .iter()
            .filter(|(_, s)| !s.phase_run_id.is_empty())
        {
            if self
                .state
                .compositions
                .get(&session.prepared_composition_id)
                .is_some_and(|c| {
                    c.runtime.as_ref().is_some_and(|r| r.status != "stopped")
                        && c.agent.as_ref().is_some_and(|a| !a.prompt_id.is_empty())
                })
            {
                continue;
            }
            let Some(run) = self
                .state
                .phase_runs
                .get(&session.phase_run_id)
                .filter(|r| r.status == d::PHASE_RUN_RUNNING && !r.agent_outcomes.is_empty())
            else {
                continue;
            };
            if open_question(&self.state, &run.id).is_none() {
                ids.push(session.id.try_clone()?)?;
            }
        }
        if ids.is_empty() {
            return Ok(0);
        }
        self.edit(|state| {
            let mut restored = 0;
            for id in ids.iter() {
                if restore_decision(state, id, &mut context)? {
                    restored += 1;
                }
            }
            Ok(restored)
        })
    }
    /// Herstart dezelfde poging; de runtime ruimt de teruggegeven oude compositie op.
    pub fn retry_workflow_session(
        &mut self,
        id: &str,
        operator: &str,
        note: &str,
        transcript: &[d::ChatLine],
        now: &Timestamp,
    ) -> Result<(d::CreateJobResponse, String)> {
        let operator = normalized(operator)?;
        let note = note.trim();
        if note.len() > 4000 {
            return Err(Error::Conflict("restart note exceeds 4000 bytes"));
        }
        let mut kept = List::new();
        for line in transcript.iter().skip(transcript.len().saturating_sub(60)) {
            let value = line.text.trim();
            if value.is_empty() {
                continue;
            }
            // De Go-grens is 4000 bytes; knip UTF-8 uitsluitend tussen codepunten.
            let mut end = value.len().min(4000);
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            let mut value_out = try_string(&value[..end])?;
            if end < value.len() {
                d::try_push_str(&mut value_out, "…")?;
            }
            kept.push(d::ChatLine {
                role: try_string(if line.role == "user" { "user" } else { "agent" })?,
                text: value_out,
            })?;
        }
        self.edit(|state| {
            let mut p = parts(state, id.trim())?;
            if !p.job.allows_operator(&operator)
                || p.job.current_phase_run_id != p.run.id
                || matches!(p.job.status.as_str(), d::JOB_DONE | d::JOB_CANCELLED)
                || !matches!(
                    p.run.status.as_str(),
                    d::PHASE_RUN_QUEUED | d::PHASE_RUN_RUNNING | d::PHASE_RUN_PENDING
                )
            {
                return Err(Error::Conflict(
                    "only an active workflow Session can be retried by its owner or assignee",
                ));
            }
            for (_, q) in state
                .workflow_questions
                .iter_mut()
                .filter(|(_, q)| q.phase_run_id == p.run.id && q.status == "open")
            {
                reset_question(q, &operator, "Session retried by user", now)?;
            }
            p.run.status = try_string(d::PHASE_RUN_QUEUED)?;
            p.run.pending_reason.clear();
            p.run.pending_outcome.clear();
            p.run.summary.clear();
            p.run.reject_reason.clear();
            p.run.completed_at = None;
            p.run.restarts = p
                .run
                .restarts
                .checked_add(1)
                .ok_or(Error::Conflict("restart count exhausted"))?;
            if !note.is_empty() {
                p.run.restart_notes.push(try_string(note)?)?;
            }
            p.run.restart_transcript = kept;
            p.job.status = try_string(d::JOB_ACTIVE)?;
            p.job.workflow_status = try_string(d::WORKFLOW_BUSY)?;
            p.job.pending_reason.clear();
            p.job.updated_at = now.try_clone()?;
            let session = state.sessions.get_mut(id.trim()).ok_or(Error::NotFound)?;
            let previous = core::mem::take(&mut session.prepared_composition_id);
            session.status = try_string(d::SESSION_QUEUED)?;
            session.client_id.clear();
            session.activation_id.clear();
            session.lease_expires_at = None;
            session.base_ref = p.job.branch.try_clone()?;
            session.target_branch = p.job.branch.try_clone()?;
            session.updated_at = now.try_clone()?;
            let session = session.try_clone()?;
            save_parts(state, &p.job, &p.run)?;
            Ok((
                d::CreateJobResponse {
                    job: p.job,
                    session,
                    replayed: false,
                    ..Default::default()
                },
                previous,
            ))
        })
    }
    /// Sluit de brainstorm af met het gekozen doel en start de eerste templatestap.
    pub fn start_process(
        &mut self,
        id: &str,
        goal: &str,
        mut context: Mutation<'_>,
    ) -> Result<(d::CreateJobResponse, String)> {
        let goal = goal.trim();
        if goal.is_empty() {
            return Err(Error::Conflict("a goal is required"));
        }
        self.edit(|state| {
            let mut p = parts(state, id.trim())?;
            if p.run.phase_id != d::BRAINSTORM_PHASE_ID
                || p.run.status != d::PHASE_RUN_RUNNING
                || p.job.current_phase_run_id != p.run.id
            {
                return Err(Error::Conflict(
                    "only the running brainstorm can start the process",
                ));
            }
            let first = p
                .template
                .phases
                .first()
                .ok_or(Error::Conflict("template has no phases"))?;
            p.run.status = try_string(d::PHASE_RUN_ACCEPTED)?;
            p.run.summary = try_string(goal)?;
            p.run.completed_at = Some(context.now.try_clone()?);
            state
                .phase_runs
                .insert(p.run.id.try_clone()?, p.run.try_clone()?)?;
            let session = state.sessions.get_mut(id.trim()).ok_or(Error::NotFound)?;
            session.status = try_string(d::SESSION_COMPLETED)?;
            session.updated_at = context.now.try_clone()?;
            let previous = session.prepared_composition_id.try_clone()?;
            p.job.objective = try_string(goal)?;
            let (session, run) = new_session(
                state,
                &mut p.job,
                &p.template,
                first,
                id.trim(),
                &mut context,
            )?;
            state
                .sessions
                .insert(session.id.try_clone()?, session.try_clone()?)?;
            save_parts(state, &p.job, &run)?;
            Ok((
                d::CreateJobResponse {
                    job: p.job,
                    session,
                    replayed: false,
                    ..Default::default()
                },
                previous,
            ))
        })
    }
    /// Neemt bewust een nieuwere template over en sluit de oude poging zonder oordeel.
    pub fn adopt_workflow_template(
        &mut self,
        id: &str,
        operator: &str,
        phase_id: &str,
        mut context: Mutation<'_>,
    ) -> Result<(d::CreateJobResponse, String)> {
        let operator = normalized(operator)?;
        let phase_id = normalized(phase_id)?;
        if phase_id.is_empty() {
            return Err(Error::Conflict("choose the step to continue at"));
        }
        self.edit(|state| {
            let mut job = state
                .jobs
                .get(id.trim())
                .ok_or(Error::NotFound)?
                .try_clone()?;
            if !job.allows_operator(&operator)
                || matches!(job.status.as_str(), d::JOB_DONE | d::JOB_CANCELLED)
                || job.template_id.is_empty()
            {
                return Err(Error::Conflict(
                    "only an active workflow Job may adopt a template",
                ));
            }
            let template = state
                .workflow_templates
                .get(&job.template_id)
                .ok_or(Error::NotFound)?
                .try_clone()?;
            let phase = phase(&template, &phase_id)?;
            let mut previous = String::new();
            let mut parent = String::new();
            if let Some(run) = state.phase_runs.get_mut(&job.current_phase_run_id) {
                let reason = text(format_args!(
                    "Job overgezet naar stap {} (Template r{})",
                    phase.name, template.revision
                ))?;
                for (_, q) in state
                    .workflow_questions
                    .iter_mut()
                    .filter(|(_, q)| q.phase_run_id == run.id && q.status == "open")
                {
                    reset_question(q, &operator, &reason, context.now)?;
                }
                if matches!(
                    run.status.as_str(),
                    d::PHASE_RUN_QUEUED | d::PHASE_RUN_RUNNING | d::PHASE_RUN_PENDING
                ) {
                    run.status = try_string(d::PHASE_RUN_ACCEPTED)?;
                    run.summary = text(format_args!(
                        "Overgezet naar stap {} (Template r{}) door {}",
                        phase.name, template.revision, operator
                    ))?;
                    run.pending_reason.clear();
                    run.pending_outcome.clear();
                    run.completed_at = Some(context.now.try_clone()?);
                }
                parent = run.session_id.try_clone()?;
                if let Some(session) = state.sessions.get_mut(&parent) {
                    previous = session.prepared_composition_id.try_clone()?;
                    if matches!(
                        session.status.as_str(),
                        d::SESSION_QUEUED | d::SESSION_RUNNING
                    ) {
                        session.status = try_string(d::SESSION_COMPLETED)?;
                        session.updated_at = context.now.try_clone()?;
                    }
                }
            }
            job.template_snapshot = Some(template.try_clone()?);
            let (session, run) =
                new_session(state, &mut job, &template, &phase, &parent, &mut context)?;
            state
                .sessions
                .insert(session.id.try_clone()?, session.try_clone()?)?;
            save_parts(state, &job, &run)?;
            Ok((
                d::CreateJobResponse {
                    job,
                    session,
                    replayed: false,
                    ..Default::default()
                },
                previous,
            ))
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
    fn fixture<'a>(fail: &'a Cell<bool>, action: bool) -> (Store<Memory<'a>>, Timestamp) {
        let now = Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap();
        let template = d::WorkflowTemplate::from_json(br#"{"id":"tpl","revision":1,"phases":[{"id":"build","name":"Build","executor":"agent","accept":{"target":"NEXT","ask_user":true},"reject":{"target":"SELF","max":2},"deliverables":[{"name":"plan","required":true}]},{"id":"review","name":"Review","executor":"agent","inject":["plan"],"accept":{"target":"DONE"},"reject":{"target":"build"}}]}"#).unwrap();
        let mut state = PersistedState::default();
        let mut job = d::Job::from_json(br#"{"id":"job_1","owner":"derek","assignee":"derek","branch":"jobs/test/main","template_id":"tpl","status":"active","current_phase_run_id":"run_0","session_ids":["ses_0"],"phase_run_ids":["run_0"]}"#).unwrap();
        job.template_snapshot = Some(template.try_clone().unwrap());
        if action {
            let phase = &mut job
                .template_snapshot
                .as_mut()
                .unwrap()
                .phases
                .as_mut_slice()[0];
            phase.executor = try_string(d::WORKFLOW_EXECUTOR_ACTION).unwrap();
            phase.deliverables = List::new();
            phase.reject.target = try_string(d::WORKFLOW_TARGET_DONE).unwrap();
            phase.reject.ask_user = true;
        }
        state.jobs.insert(job.id.try_clone().unwrap(), job).unwrap();
        state
            .workflow_templates
            .insert(try_string("tpl").unwrap(), template)
            .unwrap();
        let s = d::Session::from_json(br#"{"id":"ses_0","job_id":"job_1","phase_run_id":"run_0","operator":"derek","status":"running","prepared_composition_id":"cmp_0","activation_id":"act_old","client_id":"client_old"}"#).unwrap();
        state.sessions.insert(s.id.try_clone().unwrap(), s).unwrap();
        let r = d::PhaseRun::from_json(br#"{"id":"run_0","job_id":"job_1","template_id":"tpl","phase_id":"build","phase_name":"Build","attempt":1,"session_id":"ses_0","status":"running"}"#).unwrap();
        state
            .phase_runs
            .insert(r.id.try_clone().unwrap(), r)
            .unwrap();
        (Store::new(state, Memory(fail)), now)
    }
    #[test]
    fn required_delivery_human_gate_chat_and_transition_are_atomic() {
        let fail = Cell::new(false);
        let (mut store, now) = fixture(&fail, false);
        let mut ids = Ids(0);
        assert!(
            store
                .complete_workflow_phase(
                    "ses_0",
                    "accept",
                    "ready",
                    false,
                    Mutation {
                        now: &now,
                        ids: &mut ids
                    }
                )
                .is_err()
        );
        let d = store
            .put_workflow_deliverable(
                "ses_0",
                "plan",
                "first version",
                None,
                Context {
                    now: &now,
                    id: "del_1",
                },
            )
            .unwrap();
        let updated = store
            .put_workflow_deliverable(
                "ses_0",
                "PLAN",
                "better version",
                None,
                Context {
                    now: &now,
                    id: "del_unused",
                },
            )
            .unwrap();
        assert_eq!(d.id, updated.id);
        assert_eq!(updated.revision, 1);
        fail.set(true);
        assert_eq!(
            store
                .complete_workflow_phase(
                    "ses_0",
                    "accept",
                    "ready",
                    false,
                    Mutation {
                        now: &now,
                        ids: &mut ids
                    }
                )
                .unwrap_err(),
            Error::Storage(10)
        );
        assert_eq!(
            store.state.phase_runs.get("run_0").unwrap().status,
            d::PHASE_RUN_RUNNING
        );
        assert!(store.state.workflow_questions.is_empty());
        fail.set(false);
        let first = store
            .complete_workflow_phase(
                "ses_0",
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
        assert!(
            store
                .resume_workflow_phase_for_chat("ses_0", "other", &now)
                .is_err()
        );
        assert!(
            store
                .resume_workflow_phase_for_chat("ses_0", "derek", &now)
                .unwrap()
        );
        assert!(
            store
                .answer_workflow_question(
                    &first.id,
                    "derek",
                    "accept",
                    "",
                    Mutation {
                        now: &now,
                        ids: &mut ids
                    }
                )
                .is_err()
        );
        assert!(
            store
                .settle_workflow_chat_turn(
                    "ses_0",
                    Mutation {
                        now: &now,
                        ids: &mut ids
                    }
                )
                .unwrap()
        );
        assert_eq!(store.state.workflow_questions.len(), 1);
        assert!(
            store
                .resume_workflow_phase_for_chat("ses_0", "derek", &now)
                .unwrap()
        );
        let replacement = store
            .complete_workflow_phase(
                "ses_0",
                "accept",
                "updated judgment",
                false,
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap()
            .question
            .unwrap();
        assert_eq!(
            store
                .state
                .workflow_questions
                .get(&first.id)
                .unwrap()
                .status,
            "superseded"
        );
        assert!(
            store
                .answer_workflow_question(
                    &first.id,
                    "derek",
                    "accept",
                    "",
                    Mutation {
                        now: &now,
                        ids: &mut ids
                    }
                )
                .is_err()
        );
        fail.set(true);
        assert_eq!(
            store
                .answer_workflow_question(
                    &replacement.id,
                    "derek",
                    "accept",
                    "",
                    Mutation {
                        now: &now,
                        ids: &mut ids
                    }
                )
                .unwrap_err(),
            Error::Storage(10)
        );
        assert_eq!(store.state.sessions.len(), 1);
        assert_eq!(
            store
                .state
                .workflow_questions
                .get(&replacement.id)
                .unwrap()
                .status,
            "open"
        );
        fail.set(false);
        let next = store
            .answer_workflow_question(
                &replacement.id,
                "derek",
                "accept",
                "",
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        assert_eq!(next.phase_run.phase_id, "review");
        assert_eq!(
            next.next_session.as_ref().unwrap().parent_session_id,
            "ses_0"
        );
        let session = next.next_session.unwrap();
        store
            .mark_workflow_phase_running(&session.id, &now)
            .unwrap();
        let done = store
            .complete_workflow_phase(
                &session.id,
                "accept",
                "done",
                false,
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        assert_eq!(done.job.status, d::JOB_DONE);
        assert!(done.job.current_phase_run_id.is_empty());
        assert!(done.next_session.is_none());
    }
    #[test]
    fn failed_action_accept_retries_instead_of_false_done() {
        let fail = Cell::new(false);
        let (mut store, now) = fixture(&fail, true);
        let mut ids = Ids(0);
        let failed = store
            .complete_workflow_phase(
                "ses_0",
                "reject",
                "merge conflict",
                false,
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        let q = failed.question.unwrap();
        assert_eq!(q.kind, "action");
        assert_eq!(q.accept_target, "build");
        assert_eq!(q.reject_target, "build");
        let retried = store
            .answer_workflow_question(
                &q.id,
                "derek",
                "accept",
                "",
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap();
        assert_eq!(retried.job.status, d::JOB_ACTIVE);
        assert_eq!(retried.phase_run.attempt, 2);
        assert!(
            retried
                .next_session
                .unwrap()
                .environment_selector
                .is_empty()
        );
        assert!(
            store
                .answer_workflow_question(
                    &q.id,
                    "derek",
                    "accept",
                    "",
                    Mutation {
                        now: &now,
                        ids: &mut ids
                    }
                )
                .is_err()
        );
    }
    #[test]
    fn form_answers_resume_same_session_and_retry_fences_old_activation() {
        let fail = Cell::new(false);
        let (mut store, now) = fixture(&fail, false);
        let items = List::<d::WorkflowQuestionItem>::from_json(
            br#"[{"question":"Which?","options":["A","A"," B "]},{"question":"Why?"}]"#,
        )
        .unwrap();
        let q = store
            .ask_workflow_questions(
                "ses_0",
                &items,
                Context {
                    now: &now,
                    id: "ask_form",
                },
            )
            .unwrap();
        assert_eq!(q.items[0].options.len(), 2);
        let incomplete =
            List::<d::WorkflowQuestionAnswer>::from_json(br#"[{"item_id":"q1","answer":"A"}]"#)
                .unwrap();
        assert!(
            store
                .answer_workflow_questions(&q.id, "derek", &incomplete, &now)
                .is_err()
        );
        assert_eq!(
            store.state.phase_runs.get("run_0").unwrap().status,
            d::PHASE_RUN_PENDING
        );
        let answers = List::<d::WorkflowQuestionAnswer>::from_json(
            br#"[{"item_id":"q1","answer":"B"},{"item_id":"q2","answer":"my words"}]"#,
        )
        .unwrap();
        let answer = store
            .answer_workflow_questions(&q.id, "derek", &answers, &now)
            .unwrap();
        assert!(!answer.items[0].other);
        assert!(answer.items[1].other);
        assert_eq!(store.state.sessions.len(), 1);
        let q = store
            .ask_workflow_questions(
                "ses_0",
                &items,
                Context {
                    now: &now,
                    id: "ask_again",
                },
            )
            .unwrap();
        let transcript =
            List::<d::ChatLine>::from_json(br#"[{"role":"user","text":"keep this"}]"#).unwrap();
        let (retried, old) = store
            .retry_workflow_session("ses_0", "derek", "retry note", &transcript, &now)
            .unwrap();
        assert_eq!(old, "cmp_0");
        assert_eq!(retried.session.id, "ses_0");
        assert!(retried.session.activation_id.is_empty());
        assert!(retried.session.client_id.is_empty());
        let run = store.state.phase_runs.get("run_0").unwrap();
        assert_eq!(run.attempt, 1);
        assert_eq!(run.restarts, 1);
        assert_eq!(run.restart_transcript.len(), 1);
        assert_eq!(
            store.state.workflow_questions.get(&q.id).unwrap().answer,
            "retry"
        );
    }
    #[test]
    fn repair_waits_for_active_turn_and_restore_is_idempotent() {
        let fail = Cell::new(false);
        let (mut store, now) = fixture(&fail, true);
        let mut ids = Ids(0);
        let q = store
            .complete_workflow_phase(
                "ses_0",
                "reject",
                "failed",
                false,
                Mutation {
                    now: &now,
                    ids: &mut ids,
                },
            )
            .unwrap()
            .question
            .unwrap();
        store
            .resume_workflow_phase_for_chat("ses_0", "derek", &now)
            .unwrap();
        store
            .state
            .workflow_questions
            .get_mut(&q.id)
            .unwrap()
            .status = try_string("answered").unwrap();
        let c = d::Composition::from_json(
            br#"{"id":"cmp_0","runtime":{"status":"running"},"agent":{"prompt_id":"prompt"}}"#,
        )
        .unwrap();
        store
            .state
            .compositions
            .insert(try_string("cmp_0").unwrap(), c)
            .unwrap();
        assert_eq!(
            store
                .repair_standing_decisions(Mutation {
                    now: &now,
                    ids: &mut ids
                })
                .unwrap(),
            0
        );
        store
            .state
            .compositions
            .get_mut("cmp_0")
            .unwrap()
            .agent
            .as_mut()
            .unwrap()
            .prompt_id
            .clear();
        assert_eq!(
            store
                .repair_standing_decisions(Mutation {
                    now: &now,
                    ids: &mut ids
                })
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .repair_standing_decisions(Mutation {
                    now: &now,
                    ids: &mut ids
                })
                .unwrap(),
            0
        );
        assert_eq!(store.state.workflow_questions.len(), 2);
    }
}
