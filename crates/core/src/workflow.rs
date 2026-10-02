//! Templatevalidatie: iedere route en iedere geïnjecteerde oplevering bestaat.
use crate::validation::{invalid, normalized, selector, selectors, text, valid_token};
use alloc::string::String;
use spin_domain::{self as d, Fallible, List, Map, TryClone, try_string};

/// Of de actie een door Spin uitgevoerde merge of pull request is.
pub fn is_workflow_action(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case(d::WORKFLOW_ACTION_GIT_MERGE)
        || value.eq_ignore_ascii_case(d::WORKFLOW_ACTION_GIT_PULL_REQUEST)
}

/// Normaliseert de hele Template voordat de eigenaar iets opslaat.
pub fn normalize_template(
    mut req: d::CreateWorkflowTemplateRequest,
) -> Fallible<d::CreateWorkflowTemplateRequest> {
    req.operator = normalized(&req.operator)?;
    req.name = try_string(req.name.trim())?;
    req.description = try_string(req.description.trim())?;
    req.git_selector = if req.git_selector.trim().is_empty() {
        String::new()
    } else {
        selector(&req.git_selector)?
    };
    if req.operator.is_empty() || req.name.is_empty() || req.phases.is_empty() {
        return Err(invalid(
            "template",
            "operator, name and at least one phase are required",
        ));
    }
    let mut ids: List<String> = List::new();
    let mut available: Map<String> = Map::new();
    for (index, phase) in req.phases.as_mut_slice().iter_mut().enumerate() {
        normalize_phase(phase, index)?;
        if ids.contains(&phase.id) {
            return Err(invalid(&phase.id, "phase needs a unique id"));
        }
        ids.push(try_string(&phase.id)?)?;
        normalize_injects(phase, &available)?;
        normalize_deliverables(phase, &mut available)?;
    }
    for phase in req.phases.as_mut_slice() {
        normalize_transitions(phase, &ids)?;
    }
    Ok(req)
}

