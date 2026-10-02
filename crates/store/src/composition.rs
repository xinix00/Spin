//! Laagvolgorde en sessieworkspaces, met één eigenaar voor de hele compositie.
use crate::{Context, Error, Persistence, Result, Store};
use alloc::{string::String, vec::Vec};
use spin_core::validation::{
    can_use_artifact, merge_enablements, normalized, selector, selectors, text,
};
use spin_domain::{self as d, List, Map, TryClone, WireMap, state::PersistedState, try_string};

pub(crate) fn resolve<'a>(
    state: &'a PersistedState,
    value: &str,
    operator: &str,
    profile: &str,
) -> Result<&'a d::Artifact> {
    let value = selector(value)?;
    let (kind, name) = value
        .split_once(':')
        .ok_or(Error::Conflict("invalid selector"))?;
    crate::recordings::latest(state, kind, name, operator, profile)
}
pub(crate) fn enables(artifact: &d::Artifact, capability: &str) -> bool {
    artifact
        .enables
        .iter()
        .any(|e| e.name.trim().eq_ignore_ascii_case(capability.trim()))
}
fn newest<'a>(
    state: &'a PersistedState,
    mut artifact: &'a d::Artifact,
    operator: &str,
) -> Result<&'a d::Artifact> {
    for _ in 0..64 {
        if artifact.superseded_by.is_empty() {
            break;
        }
        let Some(next) = state.artifacts.get(&artifact.superseded_by) else {
            break;
        };
        if !can_use_artifact(operator, next)? {
            break;
        }
        artifact = next;
    }
    Ok(artifact)
}
fn find<'a>(
    state: &'a PersistedState,
    id: &str,
    mut check: impl FnMut(&d::Artifact) -> Result<bool>,
) -> Result<Option<&'a d::Artifact>> {
    let mut stack = Vec::new();
    let mut seen = Map::new();
    let Some(root) = state.artifacts.get(id) else {
        return Ok(None);
    };
    d::try_push(&mut stack, root.id.as_str())?;
    while let Some(id) = stack.pop() {
        if seen.contains_key(id) {
            continue;
        }
        seen.insert(try_string(id)?, true)?;
        if let Some(a) = state.artifacts.get(id) {
            if check(a)? {
                return Ok(Some(a));
            }
            for parent in a.parent_artifact_ids.iter().rev() {
                d::try_push(&mut stack, parent.as_str())?;
            }
        }
    }
    Ok(None)
}
fn depends(state: &PersistedState, id: &str, parent: &str) -> Result<bool> {
    Ok(id == parent || find(state, id, |a| Ok(a.id == parent))?.is_some())
}
pub(crate) fn artifact_enables(state: &PersistedState, id: &str, capability: &str) -> Result<bool> {
    Ok(find(state, id, |a| Ok(enables(a, capability)))?.is_some())
}
fn identity<'a>(
    state: &'a PersistedState,
    artifact: &'a d::Artifact,
    operator: &str,
) -> Result<&'a d::Artifact> {
    let ancestor = newest(state, artifact, &artifact.created_by)?;
    let mut own = None;
    let mut shared = None;
    for (_, candidate) in state.artifacts.iter() {
        if candidate.kind != d::ARTIFACT_CREDENTIAL || !candidate.superseded_by.is_empty() {
            continue;
        }
        let mine = candidate.scope == d::SCOPE_USER && candidate.subject == operator;
        if !mine && candidate.scope != d::SCOPE_GLOBAL {
            continue;
        }
        if find(state, &candidate.id, |a| {
            Ok(newest(state, a, &a.created_by)?.id == ancestor.id)
        })?
        .is_none()
        {
            continue;
        }
        let slot: &mut Option<&d::Artifact> = if mine { &mut own } else { &mut shared };
        if slot.is_none()
            || candidate.created_at.time()?
                > slot
                    .map(|a| a.created_at.time())
                    .transpose()?
                    .unwrap_or_default()
        {
            *slot = Some(candidate);
        }
    }
    Ok(own.or(shared).unwrap_or(artifact))
}
fn add<'a>(
    state: &'a PersistedState,
    id: &'a str,
    present: &mut Map<bool>,
    out: &mut Vec<&'a d::Artifact>,
) -> Result {
    let mut pending = Vec::new();
    let mut active = Map::new();
    d::try_push(&mut pending, (id, false))?;
    while let Some((id, finish)) = pending.pop() {
        if present.contains_key(id) {
            continue;
        }
        let Some(a) = state.artifacts.get(id) else {
            continue;
        };
        if finish {
            present.insert(try_string(id)?, true)?;
            active.remove(id);
            d::try_push(out, a)?;
            continue;
        }
        if active.contains_key(id) {
            return Err(Error::Conflict("artifact dependency cycle"));
        }
        if present.len() + active.len() >= spin_core::layers::MAX_LAYERS {
            return Err(Error::Conflict("too many layers"));
        }
        active.insert(try_string(id)?, true)?;
        d::try_push(&mut pending, (id, true))?;
        for parent in a.parent_artifact_ids.iter().rev() {
            d::try_push(&mut pending, (parent.as_str(), false))?;
        }
    }
    Ok(())
}
fn stack<'a>(
    state: &'a PersistedState,
    selections: &[&'a d::Artifact],
    operator: &str,
) -> Result<Vec<&'a d::Artifact>> {
    let mut out = Vec::new();
    let mut present = Map::new();
    for a in selections {
        add(state, &a.id, &mut present, &mut out)?;
    }
    for _ in 0..16 {
        let mut lifted = false;
        let mut index = 0;
        while let Some(current) = out.get(index).copied() {
            index += 1;
            if current.superseded_by.is_empty() || present.contains_key(&current.superseded_by) {
                continue;
            }
            let Some(next) = state.artifacts.get(&current.superseded_by) else {
                continue;
            };
            if !can_use_artifact(operator, next)? {
                continue;
            }
            let mut insert = Vec::new();
            add(state, &next.id, &mut present, &mut insert)?;
            out.try_reserve(insert.len())
                .map_err(|_| d::Error::OutOfMemory)?;
            out.splice(index..index, insert);
            lifted = true;
        }
        if !lifted {
            break;
        }
    }
    Ok(out)
}
pub(crate) fn direct(
    state: &PersistedState,
    operator: &str,
    value: &str,
    capability: &str,
    profile: &str,
) -> Result {
    if !enables(resolve(state, value, operator, profile)?, capability) {
        return Err(Error::Conflict(
            "environment does not directly ENABLE capability",
        ));
    }
    Ok(())
}
pub(crate) fn default_selector(
    state: &PersistedState,
    operator: &str,
    capability: &str,
    profile: &str,
) -> Result<String> {
    let mut choice = None;
    for (_, a) in state.artifacts.iter() {
        if !can_use_artifact(operator, a)?
            || (!profile.is_empty() && a.profile != profile)
            || !enables(a, capability)
        {
            continue;
        }
        let value = text(format_args!("{}:{}", a.kind, a.name))?;
        if choice.as_ref().is_some_and(|s| *s != value) {
            return Err(Error::Conflict(
                "multiple environments ENABLE capability; select one explicitly",
            ));
        }
        choice = Some(value);
    }
    choice.ok_or(Error::Conflict("no environment ENABLES capability"))
}
pub(crate) fn project_layers(
    state: &PersistedState,
    operator: &str,
    values: &[String],
    profile: &str,
) -> Result {
    for value in values {
        let a = resolve(state, value, operator, profile)?;
        if a.kind == d::ARTIFACT_CREDENTIAL || enables(a, "git") || enables(a, "acp") {
            return Err(Error::Conflict(
                "identity or control-plane environment cannot be a project layer",
            ));
        }
    }
    Ok(())
}
pub(crate) fn session_environment(
    state: &PersistedState,
    operator: &str,
    value: &str,
    with: &[String],
    profile: &str,
) -> Result {
    let mut git = false;
    let mut acp = false;
    for value in core::iter::once(value).chain(with.iter().map(String::as_str)) {
        let a = resolve(state, value, operator, profile)?;
        git |= artifact_enables(state, &a.id, "git")?;
        acp |= artifact_enables(state, &a.id, "acp")?;
    }
    if !git || !acp {
        return Err(Error::Conflict(
            "Session environment must ENABLE git and acp",
        ));
    }
    Ok(())
}
fn fill_workspace(
    state: &PersistedState,
    job: &d::Job,
    session: &d::Session,
    repository: &d::JobRepository,
    operator: &str,
    merge: &str,
) -> Result<d::GitWorkspace> {
    let current = state
        .git_repositories
        .get(&repository.repository_id)
        .ok_or(Error::NotFound)?;
    let fallback =
        |value: &str, default: &str| try_string(if value.is_empty() { default } else { value });
    let mut w = d::GitWorkspace {
        repository_id: current.id.try_clone()?,
        repository_name: fallback(&repository.name, &current.name)?,
        remote_url: fallback(&repository.remote_url, &current.remote_url)?,
        provider: fallback(&repository.provider, &current.provider)?,
        credential_scope: fallback(&repository.credential_scope, &current.credential_scope)?,
        path: repository.path.try_clone()?,
        mode: repository.mode.try_clone()?,
        merge_ref: try_string(merge.trim())?,
        ..Default::default()
    };
    let base = fallback(&repository.base_ref, &current.default_ref)?;
    if repository.mode == d::REPOSITORY_MODE_REFERENCE {
        w.base_ref = base.try_clone()?;
        w.bootstrap_ref = base;
    } else {
        w.base_ref = job.branch.try_clone()?;
        w.bootstrap_ref = base;
        w.head_ref = session.git_ref.try_clone()?;
        w.target_ref = job.branch.try_clone()?;
        if let Some(source) = state
            .jobs
            .get(&job.forked_from_job_id)
            .filter(|j| !j.branch.trim().is_empty())
        {
            w.context_refs.push(source.branch.try_clone()?)?;
        }
    }
    if w.credential_scope != d::CREDENTIAL_SCOPE_PUBLIC {
        let account = crate::git_accounts::resolve(
            state,
            &w.remote_url,
            &w.provider,
            &w.credential_scope,
            operator,
        )?;
        w.provider = account.provider.try_clone()?;
        w.login = account.login.try_clone()?;
        w.author_name = account.name.try_clone()?;
        w.author_email = account.email.try_clone()?;
    }
    Ok(w)
}
impl<P: Persistence> Store<P> {
    /// De volledige laagsamenstelling en Session-binding worden samen opgeslagen.
    pub fn use_environment(
        &mut self,
        mut req: d::UseRequest,
        context: Context<'_>,
    ) -> Result<d::Composition> {
        context.validate()?;
        let operator = normalized(&req.operator)?;
        if operator.is_empty() {
            return Err(Error::Conflict("operator is required"));
        }
        if req.profile.is_empty() {
            req.profile = try_string("default")?;
        }
        let mut requested = normalized(&req.selector)?;
        if requested.is_empty() && !req.tool.trim().is_empty() {
            requested = text(format_args!("tool:{}", normalized(&req.tool)?))?;
        }
        if requested.is_empty() && !req.session_id.is_empty() {
            requested = text(format_args!("session:{}", req.session_id.trim()))?;
        }
        if requested.is_empty() {
            return Err(Error::Conflict("selector is required"));
        }
        let mut with = selectors(&req.with_selectors)?;
        let mut session_id = try_string(req.session_id.trim())?;
        if let Ok(value) = selector(&requested)
            && let Some(("session", name)) = value.split_once(':')
        {
            session_id = try_string(name)?;
        }
        self.edit(|state| {
            if state.compositions.get(context.id).is_some() {
                return Err(Error::Conflict("composition id already exists"));
            }
            let mut selected = requested.try_clone()?;
            let session = if session_id.is_empty() {
                d::Session::default()
            } else {
                let session = state.sessions.get(&session_id).ok_or(Error::NotFound)?;
                if matches!(
                    session.status.as_str(),
                    d::SESSION_CLAIMED
                        | d::SESSION_RUNNING
                        | d::SESSION_COMPLETED
                        | d::SESSION_CANCELLED
                ) {
                    return Err(Error::Conflict(
                        "Session cannot change composition in its current state",
                    ));
                }
                selected = if session.environment_selector.trim().is_empty() {
                    text(format_args!("tool:{}", normalized(&session.tool)?))?
                } else {
                    try_string(session.environment_selector.trim())?
                };
                with = session.with_selectors.try_clone()?;
                session.try_clone()?
            };
            let profile = normalized(&req.profile)?;
            let entry = resolve(state, &selected, &operator, &profile)?;
            let mut selections = Vec::new();
            d::try_push(&mut selections, entry)?;
            let mut reasons = Map::new();
            reasons.insert(
                entry.id.try_clone()?,
                text(format_args!("selected {selected}"))?,
            )?;
            for value in with.iter() {
                let artifact = resolve(state, value, &operator, &profile)?;
                d::try_push(&mut selections, artifact)?;
                if !reasons.contains_key(&artifact.id) {
                    reasons.insert(
                        artifact.id.try_clone()?,
                        text(format_args!("layer {value}"))?,
                    )?;
                }
            }
            let layers = stack(state, &selections, &operator)?;
            let mut c = d::Composition {
                id: try_string(context.id)?,
                operator,
                selector: requested,
                entry_artifact_id: entry.id.try_clone()?,
                session_id: session_id.try_clone()?,
                profile,
                with_selectors: with,
                requested_artifact_ids: List::new(),
                layers: List::new(),
                resolved_artifacts: List::new(),
                slot_bindings: WireMap::new(),
                enabled: List::new(),
                mcp_server_ids: session.mcp_server_ids.try_clone()?,
                warnings: List::new(),
                for_login: req.for_login,
                for_login_private: req.for_login && req.for_login_private,
                created_at: context.now.try_clone()?,
                ..Default::default()
            };
            for selection in selections {
                if !c.requested_artifact_ids.contains(&selection.id) {
                    c.requested_artifact_ids.push(selection.id.try_clone()?)?;
                }
            }
            let mut provided = Map::new();
            for a in &layers {
                c.layers.push(a.id.try_clone()?)?;
                if let Some(current) = c.slot_bindings.get(&a.slot)
                    && !a.slot.is_empty()
                    && *current != a.id
                    && !depends(state, &a.id, current)?
                {
                    return Err(Error::Conflict("layer slot is already bound"));
                }
                if !a.slot.is_empty() {
                    c.slot_bindings
                        .insert(a.slot.try_clone()?, a.id.try_clone()?)?;
                }
                let reason = if let Some(reason) = reasons.get(&a.id) {
                    reason.try_clone()?
                } else {
                    let above = layers
                        .iter()
                        .find(|other| other.parent_artifact_ids.contains(&a.id));
                    let label = if let Some(above) = above {
                        if let Some(reason) = reasons.get(&above.id) {
                            try_string(
                                reason
                                    .strip_prefix("selected ")
                                    .or_else(|| reason.strip_prefix("layer "))
                                    .unwrap_or(reason),
                            )?
                        } else {
                            text(format_args!("{}:{}", above.kind, above.name))?
                        }
                    } else {
                        try_string("the stack")?
                    };
                    text(format_args!("under {label}"))?
                };
                c.resolved_artifacts.push(d::ResolvedArtifact {
                    artifact_id: a.id.try_clone()?,
                    kind: a.kind.try_clone()?,
                    name: a.name.try_clone()?,
                    slot: a.slot.try_clone()?,
                    scope: a.scope.try_clone()?,
                    subject: a.subject.try_clone()?,
                    profile: a.profile.try_clone()?,
                    enables: a.enables.try_clone()?,
                    reason,
                })?;
                c.enabled = merge_enablements(&c.enabled, &a.enables)?;
                for capability in a.provides.iter() {
                    provided.insert(capability.try_clone()?, true)?;
                }
            }
            c.tool = layers
                .iter()
                .rev()
                .find(|a| enables(a, "acp"))
                .map_or(entry, |a| *a)
                .name
                .try_clone()?;
            if layers
                .iter()
                .any(|a| a.requires.iter().any(|r| !provided.contains_key(r)))
            {
                return Err(Error::Conflict(
                    "artifact requirement is not provided by composition",
                ));
            }
            if !session_id.is_empty() {
                for capability in ["git", "acp"] {
                    if !c.enabled.iter().any(|e| e.name == capability) {
                        return Err(Error::Conflict(
                            "Session environment must ENABLE git and acp",
                        ));
                    }
                }
                let job = state
                    .jobs
                    .get(&session.job_id)
                    .filter(|j| {
                        !j.git_repository_id.is_empty()
                            && j.git_repository_id == session.git_repository_id
                    })
                    .ok_or(Error::Conflict(
                        "Session has no valid Git repository binding",
                    ))?;
                let mut workspaces = List::new();
                for repository in job.job_repositories()?.iter() {
                    workspaces.push(fill_workspace(
                        state,
                        job,
                        &session,
                        repository,
                        &c.operator,
                        &req.merge_ref,
                    )?)?;
                }
                c.git = Some(
                    workspaces
                        .iter()
                        .find(|w| w.changes())
                        .ok_or(Error::Conflict("the Job changes none of its repositories"))?
                        .try_clone()?,
                );
                if workspaces.len() > 1 {
                    c.workspaces = workspaces;
                }
            }
            if let Some(session) = state.sessions.get_mut(&session_id) {
                session.operator = c.operator.try_clone()?;
                session.prepared_composition_id = c.id.try_clone()?;
                session.updated_at = c.created_at.try_clone()?;
            }
            state
                .compositions
                .insert(c.id.try_clone()?, c.try_clone()?)?;
            Ok(c)
        })
    }
    /// Vindt de nieuwste versie van de laag die deze capability levert.
    pub fn enabling_layer(&self, id: &str, capability: &str) -> Result<&d::Artifact> {
        let artifact = find(&self.state, id, |a| {
            Ok(a.enables.iter().any(|e| e.name == capability))
        })?
        .ok_or(Error::NotFound)?;
        newest(&self.state, artifact, &artifact.created_by)
    }
    /// De operator krijgt de eigen credentiallaag, anders de gedeelde pool of de tool.
    pub fn identity_layer_for(&self, id: &str, operator: &str) -> Result<&d::Artifact> {
        let a = self.artifact(id)?;
        identity(&self.state, a, &normalized(operator)?)
    }
    /// De capsule van een recording volgt opgewaardeerde ancestors van zijn ene parent.
    pub fn recording_stack(
        &self,
        recording: &d::Recording,
    ) -> Result<(List<String>, List<d::Artifact>, bool)> {
        let mut ids = List::default();
        let mut artifacts = List::default();
        let mut lifted = false;
        if recording.parent_artifact_ids.len() != 1 {
            return Ok((ids, artifacts, false));
        }
        let Some(parent) = recording
            .parent_artifact_ids
            .first()
            .and_then(|id| self.state.artifacts.get(id))
        else {
            return Ok((ids, artifacts, false));
        };
        for a in stack(&self.state, &[parent], &recording.actor)? {
            ids.push(a.id.try_clone()?)?;
            artifacts.push(a.try_clone()?)?;
            lifted |= !depends(&self.state, &parent.id, &a.id)?;
        }
        Ok((ids, artifacts, lifted))
    }
}

