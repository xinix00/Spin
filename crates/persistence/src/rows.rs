//! De state per entiteit: één rij per (collectie, id) in `spin_rows`. Een
//! mutatie schrijft alleen de rijen die veranderden; laden voegt ze weer samen
//! tot hetzelfde JSON-document als de oude enkele rij `state`.
use super::*;
use spin_domain::TryClone;
use spin_store::Change;

/// Eén rij: de opgeslagen (versleutelde) JSON van een entiteit, of `None`
/// als hij verdween. Losse waarden (`worker_token`, `garbage_refs`) hebben
/// een lege id.
#[derive(Debug, PartialEq)]
pub struct Row {
    /// De collectie in de state.
    pub collection: &'static str,
    /// De sleutel in die collectie; leeg voor een losse waarde.
    pub id: String,
    /// De JSON van de entiteit, of `None` om de rij te verwijderen.
    pub value: Option<String>,
}

macro_rules! collections {
    ($($name:ident),* $(,)?) => {
        /// Alle collecties die als rijen bestaan (`login_states` is na het
        /// laden altijd leeg en wordt nooit geschreven).
        const COLLECTIONS: &[&str] = &[$(stringify!($name)),*];
        /// Kopieert één entiteit naar een gedeeltelijke state.
        fn copy_entity(
            from: &PersistedState,
            to: &mut PersistedState,
            collection: &str,
            id: &str,
        ) -> Result {
            match collection {
                $(stringify!($name) => {
                    if let Some(value) = from.$name.get(id) {
                        to.$name.insert(try_string(id)?, value.try_clone()?)?;
                    }
                    Ok(())
                })*
                _ => Err(Error::Invalid("unknown state collection")),
            }
        }
        /// De JSON van één entiteit, of `None` als hij ontbreekt.
        fn entity_json(state: &PersistedState, collection: &str, id: &str) -> Result<Option<String>> {
            match collection {
                $(stringify!($name) => match state.$name.get(id) {
                    Some(value) => Ok(Some(value.to_json()?)),
                    None => Ok(None),
                },)*
                _ => Err(Error::Invalid("unknown state collection")),
            }
        }
        /// Elke entiteit van de state, voor een volledige schrijf.
        fn every_entity(state: &PersistedState) -> Result<Vec<Change>> {
            let mut out = Vec::new();
            $(for (id, _) in state.$name.iter() {
                d::try_push(&mut out, Change { collection: stringify!($name), id: try_string(id)? })?;
            })*
            Ok(out)
        }
    };
}
collections!(
    artifacts,
    recordings,
    compositions,
    jobs,
    job_attachments,
    workflow_templates,
    phase_runs,
    deliverables,
    deliverable_comments,
    code_review_revisions,
    code_review_comments,
    workflow_questions,
    job_request_keys,
    sessions,
    activations,
    turns,
    checkpoints,
    results,
    clients,
    mcp_servers,
    git_repositories,
    git_accounts,
    users,
    auth_sessions,
    git_oauth_configurations,
    logins,
    workflow_tokens,
);
fn static_name(collection: &str) -> Option<&'static str> {
    COLLECTIONS
        .iter()
        .chain(["worker_token", "garbage_refs"].iter())
        .find(|c| **c == collection)
        .copied()
}

/// De rijen voor `changes`, of voor de hele state bij `None`. Alleen de
/// genoemde entiteiten worden gekopieerd en versleuteld.
pub fn state_rows(
    cipher: &Cipher,
    entropy: &mut impl Entropy,
    state: &PersistedState,
    changes: Option<&[Change]>,
) -> Result<Vec<Row>> {
    let all;
    let mut scalars = [false, false];
    let changes = match changes {
        Some(changes) => changes,
        None => {
            all = every_entity(state)?;
            scalars = [true, true];
            &all
        }
    };
    let mut partial = PersistedState::default();
    for change in changes {
        match change.collection {
            "worker_token" => scalars[0] = true,
            "garbage_refs" => scalars[1] = true,
            "login_states" => {}
            collection => copy_entity(state, &mut partial, collection, &change.id)?,
        }
    }
    if scalars[0] {
        partial.worker_token = state.worker_token.try_clone()?;
    }
    let sealed = cipher.encrypt_state(&partial, entropy)?;
    let mut rows = Vec::new();
    for change in changes {
        let value = match change.collection {
            "login_states" | "worker_token" | "garbage_refs" => continue,
            collection => entity_json(&sealed, collection, &change.id)?,
        };
        let collection =
            static_name(change.collection).ok_or(Error::Invalid("unknown state collection"))?;
        d::try_push(
            &mut rows,
            Row {
                collection,
                id: try_string(&change.id)?,
                value,
            },
        )?;
    }
    if scalars[0] {
        d::try_push(
            &mut rows,
            Row {
                collection: "worker_token",
                id: String::new(),
                value: Some(sealed.worker_token.to_json()?),
            },
        )?;
    }
    if scalars[1] {
        d::try_push(
            &mut rows,
            Row {
                collection: "garbage_refs",
                id: String::new(),
                value: Some(state.garbage_refs.to_json()?),
            },
        )?;
    }
    Ok(rows)
}

