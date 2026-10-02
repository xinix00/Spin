//! Git-identiteiten worden bij uitvoering gekozen, niet vast aan een repository gezet.
use crate::configuration::redact_git;
use crate::{Context, Error, Persistence, Result, Store};
use alloc::string::String;
use spin_core::validation::{normalized, text};
use spin_domain::{self as d, TryClone, state::PersistedState, try_string};

pub(crate) fn remote_host(remote: &str) -> Result<String> {
    let remote = remote.trim();
    if let Some(scp) = remote.strip_prefix("git@") {
        return Ok(normalized(scp.split(':').next().unwrap_or_default())?);
    }
    let authority = remote
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    if let Some(ip) = host.strip_prefix('[').and_then(|s| s.split_once(']')) {
        return Ok(normalized(&text(format_args!("{}{}", ip.0, ip.1))?)?);
    }
    Ok(normalized(host)?)
}
pub(crate) fn resolve<'a>(
    state: &'a PersistedState,
    remote: &str,
    provider: &str,
    scope: &str,
    operator: &str,
) -> Result<&'a d::GitAccount> {
    if scope == d::CREDENTIAL_SCOPE_PUBLIC {
        return Err(Error::NotFound);
    }
    let host = remote_host(remote)?;
    let operator = normalized(operator)?;
    let mut selected = None;
    let mut updated = d::Time::default();
    for (_, candidate) in state.git_accounts.iter() {
        if candidate.credential_scope != scope
            || normalized(&candidate.host)? != host
            || (matches!(provider, "github" | "gitlab") && candidate.provider != provider)
            || (scope == d::CREDENTIAL_SCOPE_USER && candidate.operator != operator)
        {
            continue;
        }
        let time = candidate.updated_at.time()?;
        if selected.is_none() || time > updated {
            selected = Some(candidate);
            updated = time;
        }
    }
    selected.ok_or(Error::NotFound)
}
impl<P: Persistence> Store<P> {
    /// Interne vernieuwingslijst; tokens worden nooit aan het publieke snapshot toegevoegd.
    pub fn expiring_git_accounts(&self, now: &d::Timestamp) -> Result<d::List<d::GitAccount>> {
        self.expiring_git_accounts_matching(now, |_| true)
    }
    /// Apply backoff before the page limit so failed accounts cannot starve later ones.
    pub fn expiring_git_accounts_matching(
        &self,
        now: &d::Timestamp,
        mut eligible: impl FnMut(&d::GitAccount) -> bool,
    ) -> Result<d::List<d::GitAccount>> {
        let limit = now.time()?.0.saturating_add(60_000_000_000);
        let mut accounts = d::List::new();
        for (_, account) in self.state.git_accounts.iter() {
            if accounts.len() >= 128 {
                break;
            }
            if !account.refresh_token.is_empty()
                && eligible(account)
                && account
                    .expires_at
                    .as_ref()
                    .is_some_and(|at| at.time().is_ok_and(|at| at.0 <= limit))
            {
                accounts.push(account.try_clone()?)?;
            }
        }
        Ok(accounts)
    }

