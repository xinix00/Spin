//! Templatewijzigingen maken een nieuwe revisie; Jobs houden hun eigen snapshot.
use crate::composition::{default_selector, direct, project_layers, session_environment};
use crate::{Context, Error, Persistence, Result, Store};
use alloc::string::String;
use spin_core::{validation::normalized, workflow::normalize_template};
use spin_domain::{self as d, Timestamp, TryClone, try_string};
impl<P: Persistence> Store<P> {
    /// Nieuwe templates hebben één ondubbelzinnige laag die direct Git ENABLES.
    pub fn create_workflow_template(
        &mut self,
        req: d::CreateWorkflowTemplateRequest,
        context: Context<'_>,
    ) -> Result<d::WorkflowTemplate> {
        context.validate()?;
        let mut req = normalize_template(req)?;
        self.edit(|state| {
            if state.workflow_templates.get(context.id).is_some() {
                return Err(Error::Conflict("template id already exists"));
            }
            if req.git_selector.is_empty() {
                req.git_selector = default_selector(state, &req.operator, "git", "default")?;
            }
            direct(state, &req.operator, &req.git_selector, "git", "default")?;
            let name = normalized(&req.name)?;
            for (_, existing) in state.workflow_templates.iter() {
                if normalized(&existing.name)? == name {
                    return Err(Error::Conflict("template name already exists"));
                }
            }
            let template = d::WorkflowTemplate {
                id: try_string(context.id)?,
                revision: 1,
                name: req.name,
                description: req.description,
                created_by: req.operator,
                git_selector: req.git_selector,
                phases: req.phases,
                created_at: context.now.try_clone()?,
                updated_at: context.now.try_clone()?,
            };
            state
                .workflow_templates
                .insert(template.id.try_clone()?, template.try_clone()?)?;
            Ok(template)
        })
    }
    /// Alleen de maker vervangt de template; reeds gestarte Jobs worden niet gewijzigd.
    pub fn update_workflow_template(
        &mut self,
        id: &str,
        req: d::CreateWorkflowTemplateRequest,
        now: &Timestamp,
    ) -> Result<d::WorkflowTemplate> {
        let mut req = normalize_template(req)?;
        self.edit(|state| {
            if req.git_selector.is_empty() {
                req.git_selector = default_selector(state, &req.operator, "git", "default")?;
            }
            direct(state, &req.operator, &req.git_selector, "git", "default")?;
            let old = state
                .workflow_templates
                .get(id.trim())
                .ok_or(Error::NotFound)?;
            if old.created_by != req.operator {
                return Err(Error::Conflict("template belongs to another operator"));
            }
            let name = normalized(&req.name)?;
            for (_, existing) in state.workflow_templates.iter() {
                if existing.id != old.id && normalized(&existing.name)? == name {
                    return Err(Error::Conflict("template name already exists"));
                }
            }
            let template = state
                .workflow_templates
                .get_mut(id.trim())
                .ok_or(Error::NotFound)?;
            template.name = req.name;
            template.description = req.description;
            template.git_selector = req.git_selector;
            template.phases = req.phases;
            template.revision = template
                .revision
                .checked_add(1)
                .ok_or(Error::Conflict("template revision exhausted"))?
                .max(1);
            template.updated_at = now.try_clone()?;
            Ok(template.try_clone()?)
        })
    }
    /// Een template waar nog een Job naar verwijst blijft beschikbaar.
    pub fn delete_workflow_template(
        &mut self,
        id: &str,
        operator: &str,
    ) -> Result<d::WorkflowTemplate> {
        let operator = normalized(operator)?;
        self.edit(|state| {
            let template = state.workflow_templates.get(id).ok_or(Error::NotFound)?;
            if template.created_by != operator {
                return Err(Error::Conflict("template belongs to another operator"));
            }
            if state.jobs.iter().any(|(_, job)| job.template_id == id) {
                return Err(Error::Conflict("template is used by a Job"));
            }
            state.workflow_templates.remove(id).ok_or(Error::NotFound)
        })
    }
    /// Controleert de gezamenlijke git/acp-capabilities vóór een Session wordt gemaakt.
    pub fn validate_session_environment(
        &self,
        operator: &str,
        selector: &str,
        with: &[String],
        profile: &str,
    ) -> Result {
        session_environment(&self.state, operator, selector, with, profile)
    }
    /// Projectlagen dragen geen identiteit of eigen control-plane-agent.
    pub fn validate_project_layers(
        &self,
        operator: &str,
        values: &[String],
        profile: &str,
    ) -> Result {
        project_layers(&self.state, operator, values, profile)
    }
}
