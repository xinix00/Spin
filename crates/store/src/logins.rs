//! Een loginpool behoort bij een laag over al haar versies heen.
use crate::{Context, Error, Persistence, Result, Store};
use alloc::{string::String, vec::Vec};
use spin_core::validation::{normalized, text};
use spin_domain::{
    self as d, List, Timestamp, TryClone, WireMap, state::PersistedState, try_string,
};

/// De stabiele sleutel van een laag, onafhankelijk van de artifactversie.
pub fn layer_key(artifact: &d::Artifact) -> Result<String> {
    Ok(text(format_args!(
        "{}/{}:{}",
        artifact.subject, artifact.kind, artifact.name
    ))?)
}
fn holder<'a>(state: &'a PersistedState, login: &d::Login) -> Option<&'a d::Composition> {
    state.compositions.iter().map(|(_, c)| c).find(|c| {
        c.runtime.as_ref().is_none_or(|r| r.status != "stopped")
            && c.logins.get(&login.key).is_some_and(|id| *id == login.id)
    })
}
fn allowed(login: &d::Login, operator: &str) -> bool {
    !login.disabled && (login.owner.is_empty() || login.owner == operator)
}
fn files_copy(files: &WireMap<d::Bytes>) -> Result<WireMap<d::Bytes>> {
    let mut out = WireMap::new();
    for (path, bytes) in files.iter() {
        if bytes.0.is_some() {
            out.insert(try_string(path)?, bytes.try_clone()?)?;
        }
    }
    Ok(out)
}
fn hold(
    state: &mut PersistedState,
    composition: &str,
    id: &str,
    now: &Timestamp,
) -> Result<d::Login> {
    let login = state.logins.get_mut(id).ok_or(Error::NotFound)?;
    login.last_used_at = Some(now.try_clone()?);
    let out = login.try_clone()?;
    state
        .compositions
        .get_mut(composition)
        .ok_or(Error::NotFound)?
        .logins
        .insert(login.key.try_clone()?, login.id.try_clone()?)?;
    Ok(out)
}
fn select(
    state: &PersistedState,
    composition: &str,
    key: &str,
    exclusive: bool,
    swap: bool,
) -> Result<String> {
    let capsule = state.compositions.get(composition).ok_or(Error::NotFound)?;
    if capsule
        .runtime
        .as_ref()
        .is_some_and(|r| r.status == "stopped")
    {
        return Err(Error::Conflict(
            "a login goes to a capsule that runs or is being built",
        ));
    }
    let held = capsule.logins.get(key);
    let operator = normalized(&capsule.operator)?;
    let mut choices = Vec::new();
    let mut any = false;
    for (_, login) in state.logins.iter().filter(|(_, l)| l.key == key) {
        any = true;
        if !allowed(login, &operator) || (swap && held == Some(&login.id)) {
            continue;
        }
        if exclusive && holder(state, login).is_some_and(|c| swap || c.id != composition) {
            continue;
        }
        d::try_push(
            &mut choices,
            (
                login.owner.is_empty(),
                login
                    .last_used_at
                    .as_ref()
                    .map(Timestamp::time)
                    .transpose()?
                    .unwrap_or_default(),
                login.number,
                login,
            ),
        )?;
    }
    choices.sort_unstable_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    match choices.first() {
        Some((_, _, _, login)) => Ok(login.id.try_clone()?),
        None if !any && !swap => Err(Error::NotFound),
        None => Err(Error::LoginsBusy),
    }
}
impl<P: Persistence> Store<P> {
    /// Leent één login met de geheime bestanden; niet voor publieke snapshots.
    pub fn login(&self, id: &str) -> Result<&d::Login> {
        self.state.logins.get(id).ok_or(Error::NotFound)
    }
    /// Geeft de logins van een laag in nummervolgorde.
    pub fn logins_for(&self, key: &str) -> Result<List<d::Login>> {
        let mut out = List::new();
        for (_, login) in self
            .state
            .logins
            .iter()
            .filter(|(_, login)| login.key == key)
        {
            out.push(login.try_clone()?)?;
        }
        out.as_mut_slice().sort_unstable_by_key(|l| l.number);
        Ok(out)
    }
    /// Vooruitblik vóór imagewerk; het daadwerkelijke uitgeven blijft atomair.
    pub fn logins_free(&self, key: &str, exclusive: bool, operator: &str) -> Result<bool> {
        if !exclusive {
            return Ok(true);
        }
        let operator = normalized(operator)?;
        let mut any = false;
        for (_, login) in self.state.logins.iter().filter(|(_, l)| l.key == key) {
            any = true;
            if allowed(login, &operator) && holder(&self.state, login).is_none() {
                return Ok(true);
            }
        }
        Ok(!any)
    }
    /// Houdt een login vast vóór de trage capsulebouw; eigen logins gaan voor.
    pub fn hand_out_login(
        &mut self,
        composition: &str,
        key: &str,
        exclusive: bool,
        now: &Timestamp,
    ) -> Result<d::Login> {
        let capsule = self
            .state
            .compositions
            .get(composition)
            .ok_or(Error::NotFound)?;
        if capsule
            .runtime
            .as_ref()
            .is_some_and(|r| r.status == "stopped")
        {
            return Err(Error::Conflict(
                "a login goes to a capsule that runs or is being built",
            ));
        }
        if let Some(login) = capsule
            .logins
            .get(key)
            .and_then(|id| self.state.logins.get(id))
            .filter(|l| !l.disabled)
        {
            return Ok(login.try_clone()?);
        }
        let id = select(&self.state, composition, key, exclusive, false)?;
        self.edit(|state| hold(state, composition, &id, now))
    }
    /// Wisselt naar een vrije login die het langst ongebruikt is.
    pub fn swap_login(
        &mut self,
        composition: &str,
        key: &str,
        now: &Timestamp,
    ) -> Result<d::Login> {
        let id = select(&self.state, composition, key, true, true)?;
        self.edit(|state| hold(state, composition, &id, now))
    }
    /// Reserve the replacement and fence credential capture until its write is acknowledged.
    /// After a crash the server stops this capsule instead of capturing an uncertain account.
    pub fn prepare_login_install(
        &mut self,
        composition: &str,
        key: &str,
        now: &Timestamp,
    ) -> Result<d::Login> {
        let id = select(&self.state, composition, key, true, true)?;
        self.edit(|state| {
            let capsule = state
                .compositions
                .get_mut(composition)
                .ok_or(Error::NotFound)?
                .runtime
                .as_mut()
                .ok_or(Error::Conflict("capsule is not running"))?;
            if capsule.status != "ready" || capsule.stop_pending {
                return Err(Error::Conflict("capsule is not ready for a login swap"));
            }
            capsule.status = try_string("installing_login")?;
            hold(state, composition, &id, now)
        })
    }
    /// Voegt inhoud toe en bindt deze optioneel meteen aan de broncapsule.
    pub fn create_login(
        &mut self,
        composition: &str,
        key: &str,
        files: &WireMap<d::Bytes>,
        owner: &str,
        context: Context<'_>,
    ) -> Result<d::Login> {
        context.validate()?;
        let files = files_copy(files)?;
        if files.is_empty() {
            return Err(Error::Conflict(
                "the capsule holds none of the layer's tracked files",
            ));
        }
        let owner = normalized(owner)?;
        self.edit(|state| {
            if !composition.is_empty() && state.compositions.get(composition).is_none() {
                return Err(Error::NotFound);
            }
            if state.logins.get(context.id).is_some() {
                return Err(Error::Conflict("login id already exists"));
            }
            let number = state
                .logins
                .iter()
                .filter(|(_, l)| l.key == key)
                .map(|(_, l)| l.number)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(Error::Conflict("login number exhausted"))?;
            let login = d::Login {
                id: try_string(context.id)?,
                key: try_string(key)?,
                number,
                owner,
                files,
                created_at: context.now.try_clone()?,
                updated_at: context.now.try_clone()?,
                ..Default::default()
            };
            state
                .logins
                .insert(login.id.try_clone()?, login.try_clone()?)?;
            if composition.is_empty() {
                Ok(login)
            } else {
                hold(state, composition, context.id, context.now)
            }
        })
    }
    /// Een lege folderlezing wist nooit credentials; nil betekent gezien, niet meegenomen.
    pub fn save_login_files(
        &mut self,
        id: &str,
        files: &WireMap<d::Bytes>,
        folders: &[String],
        now: &Timestamp,
    ) -> Result<(bool, List<String>)> {
        let login = self.login(id)?;
        let mut merged = files_copy(&login.files)?;
        let mut dropped = List::new();
        for (path, _) in merged.iter() {
            if files.get(path).is_some() {
                continue;
            }
            if folders.iter().any(|folder| {
                d::tracked_folder(folder)
                    && path.starts_with(folder)
                    && files.iter().any(|(seen, _)| seen.starts_with(folder))
            }) {
                dropped.push(try_string(path)?)?;
            }
        }
        for path in dropped.iter() {
            merged.remove(path);
        }
        let mut changed = !dropped.is_empty();
        for (path, bytes) in files.iter() {
            if let Some(data) = &bytes.0
                && merged.get(path).and_then(|b| b.0.as_ref()) != Some(data)
            {
                merged.insert(try_string(path)?, bytes.try_clone()?)?;
                changed = true;
            }
        }
        if !changed {
            return Ok((false, List::default()));
        }
        dropped.as_mut_slice().sort_unstable();
        self.edit(|state| {
            let login = state.logins.get_mut(id).ok_or(Error::NotFound)?;
            login.files = merged;
            login.updated_at = now.try_clone()?;
            Ok((true, dropped))
        })
    }
    /// Namen en groottes in padvolgorde, zonder bestandsinhoud.
    pub fn login_files(&self, id: &str) -> Result<List<d::LoginFile>> {
        let mut out = List::new();
        for (path, bytes) in self.login(id)?.files.iter() {
            out.push(d::LoginFile {
                path: try_string(path)?,
                size: i64::try_from(bytes.0.as_ref().map_or(0, Vec::len))
                    .map_err(|_| Error::Conflict("file too large"))?,
            })?;
        }
        out.as_mut_slice()
            .sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
    /// Verwijdert een bestand of map uit alle logins van de laag.
    pub fn exclude_from_logins(&mut self, key: &str, path: &str, now: &Timestamp) -> Result<usize> {
        let matches =
            |file: &str| file == path || (d::tracked_folder(path) && file.starts_with(path));
        if !self
            .state
            .logins
            .iter()
            .any(|(_, l)| l.key == key && l.files.iter().any(|(p, _)| matches(p)))
        {
            return Ok(0);
        }
        self.edit(|state| {
            let mut removed = 0;
            for (_, login) in state.logins.iter_mut().filter(|(_, l)| l.key == key) {
                let before = login.files.len();
                login.files.retain(|p, _| !matches(p));
                let count = before - login.files.len();
                if count != 0 {
                    login.updated_at = now.try_clone()?;
                }
                removed += count;
            }
            Ok(removed)
        })
    }
    /// Parkeren trekt de binding van een reeds draaiende capsule niet in.
    pub fn set_login_disabled(&mut self, id: &str, disabled: bool) -> Result<d::Login> {
        self.edit(|state| {
            let login = state.logins.get_mut(id).ok_or(Error::NotFound)?;
            login.disabled = disabled;
            Ok(login.try_clone()?)
        })
    }
    /// De bestaande naamsgrens is tachtig UTF-8-bytes.
    pub fn rename_login(&mut self, id: &str, name: &str) -> Result<d::Login> {
        if name.trim().len() > 80 {
            return Err(Error::Conflict("a login name may be at most 80 characters"));
        }
        self.edit(|state| {
            let login = state.logins.get_mut(id).ok_or(Error::NotFound)?;
            login.name = try_string(name.trim())?;
            Ok(login.try_clone()?)
        })
    }
    /// Alleen een vrije login kan worden verwijderd.
    pub fn delete_login(&mut self, id: &str) -> Result<d::Login> {
        self.edit(|state| {
            let login = state.logins.get(id).ok_or(Error::NotFound)?;
            if holder(state, login).is_some() {
                return Err(Error::Conflict(
                    "login is held by a running capsule; stop it first",
                ));
            }
            state.logins.remove(id).ok_or(Error::NotFound)
        })
    }
    /// Publieke poolweergave bevat alleen metagegevens en de eventuele houder.
    pub fn login_summaries(&self) -> Result<List<d::LoginSummary>> {
        let mut out = List::new();
        for (_, login) in self.state.logins.iter() {
            let mut bytes = 0_i64;
            for (_, data) in login.files.iter() {
                bytes = bytes
                    .checked_add(
                        i64::try_from(data.0.as_ref().map_or(0, Vec::len))
                            .map_err(|_| Error::Conflict("file too large"))?,
                    )
                    .ok_or(Error::Conflict("login too large"))?;
            }
            out.push(d::LoginSummary {
                id: login.id.try_clone()?,
                key: login.key.try_clone()?,
                number: login.number,
                name: login.name.try_clone()?,
                disabled: login.disabled,
                owner: login.owner.try_clone()?,
                files: i64::try_from(login.files.len())
                    .map_err(|_| Error::Conflict("too many login files"))?,
                bytes,
                last_used_at: login.last_used_at.try_clone()?,
                created_at: login.created_at.try_clone()?,
                updated_at: login.updated_at.try_clone()?,
                composition_id: try_string(
                    holder(&self.state, login).map_or("", |c| c.id.as_str()),
                )?,
            })?;
        }
        out.as_mut_slice()
            .sort_unstable_by(|a, b| (&a.key, a.number).cmp(&(&b.key, b.number)));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Memory;
    use core::cell::Cell;
    use d::Wire;
    fn now() -> Timestamp {
        Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap()
    }
    fn pool() -> PersistedState {
        PersistedState::from_json(br#"{
          "compositions":{"c1":{"id":"c1","operator":"derek"},"c2":{"id":"c2","operator":"derek"}},
          "logins":{
            "shared":{"id":"shared","key":"tool:codex","number":1,"files":{"/auth":"YQ=="}},
            "own":{"id":"own","key":"tool:codex","number":2,"owner":"derek","files":{"/auth":"Yg=="}},
            "other":{"id":"other","key":"tool:codex","number":3,"owner":"someoneelse","files":{"/auth":"Yw=="}}
          }
        }"#).unwrap()
    }
    #[test]
    fn exclusive_pool_prioritizes_owner_and_fences_failed_save() {
        let fail = Cell::new(true);
        let mut store = Store::new(pool(), Memory(&fail));
        assert_eq!(
            store
                .hand_out_login("c1", "tool:codex", true, &now())
                .unwrap_err(),
            Error::Storage(10)
        );
        assert!(
            store
                .state
                .compositions
                .get("c1")
                .unwrap()
                .logins
                .is_empty()
        );
        assert!(store.login("own").unwrap().last_used_at.is_none());
        fail.set(false);
        assert_eq!(
            store
                .hand_out_login("c1", "tool:codex", true, &now())
                .unwrap()
                .id,
            "own"
        );
        assert_eq!(
            store
                .hand_out_login("c2", "tool:codex", true, &now())
                .unwrap()
                .id,
            "shared"
        );
        assert!(!store.logins_free("tool:codex", true, "derek").unwrap());
        assert!(matches!(store.delete_login("own"), Err(Error::Conflict(_))));
        assert_eq!(
            store.swap_login("c1", "tool:codex", &now()).unwrap_err(),
            Error::LoginsBusy
        );
        store.state.compositions.remove("c2");
        assert_eq!(
            store.swap_login("c1", "tool:codex", &now()).unwrap().id,
            "shared"
        );
        assert!(store.delete_login("own").is_ok());
    }
    #[test]
    fn incomplete_folder_reads_preserve_credentials_and_report_actual_removals() {
        let fail = Cell::new(false);
        let mut store = Store::new(pool(), Memory(&fail));
        let files = WireMap::<d::Bytes>::from_json(
            br#"{"/auth/a":"YQ==","/auth/b":"Yg==","/outside":"Yw=="}"#,
        )
        .unwrap();
        store.state.logins.get_mut("own").unwrap().files = files;
        let folders = [String::from("/auth/")];
        assert!(
            !store
                .save_login_files("own", &WireMap::new(), &folders, &now())
                .unwrap()
                .0
        );
        let seen = WireMap::<d::Bytes>::from_json(br#"{"/auth/a":null}"#).unwrap();
        fail.set(true);
        assert_eq!(
            store
                .save_login_files("own", &seen, &folders, &now())
                .unwrap_err(),
            Error::Storage(10)
        );
        assert_eq!(store.login("own").unwrap().files.len(), 3);
        fail.set(false);
        let (changed, dropped) = store
            .save_login_files("own", &seen, &folders, &now())
            .unwrap();
        assert!(changed);
        assert_eq!(&*dropped, &[String::from("/auth/b")]);
        let kept = &store.login("own").unwrap().files;
        assert_eq!(
            kept.get("/auth/a").unwrap().0.as_deref(),
            Some(b"a".as_slice())
        );
        assert!(kept.get("/outside").is_some());
        assert!(
            !store
                .save_login_files("own", &seen, &folders, &now())
                .unwrap()
                .0
        );
    }
    #[test]
    fn parked_and_stopped_capsules_obey_pool_lifecycle() {
        let fail = Cell::new(false);
        let mut store = Store::new(pool(), Memory(&fail));
        store
            .hand_out_login("c1", "tool:codex", true, &now())
            .unwrap();
        store.set_login_disabled("own", true).unwrap();
        assert_eq!(
            store
                .login_summaries()
                .unwrap()
                .iter()
                .find(|s| s.id == "own")
                .unwrap()
                .composition_id,
            "c1"
        );
        store.swap_login("c1", "tool:codex", &now()).unwrap();
        store
            .set_composition_runtime(
                "c1",
                "derek",
                d::CapsuleRuntime {
                    status: "stopped".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(matches!(
            store.hand_out_login("c1", "tool:codex", true, &now()),
            Err(Error::Conflict(_))
        ));
        assert!(store.delete_login("shared").is_ok());
    }
}