fn normalize_phase(phase: &mut d::WorkflowPhase, index: usize) -> Fallible {
    if phase.executor == d::WORKFLOW_EXECUTOR_ACTION || phase.action.is_some() {
        if !phase
            .action
            .as_ref()
            .is_some_and(|a| is_workflow_action(&a.r#type))
        {
            return Err(invalid(
                &phase.id,
                "a step Spin performs itself is a merge or a pull request",
            ));
        }
        phase.executor = try_string(d::WORKFLOW_EXECUTOR_ACTION)?;
    }
    if phase.executor.is_empty() {
        phase.executor = try_string(d::WORKFLOW_EXECUTOR_AGENT)?;
    }
    phase.name = try_string(phase.name.trim())?;
    phase.instructions = try_string(phase.instructions.trim())?;
    phase.model = try_string(phase.model.trim())?;
    phase.reasoning_effort = try_string(phase.reasoning_effort.trim())?;
    phase.id = normalized(&phase.id)?;
    if phase.id.is_empty() {
        phase.id = text(format_args!("step-{}", index + 1))?;
    }
    if !valid_token(&phase.id) || phase.name.is_empty() {
        return Err(invalid(&phase.id, "phase needs a valid id and name"));
    }
    match phase.executor.as_str() {
        d::WORKFLOW_EXECUTOR_AGENT => {
            if phase.instructions.is_empty() {
                return Err(invalid(&phase.id, "agent phase needs instructions"));
            }
            if phase.resolve_merge {
                phase.allow_changes = true;
            }
            phase.action = None;
            phase.environment_selector = normalized(&phase.environment_selector)?;
            if !phase.environment_selector.is_empty() {
                selector(&phase.environment_selector)?;
            }
            phase.with_selectors = selectors(&phase.with_selectors)?;
        }
        d::WORKFLOW_EXECUTOR_EXPOSE => {
            phase.resolve_merge = false;
            phase.action = None;
            phase.environment_selector = normalized(&phase.environment_selector)?;
            phase.with_selectors = List::default();
            phase.model.clear();
            phase.reasoning_effort.clear();
            phase.deliverables = List::default();
        }
        d::WORKFLOW_EXECUTOR_ACTION => {
            phase.resolve_merge = false;
            let action = phase
                .action
                .as_mut()
                .ok_or_else(|| invalid(&phase.id, "missing system action"))?;
            action.r#type = normalized(&action.r#type)?;
            phase.environment_selector.clear();
            phase.with_selectors = List::default();
            phase.model.clear();
            phase.reasoning_effort.clear();
            phase.deliverables = List::default();
            phase.inject = List::default();
            phase.allow_changes = false;
        }
        _ => return Err(invalid(&phase.id, "unsupported executor")),
    }
    Ok(())
}
fn normalize_injects(phase: &mut d::WorkflowPhase, available: &Map<String>) -> Fallible {
    let mut inject = List::new();
    let mut seen = List::new();
    for requested in phase.inject.iter() {
        let key = normalized(requested)?;
        let canonical = available
            .get(&key)
            .ok_or_else(|| invalid(requested, "unknown earlier deliverable"))?;
        if seen.contains(&key) {
            return Err(invalid(requested, "deliverable injected more than once"));
        }
        seen.push(key)?;
        inject.push(try_string(canonical)?)?;
    }
    phase.inject = inject;
    Ok(())
}
fn normalize_deliverables(phase: &mut d::WorkflowPhase, available: &mut Map<String>) -> Fallible {
    let mut seen = List::new();
    let mut deliverables = List::new();
    for deliverable in phase.deliverables.iter() {
        let mut item = deliverable.try_clone()?;
        item.name = try_string(item.name.trim())?;
        item.description = try_string(item.description.trim())?;
        item.kind = normalized(&item.kind)?;
        if item.kind.is_empty() {
            item.kind = try_string(d::DELIVERABLE_KIND_MARKDOWN)?;
        }
        if !d::DELIVERABLE_KINDS.contains(&item.kind.as_str()) {
            return Err(invalid(&item.kind, "unknown deliverable kind"));
        }
        let key = normalized(&item.name)?;
        if key.is_empty() || seen.contains(&key) {
            return Err(invalid(&phase.id, "empty or duplicate deliverable"));
        }
        seen.push(try_string(&key)?)?;
        if !available.contains_key(&key) {
            available.insert(key, try_string(&item.name)?)?;
        }
        deliverables.push(item)?;
    }
    phase.deliverables = deliverables;
    Ok(())
}
fn valid_target(target: &str, ids: &[String]) -> Fallible<bool> {
    let target = target.trim();
    if [
        d::WORKFLOW_TARGET_NEXT,
        d::WORKFLOW_TARGET_SELF,
        d::WORKFLOW_TARGET_DONE,
        d::WORKFLOW_TARGET_ASK_USER,
    ]
    .iter()
    .any(|t| target.eq_ignore_ascii_case(t))
    {
        return Ok(true);
    }
    Ok(ids.contains(&normalized(target)?))
}
fn normalize_transitions(phase: &mut d::WorkflowPhase, ids: &[String]) -> Fallible {
    if phase.accept.target.trim().is_empty() {
        phase.accept.target = try_string(d::WORKFLOW_TARGET_NEXT)?;
    }
    if phase.reject.target.trim().is_empty() {
        phase.reject.target = try_string(d::WORKFLOW_TARGET_SELF)?;
    }
    if phase.accept.max < 0 || phase.reject.max < 0 {
        return Err(invalid(&phase.id, "transition max cannot be negative"));
    }
    if phase.reject.max > 0 && phase.reject.exhausted.trim().is_empty() {
        phase.reject.exhausted = try_string(d::WORKFLOW_TARGET_ASK_USER)?;
    }
    for transition in [&phase.accept, &phase.reject] {
        for target in [&transition.target, &transition.exhausted] {
            if !target.is_empty() && !valid_target(target, ids)? {
                return Err(invalid(target, "unknown workflow target"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use d::Wire;
    const TEMPLATE: &[u8] = br#"{"operator":" Derek ","name":" Ontwikkeling ","phases":[{"id":"design","name":"Ontwerp","instructions":"Maak het ontwerp","deliverables":[{"name":"FO","required":true}],"accept":{"target":"develop"},"reject":{"max":2}},{"id":"develop","name":"Ontwikkelen","instructions":"Bouw het","inject":["fo"],"resolve_merge":true,"accept":{"target":"DONE"}}]}"#;
    #[test]
    fn canonical_injections_and_defaults() {
        let t = normalize_template(d::CreateWorkflowTemplateRequest::from_json(TEMPLATE).unwrap())
            .unwrap();
        assert_eq!(t.operator, "derek");
        assert_eq!(t.phases[0].deliverables[0].kind, "markdown");
        assert_eq!(t.phases[0].reject.exhausted, "ASK_USER");
        assert_eq!(t.phases[1].inject[0], "FO");
        assert!(t.phases[1].allow_changes);
    }
    #[test]
    fn reject_future_or_duplicate_injections_and_unknown_routes() {
        for text in ["FO", "unknown"] {
            let mut t = d::CreateWorkflowTemplateRequest::from_json(TEMPLATE).unwrap();
            t.phases.as_mut_slice()[0]
                .inject
                .push(try_string(text).unwrap())
                .unwrap();
            assert!(normalize_template(t).is_err());
        }
        let mut t = d::CreateWorkflowTemplateRequest::from_json(TEMPLATE).unwrap();
        t.phases.as_mut_slice()[1]
            .inject
            .push(try_string("FO").unwrap())
            .unwrap();
        assert!(normalize_template(t).is_err());
        let mut t = d::CreateWorkflowTemplateRequest::from_json(TEMPLATE).unwrap();
        t.phases.as_mut_slice()[0].accept.target = try_string("missing").unwrap();
        assert!(normalize_template(t).is_err());
    }
    #[test]
    fn system_action_is_not_an_agent() {
        let t = d::CreateWorkflowTemplateRequest::from_json(br#"{"operator":"derek","name":"Merge","phases":[{"name":"Land","action":{"type":" GIT.MERGE "},"allow_changes":true,"resolve_merge":true,"model":"x"}]}"#).unwrap();
        let t = normalize_template(t).unwrap();
        assert_eq!(t.phases[0].id, "step-1");
        assert_eq!(t.phases[0].executor, "action");
        assert!(!t.phases[0].allow_changes);
        assert!(!t.phases[0].resolve_merge);
        assert!(t.phases[0].model.is_empty());
    }
}