    /// Een hernieuwde OAuth-login vervangt dezelfde scope/host-identiteit atomair.
    pub fn save_git_account(
        &mut self,
        mut account: d::GitAccount,
        context: Context<'_>,
    ) -> Result<d::GitAccount> {
        context.validate()?;
        account.operator = normalized(&account.operator)?;
        account.provider = normalized(&account.provider)?;
        account.host = normalized(&account.host)?;
        account.provider_id = try_string(account.provider_id.trim())?;
        account.login = try_string(account.login.trim())?;
        account.name = try_string(account.name.trim())?;
        account.email = try_string(account.email.trim())?;
        account.access_token = try_string(account.access_token.trim())?;
        account.refresh_token = try_string(account.refresh_token.trim())?;
        account.token_type = try_string(account.token_type.trim())?;
        account.scope = try_string(account.scope.trim())?;
        account.credential_scope = normalized(&account.credential_scope)?;
        if !matches!(
            account.credential_scope.as_str(),
            d::CREDENTIAL_SCOPE_USER | d::CREDENTIAL_SCOPE_GLOBAL | d::CREDENTIAL_SCOPE_PUBLIC
        ) {
            account.credential_scope = try_string(d::CREDENTIAL_SCOPE_USER)?;
        }
        if account.host.is_empty() {
            account.host = try_string(match account.provider.as_str() {
                "github" => "github.com",
                "gitlab" => "gitlab.com",
                _ => "",
            })?;
        }
        if account.name.is_empty() {
            account.name = account.login.try_clone()?;
        }
        if account.operator.is_empty()
            || account.provider.is_empty()
            || account.host.is_empty()
            || account.login.is_empty()
            || account.access_token.is_empty()
            || !matches!(
                account.credential_scope.as_str(),
                d::CREDENTIAL_SCOPE_USER | d::CREDENTIAL_SCOPE_GLOBAL
            )
        {
            return Err(Error::Conflict(
                "operator, provider, host, login and access token are required",
            ));
        }
        if account
            .host
            .contains(['/', '@', '?', '#', '\r', '\n', '\t', ' '])
        {
            return Err(Error::Conflict(
                "Git account host must be a hostname with optional port",
            ));
        }
        if [
            &account.access_token,
            &account.refresh_token,
            &account.login,
            &account.name,
            &account.email,
        ]
        .iter()
        .any(|s| s.contains(['\r', '\n']))
        {
            return Err(Error::Conflict(
                "Git account values cannot contain newlines",
            ));
        }
        self.edit(|state| {
            let existing = state.git_accounts.iter().find(|(_, old)| {
                old.credential_scope == account.credential_scope
                    && (account.credential_scope == d::CREDENTIAL_SCOPE_GLOBAL
                        || old.operator == account.operator)
                    && old.provider == account.provider
                    && old.host == account.host
            });
            if let Some((_, old)) = existing {
                account.id = old.id.try_clone()?;
                account.created_at = old.created_at.try_clone()?;
            } else {
                if state.git_accounts.get(context.id).is_some() {
                    return Err(Error::Conflict("Git account id already exists"));
                }
                account.id = try_string(context.id)?;
                account.created_at = context.now.try_clone()?;
            }
            account.updated_at = context.now.try_clone()?;
            let public = redact_git(account.try_clone()?);
            state
                .git_accounts
                .insert(account.id.try_clone()?, account)?;
            Ok(public)
        })
    }
    /// Interne runner-ingang; openbare antwoorden moeten de tokens redigeren.
    pub fn git_account(&self, id: &str, operator: &str) -> Result<&d::GitAccount> {
        let operator = normalized(operator)?;
        let account = self
            .state
            .git_accounts
            .get(id.trim())
            .ok_or(Error::NotFound)?;
        if operator.is_empty()
            || (account.credential_scope != d::CREDENTIAL_SCOPE_GLOBAL
                && account.operator != operator)
        {
            return Err(Error::Conflict("Git account belongs to another operator"));
        }
        Ok(account)
    }
    /// Gebruikersscope volgt de uitvoerende operator; global volgt de remotehost.
    pub fn resolve_git_account(&self, repository: &str, operator: &str) -> Result<&d::GitAccount> {
        let repository = self
            .state
            .git_repositories
            .get(repository.trim())
            .ok_or(Error::NotFound)?;
        resolve(
            &self.state,
            &repository.remote_url,
            &repository.provider,
            &repository.credential_scope,
            operator,
        )
    }
    /// Een vastgelegde workspace krijgt actuele credentials bij de uitvoeractie.
    pub fn resolve_git_workspace_account(
        &self,
        workspace: &d::GitWorkspace,
        operator: &str,
    ) -> Result<&d::GitAccount> {
        resolve(
            &self.state,
            &workspace.remote_url,
            &workspace.provider,
            &workspace.credential_scope,
            operator,
        )
    }
    /// Alleen de eigenaar verwijdert een account; legacy live bindings blokkeren dat.
    pub fn delete_git_account(&mut self, id: &str, operator: &str) -> Result<d::GitAccount> {
        let operator = normalized(operator)?;
        let id = id.trim();
        self.edit(|state| {
            let account = state.git_accounts.get(id).ok_or(Error::NotFound)?;
            if operator.is_empty() || account.operator != operator {
                return Err(Error::Conflict("Git account belongs to another operator"));
            }
            if state.compositions.iter().any(|(_, c)| {
                c.git.as_ref().is_some_and(|g| g.account_id == id)
                    && c.runtime.as_ref().is_some_and(|r| r.status != "stopped")
            }) {
                return Err(Error::Conflict(
                    "Git account is used by running composition",
                ));
            }
            Ok(redact_git(
                state.git_accounts.remove(id).ok_or(Error::NotFound)?,
            ))
        })
    }
}
