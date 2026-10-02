//! Cookieauthenticatie, CSRF en begrensde wachtwoordtaken zonder Store-lening.
use super::*;
use spin_core::validation::text;
use spin_domain::{Time, try_push_str, try_string};
use spin_security::{
    MAX_PASSWORD_ITERATIONS, PASSWORD_ITERATIONS, PasswordDeriver, constant_time_eq, decode_base64,
    digest_hex, encode_base64,
};
use spin_store::Context;
const CACHE_CAP: usize = 4096;
const SECOND: u64 = 1_000_000_000;
const SESSION_SECONDS: u64 = 24 * 60 * 60;
pub(super) struct Csrf {
    value: String,
    expires: Time,
}
pub(super) struct Attempt {
    failures: u32,
    start: Time,
    blocked: Time,
}
enum Purpose {
    Setup(d::User),
    Login(Option<d::User>),
    Create {
        user: d::User,
        admin_token: String,
    },
    Reset {
        user_id: String,
        admin_token: String,
    },
}
/// De runtime geeft dit werk per ronde hoogstens een vast CPU-budget.
pub struct PasswordWork {
    task: PasswordDeriver,
    expected: [u8; 32],
    salt: [u8; 16],
    purpose: Purpose,
    peer: String,
    secure: bool,
}
impl PasswordWork {
    /// Verbruikt maximaal `rounds` PBKDF2-rondes; `true` betekent gereed.
    pub fn step(&mut self, rounds: u32) -> bool {
        self.task.step(rounds)
    }
}
fn after(now: &Timestamp, seconds: u64) -> Result<Timestamp> {
    Ok(Timestamp::from_time(Time(
        now.time()?
            .0
            .checked_add(
                seconds
                    .checked_mul(SECOND)
                    .ok_or(Error::Http(500, "time overflow"))?,
            )
            .ok_or(Error::Http(500, "time overflow"))?,
    ))?)
}
fn public(user: &d::User) -> d::Fallible<d::PublicUser> {
    Ok(d::PublicUser {
        id: user.id.try_clone()?,
        username: user.username.try_clone()?,
        display_name: user.display_name.try_clone()?,
        role: user.role.try_clone()?,
        archived_at: user.archived_at.try_clone()?,
        created_at: user.created_at.try_clone()?,
    })
}
pub(super) fn origin(req: &Request<'_>) -> bool {
    let origin = req.header("Origin").trim();
    if origin.is_empty() {
        return true;
    }
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    matches!(scheme, "http" | "https")
        && !authority.is_empty()
        && !authority.contains(['/', '?', '#', '@', '\\'])
        && authority.eq_ignore_ascii_case(req.header("Host"))
}
pub(super) fn check_csrf(req: &Request<'_>, session: &d::AuthSession) -> Result {
    let hash = digest_hex(req.header("X-Spin-CSRF").as_bytes())?;
    if !origin(req) || !constant_time_eq(hash.as_bytes(), session.csrf_hash.as_bytes()) {
        return Err(Error::Http(403, "invalid CSRF token or request origin"));
    }
    Ok(())
}
pub(crate) fn token(runtime: &mut impl Runtime) -> Result<String> {
    let mut bytes = [0; 32];
    runtime.fill(&mut bytes)?;
    let raw = encode_base64(&bytes, false)?;
    let mut value = String::new();
    for c in raw.chars() {
        let c = match c {
            '+' => '-',
            '/' => '_',
            other => other,
        };
        let mut buf = [0; 4];
        try_push_str(&mut value, c.encode_utf8(&mut buf))?;
    }
    Ok(value)
}
fn password_work(
    password: &str,
    purpose: Purpose,
    req: &Request<'_>,
    runtime: &mut impl Runtime,
) -> Result<PasswordWork> {
    let mut salt = [0; 16];
    runtime.fill(&mut salt)?;
    if !(12..=256).contains(&password.len()) {
        return Err(Error::Security(spin_security::Error::PasswordLength(
            password.len(),
        )));
    }
    Ok(PasswordWork {
        task: PasswordDeriver::new(password.as_bytes(), &salt, PASSWORD_ITERATIONS)?,
        expected: [0; 32],
        salt,
        purpose,
        peer: try_string(req.peer)?,
        secure: req.secure,
    })
}
fn verifier(password: &str, hash: &str) -> Result<Option<(PasswordDeriver, [u8; 32])>> {
    if password.len() > 256 || hash.len() > 1024 {
        return Ok(None);
    }
    let mut parts = hash.split('$');
    if parts.next() != Some("pbkdf2-sha256") {
        return Ok(None);
    }
    let Some(rounds) = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n > 0 && *n <= MAX_PASSWORD_ITERATIONS)
    else {
        return Ok(None);
    };
    let (Some(salt), Some(expected)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    if parts.next().is_some() || salt.contains('=') || expected.contains('=') {
        return Ok(None);
    }
    let salt = match decode_base64(salt) {
        Ok(salt) => salt,
        Err(spin_security::Error::Payload) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let expected = match decode_base64(expected) {
        Ok(hash) => hash,
        Err(spin_security::Error::Payload) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Ok(expected) = expected.as_slice().try_into() else {
        return Ok(None);
    };
    Ok(Some((
        PasswordDeriver::new(password.as_bytes(), &salt, rounds)?,
        expected,
    )))
}
impl<P: Persistence> Server<P> {
    pub(super) fn identity(
        &mut self,
        req: &Request<'_>,
        now: &Timestamp,
    ) -> Result<Option<(d::User, d::AuthSession)>> {
        for name in ["__Host-spin_session", "spin_session"] {
            for (_, value) in req
                .headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case("Cookie"))
            {
                for cookie in value.split(';') {
                    let Some((key, value)) = cookie.trim().split_once('=') else {
                        continue;
                    };
                    if key != name || value.is_empty() {
                        continue;
                    }
                    match self
                        .store
                        .authenticate_session(&digest_hex(value.as_bytes())?, now)
                    {
                        Ok(identity) => return Ok(Some(identity)),
                        Err(spin_store::Error::NotFound) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        Ok(None)
    }
    fn status(&self, user: Option<&d::User>, csrf: &str) -> Result<Value> {
        let mut out = d::json::Object::default();
        out.push("version", Value::string(env!("CARGO_PKG_VERSION"))?)?;
        out.push("chunked_attachments", Value::Bool(true))?;
        out.push("configured", Value::Bool(self.store.has_users()))?;
        out.push("authenticated", Value::Bool(user.is_some()))?;
        if let Some(user) = user {
            out.push("user", public(user)?.to_value()?)?;
            out.push("csrf_token", Value::string(csrf)?)?;
        }
        Ok(Value::Object(out))
    }
    fn issue(
        &mut self,
        user: &d::User,
        secure: bool,
        status: u16,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Response> {
        let secure = secure || self.public_url.starts_with("https://");
        let current = now.time()?;
        self.csrf.retain(|_, c| c.expires > current);
        if self.csrf.len() >= CACHE_CAP {
            return Err(Error::Http(503, "session capacity reached"));
        }
        let id = runtime.next("auth")?;
        let session_token = token(runtime)?;
        let csrf = token(runtime)?;
        let expires = after(now, SESSION_SECONDS)?;
        // Reserveer de vluchtige token vóór de duurzame sessie, zodat een OOM
        // nooit een onbruikbare, wel opgeslagen login publiceert.
        self.csrf.insert(
            id.try_clone()?,
            Csrf {
                value: csrf.try_clone()?,
                expires: expires.time()?,
            },
        )?;
        if let Err(e) = self.store.create_auth_session(
            &user.id,
            &digest_hex(session_token.as_bytes())?,
            &digest_hex(csrf.as_bytes())?,
            expires,
            Context { now, id: &id },
        ) {
            self.csrf.remove(&id);
            return Err(e.into());
        }
        let mut response = Response::json(status, &self.status(Some(user), &csrf)?)?;
        let name = if secure {
            "__Host-spin_session"
        } else {
            "spin_session"
        };
        response.header(
            "Set-Cookie",
            &text(format_args!(
                "{name}={session_token}; Path=/; Max-Age={SESSION_SECONDS}; HttpOnly; SameSite=Lax{}",
                if secure { "; Secure" } else { "" }
            ))?,
        )?;
        Ok(response)
    }
    fn failure(&mut self, peer: &str, now: &Timestamp) -> Result {
        let current = now.time()?;
        if let Some(attempt) = self.attempts.get_mut(peer) {
            if current.0.saturating_sub(attempt.start.0) > 300 * SECOND {
                attempt.failures = 0;
                attempt.start = current;
            }
            attempt.failures = attempt.failures.saturating_add(1);
            if attempt.failures >= 5 {
                attempt.blocked = after(now, 60)?.time()?;
            }
        } else {
            if self.attempts.len() >= CACHE_CAP {
                return Err(Error::Http(503, "login capacity reached"));
            }
            self.attempts.insert(
                try_string(peer)?,
                Attempt {
                    failures: 1,
                    start: current,
                    blocked: Time::default(),
                },
            )?;
        }
        Ok(())
    }
    pub(super) fn auth_request(
        &mut self,
        req: Request<'_>,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Outcome> {
        match (req.method, req.path) {
            ("GET", "/api/auth/status") => {
                if let Some((user, session)) = self.identity(&req, now)? {
                    if let Some(csrf) = self.csrf.get(&session.id) {
                        return Ok(Outcome::Response(Response::json(
                            200,
                            &self.status(Some(&user), &csrf.value)?,
                        )?));
                    }
                    // Plaintext CSRF leeft niet op schijf. Na herstart krijgt de
                    // bestaande gebruiker transparant een nieuwe browsersessie.
                    let response = self.issue(&user, req.secure, 200, now, runtime)?;
                    self.store.delete_auth_session(&session.id)?;
                    return Ok(Outcome::Response(response));
                }
                Ok(Outcome::Response(Response::json(
                    200,
                    &self.status(None, "")?,
                )?))
            }
            ("POST", "/api/auth/setup") => {
                if !origin(&req) {
                    return Err(Error::Http(403, "invalid request origin"));
                }
                if self.store.has_users() {
                    return Err(Error::Http(409, "owner setup is already complete"));
                }
                let value: d::SetupUserRequest = Self::decode(&req)?;
                let user = d::User {
                    username: value.username,
                    display_name: value.display_name,
                    ..Default::default()
                };
                Ok(Outcome::Password(password_work(
                    &value.password,
                    Purpose::Setup(user),
                    &req,
                    runtime,
                )?))
            }
            ("POST", "/api/auth/login") => {
                if !origin(&req) {
                    return Err(Error::Http(403, "invalid request origin"));
                }
                if req.peer.len() > 128 {
                    return Err(Error::Http(400, "invalid peer address"));
                }
                let current = now.time()?;
                self.attempts.retain(|_, a| {
                    current.0.saturating_sub(a.start.0) <= 300 * SECOND || current < a.blocked
                });
                if let Some(a) = self.attempts.get(req.peer).filter(|a| a.blocked > current) {
                    let mut response =
                        Error::Http(429, "too many login attempts; try again shortly")
                            .response()?;
                    response.header(
                        "Retry-After",
                        &text(format_args!("{}", (a.blocked.0 - current.0) / SECOND + 1))?,
                    )?;
                    return Ok(Outcome::Response(response));
                }
                if self.attempts.len() >= CACHE_CAP && !self.attempts.contains_key(req.peer) {
                    return Err(Error::Http(503, "login capacity reached"));
                }
                let value: d::LoginRequest = Self::decode(&req)?;
                let mut user = match self.store.user_by_username(&value.username) {
                    Ok(user) => Some(user.try_clone()?),
                    Err(spin_store::Error::NotFound) => None,
                    Err(e) => return Err(e.into()),
                };
                let parsed = verifier(
                    &value.password,
                    user.as_ref().map_or("", |u| u.password_hash.as_str()),
                )?;
                let (task, expected) = if let Some(parsed) = parsed {
                    parsed
                } else {
                    user = None;
                    // Onbekende gebruikers doen evenveel werk als een nieuwe hash.
                    (
                        PasswordDeriver::new(
                            b"not-a-real-password",
                            &[0; 16],
                            PASSWORD_ITERATIONS,
                        )?,
                        [0; 32],
                    )
                };
                Ok(Outcome::Password(PasswordWork {
                    task,
                    expected,
                    salt: [0; 16],
                    purpose: Purpose::Login(user),
                    peer: try_string(req.peer)?,
                    secure: req.secure,
                }))
            }
            _ => {
                let (user, session) = self
                    .identity(&req, now)?
                    .ok_or(Error::Http(401, "authentication required"))?;
                if req.is_mutation() {
                    check_csrf(&req, &session)?;
                }
                if req.method == "POST" && req.path == "/api/auth/logout" {
                    self.store.delete_auth_session(&session.id)?;
                    self.csrf.remove(&session.id);
                    let mut response = Response::empty(204)?;
                    for name in ["__Host-spin_session", "spin_session"] {
                        response.header(
                            "Set-Cookie",
                            &text(format_args!(
                                "{name}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{}",
                                if name.starts_with("__Host-") {
                                    "; Secure"
                                } else {
                                    ""
                                }
                            ))?,
                        )?;
                    }
                    return Ok(Outcome::Response(response));
                }
                if user.role != d::USER_ADMIN {
                    return Err(Error::Http(403, "admin role required"));
                }
                if req.method == "POST" && req.path == "/api/auth/users" {
                    let value: d::CreateUserRequest = Self::decode(&req)?;
                    let user = d::User {
                        username: value.username,
                        display_name: value.display_name,
                        role: value.role,
                        ..Default::default()
                    };
                    return Ok(Outcome::Password(password_work(
                        &value.password,
                        Purpose::Create {
                            user,
                            admin_token: session.token_hash,
                        },
                        &req,
                        runtime,
                    )?));
                }
                if req.method == "POST"
                    && let Some(path) = req.path.strip_prefix("/api/auth/users/")
                    && let Some((id, action)) = path.split_once('/')
                {
                    match action {
                        "archive" | "restore" => {
                            return Ok(Outcome::Response(Response::json(
                                200,
                                &self.store.set_user_archived(
                                    &user.id,
                                    id,
                                    action == "archive",
                                    now,
                                )?,
                            )?));
                        }
                        "password" => {
                            let value = d::json::parse(req.body)?;
                            let password = value
                                .as_object()
                                .and_then(|o| o.get("password"))
                                .and_then(Value::as_str)
                                .ok_or(Error::Http(400, "password is required"))?;
                            return Ok(Outcome::Password(password_work(
                                password,
                                Purpose::Reset {
                                    user_id: try_string(id)?,
                                    admin_token: session.token_hash,
                                },
                                &req,
                                runtime,
                            )?));
                        }
                        _ => {}
                    }
                }
                Err(Error::Http(404, "not found"))
            }
        }
    }
    pub(super) fn finish_auth(
        &mut self,
        work: PasswordWork,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Response> {
        let actual = work
            .task
            .result()
            .ok_or(Error::Http(409, "password work is not complete"))?;
        match work.purpose {
            Purpose::Login(user) => {
                let user = user.filter(|u| {
                    constant_time_eq(&actual, &work.expected) && u.archived_at.is_none()
                });
                let user = if let Some(user) = user {
                    match self.store.user(&user.id) {
                        Ok(current)
                            if current.archived_at.is_none()
                                && current.password_hash == user.password_hash =>
                        {
                            Some(current.try_clone()?)
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                let Some(user) = user else {
                    self.failure(&work.peer, now)?;
                    return Err(Error::Http(401, "invalid username or password"));
                };
                self.attempts.remove(&work.peer);
                self.issue(&user, work.secure, 200, now, runtime)
            }
            purpose => {
                let hash = text(format_args!(
                    "pbkdf2-sha256${PASSWORD_ITERATIONS}${}${}",
                    encode_base64(&work.salt, false)?,
                    encode_base64(&actual, false)?
                ))?;
                match purpose {
                    Purpose::Setup(mut user) => {
                        user.password_hash = hash;
                        let id = runtime.next("usr")?;
                        let created = self
                            .store
                            .create_initial_user(user, Context { now, id: &id })?;
                        let user = self.store.user(&created.id)?.try_clone()?;
                        self.issue(&user, work.secure, 201, now, runtime)
                    }
                    Purpose::Create {
                        mut user,
                        admin_token,
                    } => {
                        let (admin, _) = self.store.authenticate_session(&admin_token, now)?;
                        if admin.role != d::USER_ADMIN {
                            return Err(Error::Http(403, "admin role required"));
                        }
                        user.password_hash = hash;
                        let id = runtime.next("usr")?;
                        Response::json(
                            201,
                            &self
                                .store
                                .create_user(&admin.id, user, Context { now, id: &id })?,
                        )
                    }
                    Purpose::Reset {
                        user_id,
                        admin_token,
                    } => {
                        let (admin, _) = self.store.authenticate_session(&admin_token, now)?;
                        if admin.role != d::USER_ADMIN {
                            return Err(Error::Http(403, "admin role required"));
                        }
                        Response::json(
                            200,
                            &self.store.reset_user_password(&admin.id, &user_id, &hash)?,
                        )
                    }
                    Purpose::Login(_) => Err(Error::Http(500, "invalid password operation")),
                }
            }
        }
    }
}