// De fase voegt haar agentidentiteit bovenop de Job-stack toe. Een al aanwezige
// versie van dezelfde laag wordt niet nogmaals toegevoegd.
pub(crate) fn phase_environment(
    state: &PersistedState,
    operator: &str,
    phase: &d::WorkflowPhase,
    base: &str,
    extra: &[String],
) -> Result<(String, List<String>)> {
    let mut with = List::new();
    for value in extra.iter().chain(phase.with_selectors.iter()) {
        if !value.trim().is_empty() && !with.contains(value) {
            with.push(value.try_clone()?)?;
        }
    }
    if phase.environment_selector.is_empty() || phase.environment_selector == base {
        return Ok((try_string(base)?, with));
    }
    let artifact = match resolve(state, &phase.environment_selector, operator, "default") {
        Ok(a) => Some(a),
        Err(Error::NotFound | Error::Conflict(_) | Error::Data(d::Error::Invalid { .. })) => None,
        Err(e) => return Err(e),
    };
    let layer = if let Some(mut artifact) = artifact {
        if artifact_enables(state, &artifact.id, "acp")? {
            artifact = identity(state, artifact, operator)?;
        }
        let newest_id = &newest(state, artifact, &artifact.created_by)?.id;
        for value in core::iter::once(base).chain(with.iter().map(String::as_str)) {
            let held = match resolve(state, value, operator, "default") {
                Ok(a) => a,
                Err(
                    Error::NotFound | Error::Conflict(_) | Error::Data(d::Error::Invalid { .. }),
                ) => continue,
                Err(e) => return Err(e),
            };
            if find(state, &held.id, |a| {
                Ok(&newest(state, a, &a.created_by)?.id == newest_id)
            })?
            .is_some()
            {
                return Ok((try_string(base)?, with));
            }
        }
        text(format_args!("{}:{}", artifact.kind, artifact.name))?
    } else {
        phase.environment_selector.try_clone()?
    };
    if !with.contains(&layer) {
        with.push(layer)?;
    }
    Ok((try_string(base)?, with))
}

