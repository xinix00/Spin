//! Operatorgebonden MCP-credentials en admin-only OAuth-instellingen.
use crate::{Context, Error, Persistence, Result, Store, require_admin};
use alloc::string::String;
use spin_core::validation::normalized;
use spin_domain::{self as d, List, Timestamp, TryClone, try_string};

pub(crate) fn redact_mcp(mut server: d::MCPServer) -> d::MCPServer {
    for secret in server
        .env
        .as_mut_slice()
        .iter_mut()
        .chain(server.headers.as_mut_slice())
    {
        secret.value.clear();
    }
    // encoding/json schrijft deze geredigeerde lijsten altijd als arrays.
    if server.env.is_empty() {
        server.env = List::new();
    }
    if server.headers.is_empty() {
        server.headers = List::new();
    }
    server
}
pub(crate) fn redact_git(mut account: d::GitAccount) -> d::GitAccount {
    account.access_token.clear();
    account.refresh_token.clear();
    account
}
fn secrets(values: List<d::MCPSecret>) -> Result<List<d::MCPSecret>> {
    let mut seen = d::Map::new();
    let mut out = List::new();
    for mut value in values.into_vec() {
        value.name = try_string(value.name.trim())?;
        let key = normalized(&value.name)?;
        if key.is_empty() || seen.contains_key(&key) {
            return Err(Error::Conflict(
                "MCP credential names must be non-empty and unique",
            ));
        }
        seen.insert(key, true)?;
        out.push(value)?;
    }
    Ok(out)
}
impl<P: Persistence> Store<P> {
    /// Slaat credentials op en geeft uitsluitend de geredigeerde configuratie terug.
    pub fn create_mcp_server(
        &mut self,
        mut req: d::CreateMCPServerRequest,
        context: Context<'_>,
    ) -> Result<d::MCPServer> {
        context.validate()?;
        let operator = normalized(&req.operator)?;
        let name = try_string(req.name.trim())?;
        if operator.is_empty() || name.is_empty() {
            return Err(Error::Conflict("operator and MCP name are required"));
        }
        if req.transport.is_empty() {
            req.transport = try_string(d::MCP_TRANSPORT_STDIO)?;
        }
        match req.transport.as_str() {
            d::MCP_TRANSPORT_STDIO if req.command.trim().starts_with('/') => {}
            d::MCP_TRANSPORT_HTTP
                if {
                    let url = normalized(&req.url)?;
                    url.starts_with("http://") || url.starts_with("https://")
                } => {}
            _ => {
                return Err(Error::Conflict(
                    "MCP needs an absolute stdio command or an http/https URL",
                ));
            }
        }
        let server = d::MCPServer {
            id: try_string(context.id)?,
            operator,
            name,
            transport: req.transport,
            command: try_string(req.command.trim())?,
            args: req.args,
            url: try_string(req.url.trim())?,
            env: secrets(req.env)?,
            headers: secrets(req.headers)?,
            created_at: context.now.try_clone()?,
        };
        self.edit(|state| {
            if state.mcp_servers.get(context.id).is_some() {
                return Err(Error::Conflict("MCP id already exists"));
            }
            let name = normalized(&server.name)?;
            for (_, old) in state.mcp_servers.iter() {
                if old.operator == server.operator && normalized(&old.name)? == name {
                    return Err(Error::Conflict("MCP server already exists for operator"));
                }
            }
            let public = redact_mcp(server.try_clone()?);
            state.mcp_servers.insert(server.id.try_clone()?, server)?;
            Ok(public)
        })
    }
    /// Verwijderen ruimt Job/Session-selecties op; live capsules blokkeren de mutatie.
    pub fn delete_mcp_server(&mut self, id: &str, operator: &str) -> Result<d::MCPServer> {
        let operator = normalized(operator)?;
        self.edit(|state| {
            let server = state.mcp_servers.get(id).ok_or(Error::NotFound)?;
            if operator.is_empty() || server.operator != operator {
                return Err(Error::Conflict("MCP belongs to another operator"));
            }
            if state.compositions.iter().any(|(_, c)| {
                c.mcp_server_ids.iter().any(|m| m == id)
                    && c.runtime.as_ref().is_some_and(|r| r.status != "stopped")
            }) {
                return Err(Error::Conflict("MCP server is used by running composition"));
            }
            let server = state.mcp_servers.remove(id).ok_or(Error::NotFound)?;
            for (_, job) in state.jobs.iter_mut() {
                job.mcp_server_ids.retain(|v| v != id);
            }
            for (_, session) in state.sessions.iter_mut() {
                session.mcp_server_ids.retain(|v| v != id);
            }
            Ok(redact_mcp(server))
        })
    }
    /// Alleen de runner-ingang krijgt de volledige credentials van deze operator.
    pub fn mcp_servers_for_operator(
        &self,
        operator: &str,
        ids: &[String],
    ) -> Result<List<d::MCPServer>> {
        let operator = normalized(operator)?;
        let mut out = List::<d::MCPServer>::new();
        for id in ids {
            let id = id.trim();
            if id.is_empty() || out.iter().any(|s| s.id == id) {
                continue;
            }
            let server = self
                .state
                .mcp_servers
                .get(id)
                .filter(|s| s.operator == operator)
                .ok_or(Error::NotFound)?;
            out.push(server.try_clone()?)?;
        }
        Ok(out)
    }
    /// Alleen een actieve admin mag OAuth-appcredentials opslaan.
    pub fn save_git_oauth_configuration(
        &mut self,
        actor: &str,
        req: d::SaveGitOAuthConfigurationRequest,
        now: &Timestamp,
    ) -> Result<d::GitOAuthConfiguration> {
        let provider = normalized(&req.provider)?;
        let client = req.client_id.trim();
        let secret = req.client_secret.trim();
        if !matches!(provider.as_str(), "github" | "gitlab")
            || client.is_empty()
            || secret.is_empty()
            || client.contains(['\r', '\n'])
            || secret.contains(['\r', '\n'])
        {
            return Err(Error::Conflict(
                "github/gitlab provider, client ID and client secret are required",
            ));
        }
        self.edit(|state| {
            require_admin(state, actor)?;
            let mut config = state
                .git_oauth_configurations
                .get(&provider)
                .map(TryClone::try_clone)
                .transpose()?
                .unwrap_or_default();
            if config.created_at.time()?.is_zero() {
                config.created_at = now.try_clone()?;
                config.created_by = state
                    .users
                    .get(actor.trim())
                    .ok_or(Error::NotFound)?
                    .username
                    .try_clone()?;
            }
            config.provider = provider.try_clone()?;
            config.client_id = try_string(client)?;
            config.client_secret = try_string(secret)?;
            config.updated_at = now.try_clone()?;
            state
                .git_oauth_configurations
                .insert(provider, config.try_clone()?)?;
            config.client_secret.clear();
            Ok(config)
        })
    }
    /// Interne OAuth-ingang; bevat het geheim voor de tokenuitwisseling.
    pub fn git_oauth_configuration(&self, provider: &str) -> Result<&d::GitOAuthConfiguration> {
        self.state
            .git_oauth_configurations
            .get(&normalized(provider)?)
            .ok_or(Error::NotFound)
    }
    /// Een admin trekt de configuratie in; de respons bevat geen clientsecret.
    pub fn delete_git_oauth_configuration(
        &mut self,
        actor: &str,
        provider: &str,
    ) -> Result<d::GitOAuthConfiguration> {
        let provider = normalized(provider)?;
        self.edit(|state| {
            require_admin(state, actor)?;
            let mut config = state
                .git_oauth_configurations
                .remove(&provider)
                .ok_or(Error::NotFound)?;
            config.client_secret.clear();
            Ok(config)
        })
    }
    /// Publieke configuratielijst, alfabetisch per provider.
    pub fn git_oauth_configurations(&self) -> Result<List<d::GitOAuthConfiguration>> {
        let mut out = List::new();
        for (_, config) in self.state.git_oauth_configurations.iter() {
            let mut config = config.try_clone()?;
            config.client_secret.clear();
            out.push(config)?;
        }
        out.as_mut_slice()
            .sort_unstable_by(|a, b| a.provider.cmp(&b.provider));
        Ok(out)
    }
}
