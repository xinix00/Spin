//! Identiteiten en browsersessies worden samen met hun intrekkingen opgeslagen.
use crate::{Context, Error, Persistence, Result, Store, require_admin};
use spin_core::validation::normalized;
use spin_domain::{self as d, Timestamp, TryClone, try_string};

pub(crate) fn public_user(user: &d::User) -> Result<d::PublicUser> {
    Ok(d::PublicUser {
        id: user.id.try_clone()?,
        username: user.username.try_clone()?,
        display_name: user.display_name.try_clone()?,
        role: user.role.try_clone()?,
        archived_at: user.archived_at.try_clone()?,
        created_at: user.created_at.try_clone()?,
    })
}

fn normalize_user(user: &mut d::User, context: &Context<'_>) -> Result {
    context.validate()?;
    user.username = normalized(&user.username)?;
    user.display_name = try_string(user.display_name.trim())?;
    if user.display_name.is_empty() {
        user.display_name = user.username.try_clone()?;
    }
    if user.username.is_empty() || user.password_hash.is_empty() {
        return Err(Error::Conflict("username and password hash are required"));
    }
    user.id = try_string(context.id)?;
    user.created_at = context.now.try_clone()?;
    user.updated_at = context.now.try_clone()?;
    Ok(())
}
impl<P: Persistence> Store<P> {
    /// Of de eenmalige owner-setup al voltooid is.
    pub fn has_users(&self) -> bool {
        !self.state.users.is_empty()
    }
    /// De eerste identiteit wordt admin; een tweede setup wordt geweigerd.
    pub fn create_initial_user(
        &mut self,
        mut user: d::User,
        context: Context<'_>,
    ) -> Result<d::PublicUser> {
        normalize_user(&mut user, &context)?;
        user.role = try_string(d::USER_ADMIN)?;
        self.edit(move |state| {
            if !state.users.is_empty() {
                return Err(Error::Conflict("owner setup is already complete"));
            }
            let public = public_user(&user)?;
            state.users.insert(user.id.try_clone()?, user)?;
            Ok(public)
        })
    }
    /// Een admin maakt een nieuwe, unieke identiteit.
    pub fn create_user(
        &mut self,
        actor_id: &str,
        mut user: d::User,
        context: Context<'_>,
    ) -> Result<d::PublicUser> {
        normalize_user(&mut user, &context)?;
        if user.role.is_empty() {
            user.role = try_string(d::USER_MEMBER)?;
        }
        if user.role != d::USER_ADMIN && user.role != d::USER_MEMBER {
            return Err(Error::Conflict("invalid user role"));
        }
        self.edit(move |state| {
            require_admin(state, actor_id)?;
            if state.users.get(&user.id).is_some()
                || state
                    .users
                    .iter()
                    .any(|(_, old)| old.username == user.username)
            {
                return Err(Error::Conflict("user already exists"));
            }
            let public = public_user(&user)?;
            state.users.insert(user.id.try_clone()?, user)?;
            Ok(public)
        })
    }
    /// Leent de interne identiteit, inclusief het wachtwoordhash.
    pub fn user(&self, id: &str) -> Result<&d::User> {
        self.state.users.get(id.trim()).ok_or(Error::NotFound)
    }
    /// Zoekt de genormaliseerde gebruikersnaam voor authenticatie.
    pub fn user_by_username(&self, name: &str) -> Result<&d::User> {
        let name = normalized(name)?;
        self.state
            .users
            .iter()
            .find(|(_, user)| user.username == name)
            .map(|(_, user)| user)
            .ok_or(Error::NotFound)
    }
    /// Een nieuw wachtwoord trekt alle bestaande browsersessies atomair in.
    pub fn reset_user_password(
        &mut self,
        actor: &str,
        id: &str,
        hash: &str,
    ) -> Result<d::PublicUser> {
        if hash.trim().is_empty() {
            return Err(Error::Conflict("password hash is required"));
        }
        self.edit(|state| {
            require_admin(state, actor)?;
            let user = state.users.get_mut(id.trim()).ok_or(Error::NotFound)?;
            user.password_hash = try_string(hash)?;
            let public = public_user(user)?;
            state
                .auth_sessions
                .retain(|_, session| session.user_id != public.id);
            Ok(public)
        })
    }
    /// Archiveren bewaart historie en trekt de browsersessies in.
    pub fn set_user_archived(
        &mut self,
        actor: &str,
        id: &str,
        archived: bool,
        now: &Timestamp,
    ) -> Result<d::PublicUser> {
        self.edit(|state| {
            require_admin(state, actor)?;
            let user = state.users.get(id.trim()).ok_or(Error::NotFound)?;
            if archived && user.id == actor.trim() {
                return Err(Error::Conflict(
                    "an admin cannot archive their own active identity",
                ));
            }
            if archived
                && user.role == d::USER_ADMIN
                && user.archived_at.is_none()
                && state
                    .users
                    .iter()
                    .filter(|(_, u)| u.role == d::USER_ADMIN && u.archived_at.is_none())
                    .count()
                    <= 1
            {
                return Err(Error::Conflict("the last active admin cannot be archived"));
            }
            let user = state.users.get_mut(id.trim()).ok_or(Error::NotFound)?;
            if archived {
                if user.archived_at.is_none() {
                    user.archived_at = Some(now.try_clone()?);
                }
            } else {
                user.archived_at = None;
            }
            user.updated_at = now.try_clone()?;
            let public = public_user(user)?;
            if archived {
                state.auth_sessions.retain(|_, s| s.user_id != public.id);
            }
            Ok(public)
        })
    }
    /// Een ingelogde identiteit krijgt een serverzijdige sessie met eindtijd.
    pub fn create_auth_session(
        &mut self,
        user_id: &str,
        token_hash: &str,
        csrf_hash: &str,
        expires: Timestamp,
        context: Context<'_>,
    ) -> Result<d::AuthSession> {
        context.validate()?;
        if token_hash.is_empty() || csrf_hash.is_empty() || expires.time()? <= context.now.time()? {
            return Err(Error::Conflict(
                "valid session hashes and expiry are required",
            ));
        }
        self.edit(|state| {
            let user = state.users.get(user_id).ok_or(Error::NotFound)?;
            if user.archived_at.is_some() {
                return Err(Error::Conflict("identity is archived"));
            }
            if state.auth_sessions.get(context.id).is_some() {
                return Err(Error::Conflict("auth session id already exists"));
            }
            let session = d::AuthSession {
                id: try_string(context.id)?,
                user_id: try_string(user_id)?,
                token_hash: try_string(token_hash)?,
                csrf_hash: try_string(csrf_hash)?,
                expires_at: expires,
                created_at: context.now.try_clone()?,
                last_seen_at: context.now.try_clone()?,
            };
            state
                .auth_sessions
                .insert(session.id.try_clone()?, session.try_clone()?)?;
            Ok(session)
        })
    }
    /// Controleert token en eindtijd en werkt het bezoek hoogstens per vijf minuten bij.
    pub fn authenticate_session(
        &mut self,
        token_hash: &str,
        now: &Timestamp,
    ) -> Result<(d::User, d::AuthSession)> {
        let time = now.time()?;
        let mut matched = None;
        let mut expired = false;
        for (_, session) in self.state.auth_sessions.iter() {
            if session.expires_at.time()? <= time {
                expired = true;
                continue;
            }
            if session.token_hash == token_hash {
                matched = Some(session.try_clone()?);
            }
        }
        let Some(mut session) = matched else {
            if expired {
                self.expire_auth_sessions(time)?;
            }
            return Err(Error::NotFound);
        };
        let user = self.state.users.get(&session.user_id);
        if user.is_none_or(|u| u.archived_at.is_some()) {
            self.delete_auth_session(&session.id)?;
            return Err(Error::NotFound);
        }
        let user = user.ok_or(Error::NotFound)?.try_clone()?;
        let update = time.since(session.last_seen_at.time()?) > 300_000_000_000;
        if update {
            session.last_seen_at = now.try_clone()?;
        }
        if expired || update {
            let saved = session.try_clone()?;
            self.edit(|state| {
                let mut remove = d::List::new();
                for (id, old) in state.auth_sessions.iter() {
                    if old.expires_at.time()? <= time {
                        remove.push(try_string(id)?)?;
                    }
                }
                for id in remove.iter() {
                    state.auth_sessions.remove(id);
                }
                state.auth_sessions.insert(saved.id.try_clone()?, saved)?;
                Ok(())
            })?;
        }
        Ok((user, session))
    }
    fn expire_auth_sessions(&mut self, now: d::Time) -> Result {
        self.edit(|state| {
            let mut remove = d::List::new();
            for (id, session) in state.auth_sessions.iter() {
                if session.expires_at.time()? <= now {
                    remove.push(try_string(id)?)?;
                }
            }
            for id in remove.iter() {
                state.auth_sessions.remove(id);
            }
            Ok(())
        })
    }
    /// Logout trekt exact deze browsersessie in.
    pub fn delete_auth_session(&mut self, id: &str) -> Result {
        self.edit(|state| {
            state.auth_sessions.remove(id).ok_or(Error::NotFound)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Memory;
    use core::cell::Cell;
    use d::{Wire, state::PersistedState};

    #[test]
    fn archive_revokes_sessions_and_keeps_audit_identity() {
        let fail = Cell::new(false);
        let mut store = Store::new(PersistedState::default(), Memory(&fail));
        let now = Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap();
        let expires = || Timestamp::from_json(br#""2026-09-30T13:00:00Z""#).unwrap();
        let user = |name| d::User {
            username: try_string(name).unwrap(),
            password_hash: try_string("hash").unwrap(),
            ..Default::default()
        };
        store
            .create_initial_user(
                user("Derek"),
                Context {
                    id: "admin",
                    now: &now,
                },
            )
            .unwrap();
        store
            .create_user(
                "admin",
                user("John"),
                Context {
                    id: "member",
                    now: &now,
                },
            )
            .unwrap();
        store
            .create_auth_session(
                "member",
                "token",
                "csrf",
                expires(),
                Context {
                    id: "browser",
                    now: &now,
                },
            )
            .unwrap();
        assert_eq!(
            store.authenticate_session("token", &now).unwrap().0.id,
            "member"
        );
        assert!(
            store
                .set_user_archived("member", "admin", true, &now)
                .is_err()
        );
        assert!(
            store
                .set_user_archived("admin", "admin", true, &now)
                .is_err()
        );
        store
            .set_user_archived("admin", "member", true, &now)
            .unwrap();
        assert!(matches!(
            store.authenticate_session("token", &now),
            Err(Error::NotFound)
        ));
        assert!(store.user("member").unwrap().archived_at.is_some());
        assert!(
            store
                .create_auth_session(
                    "member",
                    "token",
                    "csrf",
                    expires(),
                    Context {
                        id: "browser2",
                        now: &now
                    }
                )
                .is_err()
        );
        store
            .set_user_archived("admin", "member", false, &now)
            .unwrap();
        assert!(store.user("member").unwrap().archived_at.is_none());
        assert!(store.authenticate_session("token", &now).is_err());
    }

    #[test]
    fn password_reset_revokes_only_its_users_sessions() {
        let fail = Cell::new(false);
        let mut store = Store::new(PersistedState::default(), Memory(&fail));
        let now = Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap();
        let expires = || Timestamp::from_json(br#""2026-09-30T13:00:00Z""#).unwrap();
        let user = |name| d::User {
            username: try_string(name).unwrap(),
            password_hash: try_string("hash").unwrap(),
            ..Default::default()
        };
        store
            .create_initial_user(
                user("Derek"),
                Context {
                    id: "admin",
                    now: &now,
                },
            )
            .unwrap();
        store
            .create_user(
                "admin",
                user("John"),
                Context {
                    id: "member",
                    now: &now,
                },
            )
            .unwrap();
        store
            .create_auth_session(
                "admin",
                "a",
                "csrf",
                expires(),
                Context {
                    id: "a1",
                    now: &now,
                },
            )
            .unwrap();
        store
            .create_auth_session(
                "member",
                "b",
                "csrf",
                expires(),
                Context {
                    id: "b1",
                    now: &now,
                },
            )
            .unwrap();
        fail.set(true);
        assert!(
            store
                .reset_user_password("admin", "member", "new-hash")
                .is_err()
        );
        assert_eq!(store.user("member").unwrap().password_hash, "hash");
        assert!(store.authenticate_session("b", &now).is_ok());
        fail.set(false);
        store
            .reset_user_password("admin", "member", "new-hash")
            .unwrap();
        assert!(store.authenticate_session("b", &now).is_err());
        assert!(store.authenticate_session("a", &now).is_ok());
        assert_eq!(store.user("member").unwrap().password_hash, "new-hash");
    }
}