pub(crate) fn agent_tool(
    state: &PersistedState,
    operator: &str,
    base: &str,
    with: &[String],
) -> Result<String> {
    for value in with
        .iter()
        .rev()
        .map(String::as_str)
        .chain(core::iter::once(base))
    {
        let artifact = match resolve(state, value, operator, "default") {
            Ok(a) => a,
            Err(Error::NotFound | Error::Conflict(_) | Error::Data(d::Error::Invalid { .. })) => {
                continue;
            }
            Err(e) => return Err(e),
        };
        if let Some(a) = find(state, &artifact.id, |a| Ok(enables(a, "acp")))? {
            return Ok(a.name.try_clone()?);
        }
    }
    Ok(try_string(
        base.split_once(':').map_or("", |(_, name)| name),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Memory;
    use core::cell::Cell;
    use d::{Timestamp, Wire};
    fn state() -> PersistedState {
        PersistedState::from_json(br#"{"artifacts":{
      "node1":{"id":"node1","kind":"tool","name":"node","scope":"global","profile":"default","slot":"tool:node","superseded_by":"node2"},
      "node2":{"id":"node2","kind":"tool","name":"node","scope":"global","profile":"default","slot":"tool:node","parent_artifact_ids":["node1"]},
      "codex":{"id":"codex","kind":"tool","name":"codex","scope":"global","profile":"default","slot":"tool:codex","parent_artifact_ids":["node1"],"enables":[{"name":"acp","command":"codex"}]},
      "cred":{"id":"cred","kind":"credential","name":"codex","scope":"user","subject":"derek","profile":"default","slot":"credential:codex","parent_artifact_ids":["codex"]},
      "git":{"id":"git","kind":"tool","name":"git","scope":"global","profile":"default","slot":"tool:git","enables":[{"name":"git"}]}
    }}"#).unwrap()
    }
    fn now() -> Timestamp {
        Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap()
    }
    #[test]
    fn composition_lifts_old_parents_and_keeps_operator_credentials_separate() {
        let fail = Cell::new(false);
        let mut store = Store::new(state(), Memory(&fail));
        assert_eq!(
            store.identity_layer_for("codex", "derek").unwrap().id,
            "cred"
        );
        assert_eq!(
            store.identity_layer_for("codex", "other").unwrap().id,
            "codex"
        );
        let req = || {
            d::UseRequest::from_json(br#"{"operator":"derek","selector":"credential:codex","with_selectors":["tool:git"]}"#).unwrap()
        };
        fail.set(true);
        assert_eq!(
            store
                .use_environment(
                    req(),
                    Context {
                        now: &now(),
                        id: "c"
                    }
                )
                .unwrap_err(),
            Error::Storage(10)
        );
        assert!(store.state.compositions.is_empty());
        fail.set(false);
        let c = store
            .use_environment(
                req(),
                Context {
                    now: &now(),
                    id: "c",
                },
            )
            .unwrap();
        assert_eq!(&*c.layers, &["node1", "node2", "codex", "cred", "git"]);
        assert_eq!(c.slot_bindings.get("tool:node").unwrap(), "node2");
        assert_eq!(c.tool, "codex");
        assert_eq!(c.enabled.len(), 2);
        let recording =
            d::Recording::from_json(br#"{"actor":"derek","parent_artifact_ids":["codex"]}"#)
                .unwrap();
        let (layers, _, lifted) = store.recording_stack(&recording).unwrap();
        assert!(lifted);
        assert_eq!(&*layers, &["node1", "node2", "codex"]);
    }
    #[test]
    fn session_composition_binds_public_workspaces_and_requires_both_capabilities() {
        let fail = Cell::new(false);
        let mut state = state();
        state.git_repositories.insert("g".into(),d::GitRepository::from_json(br#"{"id":"g","name":"repo","remote_url":"https://example.com/repo.git","default_ref":"main","credential_scope":"public"}"#).unwrap()).unwrap();
        state
            .jobs
            .insert(
                "j".into(),
                d::Job::from_json(br#"{"id":"j","branch":"jobs/j/main","git_repository_id":"g"}"#)
                    .unwrap(),
            )
            .unwrap();
        state.sessions.insert("s".into(),d::Session::from_json(br#"{"id":"s","job_id":"j","status":"queued","environment_selector":"tool:codex","with_selectors":["tool:git"],"git_repository_id":"g","git_ref":"jobs/j/sessions/s"}"#).unwrap()).unwrap();
        let mut store = Store::new(state, Memory(&fail));
        let c = store
            .use_environment(
                d::UseRequest::from_json(br#"{"operator":"derek","selector":"session:s"}"#)
                    .unwrap(),
                Context {
                    now: &now(),
                    id: "c",
                },
            )
            .unwrap();
        let git = c.git.unwrap();
        assert_eq!(git.remote_url, "https://example.com/repo.git");
        assert_eq!(git.head_ref, "jobs/j/sessions/s");
        assert_eq!(
            store
                .state
                .sessions
                .get("s")
                .unwrap()
                .prepared_composition_id,
            "c"
        );
        assert!(
            store
                .validate_session_environment("derek", "tool:codex", &[], "default")
                .is_err()
        );
    }
    #[test]
    fn cyclic_metadata_and_unrelated_slot_replacement_fail_without_publication() {
        let fail = Cell::new(false);
        let mut state = state();
        state
            .artifacts
            .get_mut("node1")
            .unwrap()
            .parent_artifact_ids
            .push("codex".into())
            .unwrap();
        let mut store = Store::new(state, Memory(&fail));
        let req = || {
            d::UseRequest::from_json(br#"{"operator":"derek","selector":"tool:codex"}"#).unwrap()
        };
        assert!(matches!(
            store.use_environment(
                req(),
                Context {
                    now: &now(),
                    id: "c"
                }
            ),
            Err(Error::Conflict("artifact dependency cycle"))
        ));
        store
            .state
            .artifacts
            .get_mut("node1")
            .unwrap()
            .parent_artifact_ids = List::new();
        store.state.artifacts.get_mut("git").unwrap().slot = "tool:node".into();
        let mut with = req();
        with.with_selectors.push("tool:git".into()).unwrap();
        assert!(matches!(
            store.use_environment(
                with,
                Context {
                    now: &now(),
                    id: "c"
                }
            ),
            Err(Error::Conflict("layer slot is already bound"))
        ));
        assert_eq!(store.version(), 0);
    }
}
