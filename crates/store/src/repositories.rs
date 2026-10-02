//! Repositoryconfiguratie bevat selectors en scopes, nooit een vast accounttoken.
use crate::{Context, Error, Persistence, Result, Store};
use crate::{composition::project_layers, git_accounts::remote_host};
use alloc::string::String;
use spin_core::{
    git::{app_services, credential_scope, service_hosts, valid_remote},
    validation::{normalized, selectors, valid_git_base_ref},
};
use spin_domain::{self as d, Timestamp, TryClone, try_string};
fn provider(remote: &str) -> Result<String> {
    let host = remote_host(remote)?;
    Ok(match host.as_str() {
        "github.com" => try_string("github")?,
        "gitlab.com" => try_string("gitlab")?,
        _ => host,
    })
}
impl<P: Persistence> Store<P> {
    /// Een remote is uniek en credential-vrij; de uitvoerende identiteit wordt later gekozen.
    pub fn create_git_repository(
        &mut self,
        req: d::CreateGitRepositoryRequest,
        context: Context<'_>,
    ) -> Result<d::CreateGitRepositoryResponse> {
        context.validate()?;
        let operator = normalized(&req.operator)?;
        let name = req.name.trim();
        let remote = req.remote_url.trim();
        let default = if req.default_ref.trim().is_empty() {
            "main"
        } else {
            req.default_ref.trim()
        };
        if operator.is_empty()
            || name.is_empty()
            || !valid_remote(remote)
            || !valid_git_base_ref(default)
        {
            return Err(Error::Conflict(
                "operator, name, credential-free Git remote and valid default ref are required",
            ));
        }
        let layers = selectors(&req.layer_selectors)?;
        let services = app_services(req.services)?;
        let hosts = service_hosts(&req.service_hosts)?;
        let scope = credential_scope(&req.credential_scope, d::CREDENTIAL_SCOPE_PUBLIC)?;
        self.edit(|state| {
            if state.git_repositories.get(context.id).is_some() {
                return Err(Error::Conflict("repository id already exists"));
            }
            project_layers(state, &operator, &layers, "default")?;
            let compared = normalized(name)?;
            for (_, old) in state.git_repositories.iter() {
                if normalized(&old.name)? == compared || old.remote_url == remote {
                    return Err(Error::Conflict("Git repository already exists"));
                }
            }
            let repository = d::GitRepository {
                id: try_string(context.id)?,
                name: try_string(name)?,
                remote_url: try_string(remote)?,
                default_ref: try_string(default)?,
                provider: provider(remote)?,
                credential_scope: scope,
                layer_selectors: layers,
                services,
                service_hosts: hosts,
                created_by: operator,
                created_at: context.now.try_clone()?,
                updated_at: context.now.try_clone()?,
            };
            state
                .git_repositories
                .insert(repository.id.try_clone()?, repository.try_clone()?)?;
            Ok(d::CreateGitRepositoryResponse { repository })
        })
    }
    /// Lege basisvelden behouden hun waarde; lagen en apprecept worden vervangen.
    pub fn update_git_repository(
        &mut self,
        id: &str,
        req: d::UpdateGitRepositoryRequest,
        now: &Timestamp,
    ) -> Result<d::GitRepository> {
        let operator = normalized(&req.operator)?;
        let scope = credential_scope(&req.credential_scope, "")?;
        let layers = selectors(&req.layer_selectors)?;
        let services = app_services(req.services)?;
        let hosts = service_hosts(&req.service_hosts)?;
        self.edit(|state| {
            let old = state
                .git_repositories
                .get(id.trim())
                .ok_or(Error::NotFound)?;
            if operator.is_empty() || old.created_by != operator {
                return Err(Error::Conflict("repository belongs to another operator"));
            }
            let name = if req.name.trim().is_empty() {
                old.name.as_str()
            } else {
                req.name.trim()
            };
            let remote = if req.remote_url.trim().is_empty() {
                old.remote_url.as_str()
            } else {
                req.remote_url.trim()
            };
            let base = if req.default_ref.trim().is_empty() {
                old.default_ref.as_str()
            } else {
                req.default_ref.trim()
            };
            if name.is_empty() || !valid_remote(remote) || !valid_git_base_ref(base) {
                return Err(Error::Conflict(
                    "name, credential-free remote and valid default ref are required",
                ));
            }
            let compared = normalized(name)?;
            for (_, other) in state.git_repositories.iter() {
                if other.id != old.id
                    && (normalized(&other.name)? == compared || other.remote_url == remote)
                {
                    return Err(Error::Conflict("Git repository already exists"));
                }
            }
            project_layers(state, &operator, &layers, "default")?;
            let name = try_string(name)?;
            let remote = try_string(remote)?;
            let base = try_string(base)?;
            let repository = state
                .git_repositories
                .get_mut(id.trim())
                .ok_or(Error::NotFound)?;
            repository.name = name;
            repository.provider = provider(&remote)?;
            repository.remote_url = remote;
            repository.default_ref = base;
            repository.layer_selectors = layers;
            repository.services = services;
            repository.service_hosts = hosts;
            if !scope.is_empty() {
                repository.credential_scope = scope;
            }
            repository.updated_at = now.try_clone()?;
            Ok(repository.try_clone()?)
        })
    }
    /// Alleen een ongebruikte repository mag verdwijnen; historische Jobs blijven geldig.
    pub fn delete_git_repository(&mut self, id: &str, operator: &str) -> Result<d::GitRepository> {
        let operator = normalized(operator)?;
        self.edit(|state| {
            let repository = state.git_repositories.get(id).ok_or(Error::NotFound)?;
            if operator.is_empty() || repository.created_by != operator {
                return Err(Error::Conflict("repository belongs to another operator"));
            }
            for (_, job) in state.jobs.iter() {
                if job
                    .job_repositories()?
                    .iter()
                    .any(|r| r.repository_id == id)
                {
                    return Err(Error::Conflict("Git repository is used by a Job"));
                }
            }
            state.git_repositories.remove(id).ok_or(Error::NotFound)
        })
    }
}