impl<'e, 'a, B: Storage> Database<'e, 'a, B> {
    /// Of deze database al rijen heeft (een staged backup kan ouder zijn).
    fn has_rows(&mut self) -> Result<bool> {
        let mut s = self.connection.prepare(
            c"SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='spin_rows'",
        )?;
        if !s.step()? || integer(&mut s, 0)? == 0 {
            return Ok(false);
        }
        drop(s);
        let mut s = self
            .connection
            .prepare(c"SELECT 1 FROM spin_rows LIMIT 1")?;
        Ok(s.step()?)
    }
    /// De opgeslagen state als één JSON-document, uit de rijen of uit de oude
    /// rij `state`; `None` als er nog niets is. `true` betekent de oude vorm.
    pub fn read_state(&mut self, limit: usize) -> Result<Option<(Vec<u8>, bool)>> {
        self.ready()?;
        if !self.has_rows()? {
            return match self.read_file("state", limit) {
                Ok(bytes) => Ok(Some((bytes, true))),
                Err(Error::NotFound) => Ok(None),
                Err(e) => Err(e),
            };
        }
        let mut out = String::new();
        let mut current = "";
        let mut open = false;
        let mut s = self
            .connection
            .prepare(c"SELECT collection,id,value FROM spin_rows ORDER BY collection,id")?;
        out.push('{');
        let mut first_collection = true;
        while s.step()? {
            let collection = static_name(&string(&mut s, 0)?)
                .ok_or(Error::Invalid("unknown state collection"))?;
            let id = string(&mut s, 1)?;
            let value = match s.column(2)? {
                Value::Blob(b) => {
                    core::str::from_utf8(b).map_err(|_| Error::Invalid("state row is not UTF-8"))?
                }
                _ => return Err(Error::Invalid("state row has wrong type")),
            };
            if collection != current {
                if open {
                    d::try_push_str(&mut out, "}")?;
                    open = false;
                }
                if !first_collection {
                    d::try_push_str(&mut out, ",")?;
                }
                first_collection = false;
                d::json::write_string(collection, &mut out)?;
                d::try_push_str(&mut out, ":")?;
                if !id.is_empty() {
                    d::try_push_str(&mut out, "{")?;
                    open = true;
                }
                current = collection;
            } else if open {
                d::try_push_str(&mut out, ",")?;
            }
            if open {
                d::json::write_string(&id, &mut out)?;
                d::try_push_str(&mut out, ":")?;
            }
            d::try_push_str(&mut out, value)?;
            if out.len() > limit {
                return Err(Error::Invalid("state exceeds budget"));
            }
        }
        if open {
            d::try_push_str(&mut out, "}")?;
        }
        d::try_push_str(&mut out, "}")?;
        Ok(Some((out.into_bytes(), false)))
    }
    pub(crate) fn put_rows(&mut self, rows: &[Row]) -> Result {
        for row in rows {
            match &row.value {
                Some(value) => self.execute(
                    c"INSERT INTO spin_rows(collection,id,value) VALUES(?,?,?) ON CONFLICT(collection,id) DO UPDATE SET value=excluded.value",
                    &[
                        Value::Text(row.collection),
                        Value::Text(&row.id),
                        Value::Blob(value.as_bytes()),
                    ],
                )?,
                None => self.execute(
                    c"DELETE FROM spin_rows WHERE collection=? AND id=?",
                    &[Value::Text(row.collection), Value::Text(&row.id)],
                )?,
            }
        }
        Ok(())
    }
    /// Schrijft de gewijzigde rijen in één bevestigde transactie.
    pub fn write_rows(&mut self, rows: &[Row]) -> Result {
        if rows.is_empty() {
            return Ok(());
        }
        self.transaction(|db| db.put_rows(rows))
    }
    /// Vervangt de hele state door deze rijen, en ruimt de oude rij `state` op.
    pub fn replace_rows(&mut self, rows: &[Row]) -> Result {
        self.transaction(|db| {
            db.connection
                .execute(c"DELETE FROM spin_rows; DELETE FROM spin_kv WHERE key='state';")?;
            db.put_rows(rows)
        })
    }
}
