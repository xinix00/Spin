//! Schema-native backups bevatten ciphertext én de bronkey in één admin-only archive.
use crate::{Error, Persistence, Result, Store};
use alloc::string::String;
use spin_domain::{self as d, List, Timestamp, TryClone, Wire, state::PersistedState, try_string};
use spin_security::{Cipher, Entropy};
/// Gecontroleerde toegang tot een stilstaande database; de adapter weigert writes tot End.
pub enum Request {
    /// Synchroniseer en reserveer de database voor één download.
    Begin,
    /// Lees één aaneengesloten blok van hoogstens 64 KiB.
    Read {
        /// Bytepositie in de bevroren database.
        offset: u64,
        /// Gevraagde bloklengte, maximaal 64 KiB.
        length: usize,
    },
    /// Geef de schrijfpoort vrij, ook na een afgebroken verbinding.
    End,
}
/// Alleen de geauthenticeerde backup-eigenaar mag de sleutel ontvangen.
pub enum Reply {
    /// De totale databaseomvang en de bijbehorende encryptiesleutel.
    Ready {
        /// Exacte bestandsgrootte in bytes.
        size: u64,
        /// Portable master key voor dit databasebestand.
        key: String,
    },
    /// Eén databaseblok.
    Bytes(alloc::vec::Vec<u8>),
    /// Bevestigde vrijgave.
    Done,
}
/// Bounded work on an uploaded database, kept separate from the active Store.
pub enum RestoreRequest {
    /// Take ownership of a completely uploaded backup object.
    Begin(i64),
    /// Queue a retained Replica generation for the same validated restore pipeline.
    Replica {
        /// Full generation identifier, validated by Replica before path construction.
        generation: String,
        /// Optional inclusive recovery timestamp.
        at: Option<Timestamp>,
    },
    /// Validate/extract the next bounded block.
    Step,
    /// Remove staging data after either failure or success.
    Abort,
}
/// One retained recovery point from the existing Replica catalog.
pub struct ReplicaPoint {
    /// Full generation identifier.
    pub generation: String,
    /// RFC3339 timestamp with preserved nanoseconds.
    pub at: String,
    /// Snapshot (-1), raw commit (0), or compaction level.
    pub level: i32,
    /// Whether this is the active complete generation.
    pub current: bool,
}
/// The adapter never installs unvalidated state itself.
pub enum RestoreReply {
    /// Bytes extracted so far and expected database size.
    Progress(u64, u64),
    /// Staged database and its objects passed validation; decrypt before installation.
    Prepared(PortableState),
    /// Cleanup completed.
    Done,
}
impl<P: Persistence> Store<P> {
    /// Catalog access remains with the same exclusive Replica owner.
    pub fn replica_points(&mut self) -> Result<List<ReplicaPoint>> {
        self.persistence.replica_points()
    }
    /// Drive staging under the same durable error fence as ordinary operations.
    pub fn stage_restore(&mut self, request: RestoreRequest) -> Result<RestoreReply> {
        if let Some(code) = self.uncertain {
            return Err(Error::StorageUncertain(code));
        }
        self.persistence.stage_restore(request)
    }
    /// Atomically replace both blobs and encrypted state, then publish the in-memory candidate.
    pub fn install_restore(&mut self, candidate: PersistedState) -> Result {
        if let Some(code) = self.uncertain {
            return Err(Error::StorageUncertain(code));
        }
        let version = self
            .version
            .checked_add(1)
            .ok_or(Error::Conflict("state version exhausted"))?;
        if let Err(error) = self.persistence.install_restore(&candidate) {
            if let Error::StorageUncertain(code) = error {
                self.uncertain = Some(code);
            }
            return Err(error);
        }
        self.state = candidate;
        self.version = version;
        self.changes.clear();
        Ok(())
    }
    /// Behoudt de duurzame foutgrens van de gewone Store-opdrachten.
    pub fn backup(&mut self, request: Request) -> Result<Reply> {
        if let Some(code) = self.uncertain {
            return Err(Error::StorageUncertain(code));
        }
        self.persistence.backup(request)
    }
}
/// Dezelfde expliciete bovengrens als de SQLite-state-ingang.
pub const MAX_PORTABLE_BYTES: usize = 64 << 20;
/// De server bewaakt toegang tot zowel deze JSON als de bijbehorende sleutel.
pub struct PortableState {
    /// Versleutelde state in het bestaande schema.
    pub json: String,
    /// De 32-byte bronkey in raw standaard-base64.
    pub master_key: String,
}
/// De benodigde objecten en aantallen voor backupvoorbereiding.
pub struct Inspection {
    /// Artifactmetadata met de verwijzingen naar snapshots.
    pub artifacts: List<d::Artifact>,
    /// Jobattachments waarvan de bytes mee moeten.
    pub attachments: List<d::JobAttachment>,
    /// Gebruikers in de kandidaat.
    pub users: usize,
    /// Jobs in de kandidaat.
    pub jobs: usize,
    /// Templates in de kandidaat.
    pub templates: usize,
    /// Deliverables in de kandidaat.
    pub deliverables: usize,
}
/// Alleen een opslagadapter die zijn sleutel bezit kan een portable backup uitvoeren.
pub trait PortablePersistence: Persistence {
    /// Versleutelt een snapshot met nieuwe nonces en voegt de bronkey toe.
    fn export(&mut self, state: &PersistedState) -> Result<PortableState>;
}
/// De gemeenschappelijke exportgrens voor concrete adapters.
pub fn encrypt(
    state: &PersistedState,
    cipher: &Cipher,
    entropy: &mut impl Entropy,
) -> Result<PortableState> {
    let sealed = cipher.encrypt_state(state, entropy)?;
    let json = sealed.to_json()?;
    if json.len() > MAX_PORTABLE_BYTES {
        return Err(Error::Conflict("backup exceeds state budget"));
    }
    Ok(PortableState {
        json,
        master_key: cipher.portable_key()?,
    })
}
fn decode(
    encoded: &[u8],
    key: &str,
    login_id: impl FnMut() -> spin_security::Result<String>,
) -> Result<PersistedState> {
    let sealed = PersistedState::from_json_strict(encoded, MAX_PORTABLE_BYTES)?;
    let mut state = Cipher::from_encoded(key)?.decrypt_state(&sealed, login_id)?;
    state.normalize_loaded()?;
    validate(&state)?;
    Ok(state)
}
fn validate(state: &PersistedState) -> Result {
    if state.users.is_empty() {
        return Err(Error::Conflict("backup has no users"));
    }
    let mut admins = 0;
    for (id, user) in state.users.iter() {
        if id.is_empty()
            || user.id != id
            || user.username.is_empty()
            || user.password_hash.is_empty()
        {
            return Err(Error::Conflict("invalid backup user"));
        }
        match user.role.as_str() {
            d::USER_ADMIN => admins += 1,
            d::USER_MEMBER => {}
            _ => return Err(Error::Conflict("invalid backup user role")),
        }
    }
    if admins == 0 {
        return Err(Error::Conflict("backup has no admin user"));
    }
    for (id, a) in state.job_attachments.iter() {
        if id.is_empty() || a.id != id || a.name.is_empty() || a.size < 0 || a.sha256.len() != 64 {
            return Err(Error::Conflict("invalid backup attachment"));
        }
        if !a.job_id.is_empty() && state.jobs.get(&a.job_id).is_none() {
            return Err(Error::Conflict("backup attachment refers to missing Job"));
        }
    }
    Ok(())
}
fn normalize(state: &mut PersistedState, now: &Timestamp) -> Result {
    state.auth_sessions = d::WireMap::new();
    state.workflow_tokens = d::WireMap::new();
    for (_, client) in state.clients.iter_mut() {
        client.status = try_string("offline")?;
    }
    for (_, composition) in state.compositions.iter_mut() {
        composition.runtime = None;
        composition.agent = None;
    }
    for (_, recording) in state.recordings.iter_mut() {
        if recording.status == d::RECORDING_OPEN {
            recording.status = try_string(d::RECORDING_CANCELLED)?;
            recording.ended_at = Some(now.try_clone()?);
            recording.runtime = None;
        }
    }
    for (_, session) in state.sessions.iter_mut() {
        if matches!(
            session.status.as_str(),
            d::SESSION_CLAIMED | d::SESSION_RUNNING
        ) {
            session.status = try_string(d::SESSION_FROZEN)?;
            session.lease_expires_at = None;
            session.activation_id.clear();
            session.updated_at = now.try_clone()?;
        }
    }
    for (_, activation) in state.activations.iter_mut() {
        if activation.status != d::ACTIVATION_ENDED {
            activation.status = try_string(d::ACTIVATION_ENDED)?;
            activation.reason = try_string("restored from portable backup")?;
            activation.ended_at = Some(now.try_clone()?);
        }
    }
    for (_, turn) in state.turns.iter_mut() {
        if turn.status == d::TURN_RUNNING {
            turn.status = try_string(d::TURN_COMPLETED)?;
            turn.ended_at = Some(now.try_clone()?);
        }
    }
    Ok(())
}
/// Decrypt and normalize without touching active state or storage.
pub fn prepare(
    encoded: &[u8],
    key: &str,
    now: &Timestamp,
    login_id: impl FnMut() -> spin_security::Result<String>,
) -> Result<PersistedState> {
    now.time()?;
    let mut state = decode(encoded, key, login_id)?;
    normalize(&mut state, now)?;
    Ok(state)
}
/// Inspecteert zonder de actieve Store te wijzigen; iedere secret moet ontsleutelbaar zijn.
pub fn inspect(
    encoded: &[u8],
    key: &str,
    login_id: impl FnMut() -> spin_security::Result<String>,
) -> Result<Inspection> {
    let state = decode(encoded, key, login_id)?;
    let mut artifacts = List::new();
    let mut attachments = List::new();
    for (_, a) in state.artifacts.iter() {
        artifacts.push(a.try_clone()?)?;
    }
    for (_, a) in state.job_attachments.iter() {
        attachments.push(a.try_clone()?)?;
    }
    Ok(Inspection {
        artifacts,
        attachments,
        users: state.users.len(),
        jobs: state.jobs.len(),
        templates: state.workflow_templates.len(),
        deliverables: state.deliverables.len(),
    })
}
impl<P: PortablePersistence> Store<P> {
    /// Export door de sleuteleigenaar zonder een globale key of tweede opslagverbinding.
    pub fn export_portable_state(&mut self) -> Result<PortableState> {
        self.persistence.export(&self.state)
    }
}
impl<P: Persistence> Store<P> {
    /// Herstelt pas na validatie en opslag met de sleutel van de bestemming.
    pub fn restore_portable_state(
        &mut self,
        encoded: &[u8],
        key: &str,
        now: &Timestamp,
        login_id: impl FnMut() -> spin_security::Result<String>,
    ) -> Result {
        now.time()?;
        let mut candidate = decode(encoded, key, login_id)?;
        normalize(&mut candidate, now)?;
        self.edit(|state| {
            *state = candidate;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Memory;
    use core::cell::Cell;
    struct Random(u8);
    impl Entropy for Random {
        fn fill(&mut self, b: &mut [u8]) -> spin_security::Result {
            self.0 += 1;
            b.fill(self.0);
            Ok(())
        }
    }
    #[test]
    fn restore_drops_machine_handles_and_never_publishes_failed_or_unknown_state() {
        let source = PersistedState::from_json(
            br#"{
          "users":{"u":{"id":"u","username":"derek","role":"admin","password_hash":"hash"}},
          "worker_token":"runner-secret","auth_sessions":{"s":{"user_id":"u"}},
          "clients":{"c":{"status":"online"}},"workflow_tokens":{"s":"token"},
          "compositions":{"c":{"runtime":{"status":"running"},"agent":{"stream_id":"stream"}}},
          "sessions":{"s":{"status":"running","activation_id":"a"}},
          "activations":{"a":{"status":"active"}},"turns":{"t":{"status":"running"}}
        }"#,
        )
        .unwrap();
        let backup = encrypt(&source, &Cipher::new([7; 32]), &mut Random(0)).unwrap();
        let fail = Cell::new(true);
        let mut store = Store::new(PersistedState::default(), Memory(&fail));
        let now = Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap();
        assert_eq!(
            store
                .restore_portable_state(backup.json.as_bytes(), &backup.master_key, &now, || Err(
                    spin_security::Error::Payload
                ))
                .unwrap_err(),
            Error::Storage(10)
        );
        assert!(!store.has_users());
        fail.set(false);
        store
            .restore_portable_state(backup.json.as_bytes(), &backup.master_key, &now, || {
                Err(spin_security::Error::Payload)
            })
            .unwrap();
        assert_eq!(store.state.worker_token, "runner-secret");
        assert!(store.state.auth_sessions.is_empty());
        assert!(store.state.workflow_tokens.is_empty());
        assert!(store.state.compositions.get("c").unwrap().runtime.is_none());
        assert!(store.state.compositions.get("c").unwrap().agent.is_none());
        assert_eq!(store.state.clients.get("c").unwrap().status, "offline");
        assert_eq!(
            store.state.sessions.get("s").unwrap().status,
            d::SESSION_FROZEN
        );
        assert!(
            store
                .state
                .sessions
                .get("s")
                .unwrap()
                .activation_id
                .is_empty()
        );
        let bad = backup
            .json
            .replacen("\"username\":", "\"unexpected\":true,\"username\":", 1);
        assert!(
            store
                .restore_portable_state(bad.as_bytes(), &backup.master_key, &now, || Err(
                    spin_security::Error::Payload
                ))
                .is_err()
        );
        assert_eq!(store.version(), 1);
        assert_eq!(
            inspect(backup.json.as_bytes(), &backup.master_key, || Err(
                spin_security::Error::Payload
            ))
            .unwrap()
            .users,
            1
        );
    }
}
