//! OAuth gebruikt een eenmalige browsergebonden state en PKCE; tokens blijven server-side.
use super::*;
use crate::external::{NetworkRequest, NetworkResponse, request};
use alloc::vec::Vec;
use d::{List, try_string};
use spin_core::validation::text;
use spin_store::Context;

pub(crate) struct Attempt {
    owner: String,
    operator: String,
    provider: String,
    scope: String,
    verifier: String,
    callback: String,
    expires: u64,
}
pub(crate) struct Exchange {
    attempt: Attempt,
    account: d::GitAccount,
}
struct Provider {
    name: &'static str,
    host: &'static str,
    authorize: &'static str,
    token: &'static str,
    user: &'static str,
    scopes: &'static str,
    setup: &'static str,
}
fn provider(id: &str) -> Result<Provider> {
    match id {
        "github" => Ok(Provider {
            name: "GitHub",
            host: "github.com",
            authorize: "https://github.com/login/oauth/authorize",
            token: "https://github.com/login/oauth/access_token",
            user: "https://api.github.com/user",
            scopes: "repo read:user user:email",
            setup: "https://github.com/settings/applications/new",
        }),
        "gitlab" => Ok(Provider {
            name: "GitLab",
            host: "gitlab.com",
            authorize: "https://gitlab.com/oauth/authorize",
            token: "https://gitlab.com/oauth/token",
            user: "https://gitlab.com/api/v4/user",
            scopes: "read_user read_repository write_repository",
            setup: "https://gitlab.com/-/user_settings/applications",
        }),
        _ => Err(Error::Http(404, "unknown OAuth provider")),
    }
}
pub(crate) fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .as_object()
        .and_then(|v| v.get(key))
        .unwrap_or(&Value::Null)
}
fn string(value: &Value, key: &str) -> Result<String> {
    Ok(try_string(field(value, key).as_str().unwrap_or(""))?)
}
pub(crate) fn encode(value: &str) -> Result<String> {
    let mut out = String::new();
    out.try_reserve(
        value
            .len()
            .checked_mul(3)
            .ok_or(Error::Http(413, "URL value too large"))?,
    )
    .map_err(|_| d::Error::OutOfMemory)?;
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[usize::from(byte >> 4)]));
            out.push(char::from(b"0123456789ABCDEF"[usize::from(byte & 15)]));
        }
    }
    Ok(out)
}
fn form(pairs: &[(&str, &str)]) -> Result<Vec<u8>> {
    let mut out = String::new();
    for (key, value) in pairs {
        if !out.is_empty() {
            d::try_push_str(&mut out, "&")?;
        }
        d::try_push_str(
            &mut out,
            &text(format_args!("{}={}", encode(key)?, encode(value)?))?,
        )?;
    }
    Ok(out.into_bytes())
}
pub(crate) fn redirect(location: &str) -> Result<Response> {
    let mut response = Response::empty(302)?;
    response.header("Location", location)?;
    Ok(response)
}
impl<P: Persistence> Server<P> {
    /// Houdt vernieuwbare Git-identiteiten bruikbaar zonder een caller te blokkeren.
    pub fn maintain_network(&mut self, now: &Timestamp, random: &mut impl Runtime) -> Result {
        let time = now.time()?;
        if self
            .refresh_checked
            .is_some_and(|last| time.0.saturating_sub(last.0) < 30_000_000_000)
        {
            return Ok(());
        }
        self.refresh_checked = Some(time);
        self.refresh_after.retain(|_, after| *after > time.0);
        let accounts = self.store.expiring_git_accounts_matching(now, |account| {
            !self.refresh_after.contains_key(&account.id)
                && !self.network.iter().any(|c| matches!(&c.work, crate::external::Work::Refresh(old) if old.id == account.id))
                && self.oauth_config(&account.provider).is_ok()
        })?;
        for account in accounts.into_vec() {
            if self.network.len() >= 28 {
                break;
            }
            if self.refresh_after.contains_key(&account.id)
                || self.network.iter().any(
                    |c| matches!(&c.work,crate::external::Work::Refresh(old) if old.id==account.id),
                )
            {
                continue;
            }
            let Ok((config, _)) = self.oauth_config(&account.provider) else {
                continue;
            };
            let provider = provider(&account.provider)?;
            let body = form(&[
                ("client_id", &config.client_id),
                ("client_secret", &config.client_secret),
                ("grant_type", "refresh_token"),
                ("refresh_token", &account.refresh_token),
            ])?;
            let request = request(
                "POST",
                provider.token,
                "",
                body,
                "application/x-www-form-urlencoded",
            )?;
            self.refresh_after.insert(
                account.id.try_clone()?,
                time.0.saturating_add(60_000_000_000),
            )?;
            self.queue_network(
                request,
                crate::external::Work::Refresh(account),
                now,
                random,
            )?;
        }
        Ok(())
    }
    pub(crate) fn credential_ready(&self, account: &d::GitAccount) -> Result {
        if self
            .network
            .iter()
            .any(|c| matches!(&c.work,crate::external::Work::Refresh(old) if old.id==account.id))
        {
            return Err(Error::Http(503, "Git token renewal in progress"));
        }
        if let (Some(now), Some(expires)) = (self.refresh_checked, &account.expires_at)
            && expires.time()? <= now
        {
            return Err(Error::Http(
                409,
                "Git OAuth token expired; reconnect the account",
            ));
        }
        Ok(())
    }
    pub(crate) fn finish_refresh(
        &mut self,
        index: usize,
        reply: Result<NetworkResponse>,
        now: &Timestamp,
    ) -> Result<Response> {
        let reply = reply?;
        if !(200..300).contains(&reply.status) {
            return Err(Error::Http(502, "Git token renewal failed"));
        }
        let value = Value::from_json(&reply.body)?;
        let crate::external::Work::Refresh(old) = &self.network[index].work else {
            return Err(Error::Http(500, "missing token renewal"));
        };
        let current = self.store.git_account(&old.id, &old.operator)?;
        // Handmatige reconnect of verwijdering wint van een ouder, trager antwoord.
        if current.refresh_token != old.refresh_token
            || current.updated_at.time()? != old.updated_at.time()?
        {
            return Response::empty(204);
        }
        let mut account = old.try_clone()?;
        let token = string(&value, "access_token")?;
        if token.is_empty() || !field(&value, "error").as_str().unwrap_or("").is_empty() {
            return Err(Error::Http(502, "Git token grant rejected"));
        }
        account.access_token = token;
        let refresh = string(&value, "refresh_token")?;
        if !refresh.is_empty() {
            account.refresh_token = refresh;
        }
        account.token_type = string(&value, "token_type")?;
        account.scope = string(&value, "scope")?;
        account.expires_at = match field(&value, "expires_in").as_i64().filter(|n| *n > 0) {
            Some(seconds) => Some(Timestamp::from_time(d::Time(
                now.time()?
                    .0
                    .saturating_add((seconds as u64).saturating_mul(1_000_000_000)),
            ))?),
            None => None,
        };
        let id = account.id.try_clone()?;
        self.store
            .save_git_account(account, Context { now, id: &id })?;
        self.last_launch_sweep = None;
        Response::empty(204)
    }
    /// Expliciete publieke origin voor cookies en OAuth-callbacks, vóór de listener start.
    pub fn set_public_url(&mut self, url: &str) -> Result {
        let url = url.trim_end_matches('/');
        if !url.is_empty()
            && (!spin_core::git::valid_remote(url)
                || !(url.starts_with("http://") || url.starts_with("https://")))
        {
            return Err(Error::Http(500, "invalid public URL"));
        }
        self.public_url = try_string(url)?;
        Ok(())
    }
    /// Omgevingscredentials hebben dezelfde voorrang als in Go.
    pub fn set_oauth_environment(&mut self, id: &str, client: &str, secret: &str) -> Result {
        provider(id)?;
        if client.is_empty() || secret.is_empty() {
            return Ok(());
        }
        self.oauth_env.insert(
            try_string(id)?,
            d::GitOAuthConfiguration {
                client_id: try_string(client)?,
                client_secret: try_string(secret)?,
                ..Default::default()
            },
        )?;
        Ok(())
    }
    fn oauth_config(&self, id: &str) -> Result<(&d::GitOAuthConfiguration, &'static str)> {
        if let Some(config) = self.oauth_env.get(id) {
            return Ok((config, "environment"));
        }
        Ok((self.store.git_oauth_configuration(id)?, "app"))
    }
    pub(crate) fn oauth_providers(&self) -> Result<Value> {
        let mut items = List::new();
        for id in ["github", "gitlab"] {
            let provider = provider(id)?;
            let config = self.oauth_config(id).ok();
            items.push(http::object(&[
                ("id", Value::string(id)?),
                ("name", Value::string(provider.name)?),
                ("configured", Value::Bool(config.is_some())),
                (
                    "source",
                    Value::string(config.map_or("", |(_, source)| source))?,
                ),
                (
                    "client_id",
                    Value::string(config.map_or("", |(config, _)| config.client_id.as_str()))?,
                ),
                (
                    "callback_url",
                    Value::string(&text(format_args!(
                        "{}/api/git/oauth/{id}/callback",
                        self.public_url
                    ))?)?,
                ),
                ("setup_url", Value::string(provider.setup)?),
            ])?)?;
        }
        Ok(items.to_value()?)
    }
    pub(crate) fn oauth_route(
        &mut self,
        req: &Request<'_>,
        user: &d::User,
        owner: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        let mut path = req.path.trim_start_matches('/').split('/');
        let (Some("api"), Some("git"), Some("oauth"), Some(id), Some(route), None) = (
            path.next(),
            path.next(),
            path.next(),
            path.next(),
            path.next(),
            path.next(),
        ) else {
            return Ok(None);
        };
        if req.method != "GET" || !matches!(route, "start" | "callback") {
            return Ok(None);
        }
        let provider = provider(id)?;
        let time = now.time()?.0;
        self.oauth_attempts.retain(|_, a| a.expires > time);
        if route == "start" {
            let mut scope = req.query("credential_scope")?;
            if scope.is_empty() {
                scope = try_string(d::CREDENTIAL_SCOPE_USER)?;
            }
            if !matches!(
                scope.as_str(),
                d::CREDENTIAL_SCOPE_USER | d::CREDENTIAL_SCOPE_GLOBAL
            ) {
                return Err(Error::Http(400, "invalid Git credential scope"));
            }
            if scope == d::CREDENTIAL_SCOPE_GLOBAL && user.role != d::USER_ADMIN {
                return Err(Error::Http(403, "admin role required"));
            }
            let client = self.oauth_config(id)?.0.client_id.try_clone()?;
            if self.oauth_attempts.len() >= 128 {
                return Err(Error::Http(503, "OAuth attempt capacity reached"));
            }
            let state = auth::token(random)?;
            let verifier = auth::token(random)?;
            let hash = spin_security::sha256(verifier.as_bytes());
            let mut challenge = spin_security::encode_base64(&hash, false)?;
            // De bron is ASCII en de vervangingen veranderen de lengte niet.
            let bytes = challenge.into_bytes();
            let mut urlsafe = Vec::new();
            urlsafe
                .try_reserve_exact(bytes.len())
                .map_err(|_| d::Error::OutOfMemory)?;
            for byte in bytes {
                urlsafe.push(match byte {
                    b'+' => b'-',
                    b'/' => b'_',
                    other => other,
                });
            }
            challenge = String::from_utf8(urlsafe)
                .map_err(|_| Error::Http(500, "invalid PKCE encoding"))?;
            let base = if self.public_url.is_empty() {
                let host = req.header("Host");
                if host.is_empty()
                    || host.contains(['/', '@', '?', '#'])
                    || host.bytes().any(|c| c <= 32 || c == 127)
                {
                    return Err(Error::Http(400, "invalid callback host"));
                }
                text(format_args!(
                    "{}://{host}",
                    if req.secure { "https" } else { "http" }
                ))?
            } else {
                self.public_url.try_clone()?
            };
            let callback = text(format_args!("{base}/api/git/oauth/{id}/callback"))?;
            let query = form(&[
                ("client_id", &client),
                ("redirect_uri", &callback),
                ("response_type", "code"),
                ("scope", provider.scopes),
                ("state", &state),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("prompt", "select_account"),
            ])?;
            let location = text(format_args!(
                "{}?{}",
                provider.authorize,
                core::str::from_utf8(&query)
                    .map_err(|_| Error::Http(500, "invalid OAuth query"))?
            ))?;
            self.oauth_attempts.insert(
                spin_security::digest_hex(state.as_bytes())?,
                Attempt {
                    owner: try_string(owner)?,
                    operator: user.username.try_clone()?,
                    provider: try_string(id)?,
                    scope,
                    verifier,
                    callback,
                    expires: time.saturating_add(600_000_000_000),
                },
            )?;
            return Ok(Some(Outcome::Response(redirect(&location)?)));
        }
        let state = req.query("state")?;
        let key = spin_security::digest_hex(state.as_bytes())?;
        let attempt = self
            .oauth_attempts
            .get(&key)
            .filter(|a| a.provider == id && a.owner == owner)
            .ok_or(Error::Http(409, "OAuth state invalid or expired"))?;
        if attempt.scope == d::CREDENTIAL_SCOPE_GLOBAL && user.role != d::USER_ADMIN {
            return Err(Error::Http(403, "admin role required"));
        }
        let attempt = self
            .oauth_attempts
            .remove(&key)
            .ok_or(Error::Http(409, "OAuth state already used"))?;
        if !req.query("error")?.is_empty() {
            return Ok(Some(Outcome::Response(redirect("/?git_oauth=denied#git")?)));
        }
        let code = req.query("code")?;
        if code.is_empty() || code.len() > 8192 {
            return Err(Error::Http(400, "OAuth code required"));
        }
        let config = self.oauth_config(id)?.0;
        let body = form(&[
            ("client_id", &config.client_id),
            ("client_secret", &config.client_secret),
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &attempt.callback),
            ("code_verifier", &attempt.verifier),
        ])?;
        let request = request(
            "POST",
            provider.token,
            "",
            body,
            "application/x-www-form-urlencoded",
        )?;
        let wait = self.queue_network(
            request,
            crate::external::Work::OAuth(Exchange {
                attempt,
                account: Default::default(),
            }),
            now,
            random,
        )?;
        Ok(Some(Outcome::Network(wait)))
    }
    pub(crate) fn advance_oauth(
        &mut self,
        index: usize,
        reply: Result<NetworkResponse>,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let reply = reply?;
        if !(200..300).contains(&reply.status) {
            return Err(Error::Http(502, "OAuth provider rejected request"));
        }
        let value = Value::from_json(&reply.body)?;
        let call = &mut self.network[index];
        let crate::external::Work::OAuth(work) = &mut call.work else {
            return Err(Error::Http(500, "missing OAuth exchange"));
        };
        let provider = provider(&work.attempt.provider)?;
        if work.account.access_token.is_empty() {
            let token = string(&value, "access_token")?;
            if token.is_empty() || !field(&value, "error").as_str().unwrap_or("").is_empty() {
                return Err(Error::Http(502, "OAuth grant rejected"));
            }
            work.account = d::GitAccount {
                operator: work.attempt.operator.try_clone()?,
                provider: work.attempt.provider.try_clone()?,
                host: try_string(provider.host)?,
                credential_scope: work.attempt.scope.try_clone()?,
                access_token: token,
                refresh_token: string(&value, "refresh_token")?,
                token_type: string(&value, "token_type")?,
                scope: string(&value, "scope")?,
                ..Default::default()
            };
            if let Some(seconds) = field(&value, "expires_in").as_i64().filter(|n| *n > 0) {
                work.account.expires_at = Some(Timestamp::from_time(d::Time(
                    now.time()?
                        .0
                        .saturating_add((seconds as u64).saturating_mul(1_000_000_000)),
                ))?);
            }
            let mut request: NetworkRequest = request(
                "GET",
                provider.user,
                &work.account.access_token,
                Vec::new(),
                "",
            )?;
            request.id = call.id.try_clone()?;
            call.request = Some(request);
            return Ok(None);
        }
        let id = field(&value, "id")
            .as_i64()
            .filter(|n| *n > 0)
            .ok_or(Error::Http(502, "OAuth identity has no id"))?;
        let mut login = string(&value, "login")?;
        if login.is_empty() {
            login = string(&value, "username")?;
        }
        if login.is_empty() {
            return Err(Error::Http(502, "OAuth identity has no login"));
        }
        let account = &mut work.account;
        account.provider_id = text(format_args!("{id}"))?;
        account.login = login;
        account.name = string(&value, "name")?;
        account.email = string(&value, "commit_email")?;
        if account.email.is_empty() {
            account.email = string(&value, "email")?;
        }
        if account.email.is_empty() && account.provider == "github" {
            account.email = text(format_args!("{}@users.noreply.github.com", account.login))?;
        }
        let (user, _) = self.store.authenticate_session(&work.attempt.owner, now)?;
        if user.username != work.attempt.operator
            || (work.attempt.scope == d::CREDENTIAL_SCOPE_GLOBAL && user.role != d::USER_ADMIN)
        {
            return Err(Error::Http(403, "OAuth authorization was revoked"));
        }
        let account = account.try_clone()?;
        let id = random.next("git")?;
        self.store
            .save_git_account(account, Context { now, id: &id })?;
        self.last_launch_sweep = None;
        Ok(Some(redirect("/?git_oauth=connected#git")?))
    }
}
