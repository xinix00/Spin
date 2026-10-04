//! De Store is de enige eigenaar van de duurzame Spin-graaf.
//!
//! Een mutatie publiceert haar kandidaat pas na geslaagde opslag. Klok en IDs
//! komen van de runtime; er zijn geen verborgen locks, threads of I/O-globals.
#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
extern crate alloc;
use spin_domain::{self as d, Name, Timestamp, TryClone, state::PersistedState};

mod blobs;
mod changes;
pub use blobs::{BlobInfo, BlobReply, BlobRequest};
pub use changes::{Change, diff};
mod artifacts;
mod attachments;
/// Portable backups valideren voordat ze de actieve staat vervangen.
pub mod backup;
mod clients;
mod code_review;
mod composition;
mod configuration;
mod git_accounts;
mod jobs;
mod logins;
mod recordings;
mod repositories;
mod sessions;
mod snapshot;
mod templates;
mod users;
/// Workflowhistorie en state-overgangen onder de Store-eigenaar.
pub mod workflow;
pub use logins::layer_key;
pub use snapshot::job_is_closed;

/// De fout van een Store-opdracht.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// Het opgevraagde object bestaat niet.
    NotFound,
    /// Een invariant of bevoegdheid weigert de opdracht.
    Conflict(&'static str),
    /// Geen uitvoerbare Session voor deze werker.
    NoWork,
    /// Alle logins van de credentiallaag zijn bezet of niet beschikbaar.
    LoginsBusy,
    /// Een activatie behoort niet meer bij de Session.
    StaleActivation,
    /// Een invoer- of allocatiefout.
    Data(d::Error),
    /// Een geheim of de entropiebron is ongeldig.
    Security(spin_security::Error),
    /// De opslag weigerde de kandidaat, met de oorspronkelijke backendcode.
    Storage(i32),
    /// Een onzekere commit vereist heropenen en herstel vóór verdere mutaties.
    StorageUncertain(i32),
    /// Een runtime leverde geen bruikbare identiteit.
    Identity(Name),
}
impl From<d::Error> for Error {
    fn from(e: d::Error) -> Self {
        Self::Data(e)
    }
}
impl From<spin_security::Error> for Error {
    fn from(e: spin_security::Error) -> Self {
        Self::Security(e)
    }
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::Conflict(reason) => write!(f, "{reason}: conflict"),
            Self::NoWork => f.write_str("no matching work"),
            Self::LoginsBusy => f.write_str("every login of the layer is in use"),
            Self::StaleActivation => f.write_str("stale activation"),
            Self::Data(e) => e.fmt(f),
            Self::Security(e) => e.fmt(f),
            Self::StorageUncertain(code) => write!(f, "state needs recovery: code={code}"),
            Self::Storage(code) => write!(f, "state storage failed: code={code}"),
            Self::Identity(id) => write!(f, "invalid generated identity: {id}"),
        }
    }
}
impl core::error::Error for Error {}
/// Het resultaat van een Store-opdracht.
pub type Result<T = ()> = core::result::Result<T, Error>;

/// De opslaggrens; de concrete adapter versleutelt geheimen vóór publicatie.
pub trait Persistence {
    /// Database statistics and optional Replica metadata, without credentials.
    fn storage_usage(&mut self) -> Result<Option<StorageUsage>> {
        Ok(None)
    }
    /// An unreplicated development database has an empty recovery catalog.
    fn replica_points(&mut self) -> Result<d::List<backup::ReplicaPoint>> {
        Ok(d::List::new())
    }
    /// Prepare an immutable uploaded database without modifying live tables.
    fn stage_restore(&mut self, _: backup::RestoreRequest) -> Result<backup::RestoreReply> {
        Err(Error::Conflict("database restore is not configured"))
    }
    /// Replace staged objects and destination-key state in one durable transaction.
    fn install_restore(&mut self, _: &PersistedState) -> Result {
        Err(Error::Conflict("database restore is not configured"))
    }
    /// Native backuptransport is beschikbaar wanneer de adapter het databasebestand bezit.
    fn backup(&mut self, _: backup::Request) -> Result<backup::Reply> {
        Err(Error::Conflict("database backup is not configured"))
    }
    /// De eigenaar krijgt tussen SQL-opdrachten een beurt voor Replica en onderhoud.
    fn maintain(&mut self, _: &Timestamp) -> Result {
        Ok(())
    }

    /// Publiceert atomair en duurzaam, of laat de vorige state intact.
    fn save(&mut self, state: &PersistedState) -> Result;
    /// Zoals [`Persistence::save`], met de entiteiten die sinds de vorige
    /// opslag veranderden; een adapter met rijen schrijft alleen die.
    fn save_changes(&mut self, state: &PersistedState, _changes: &[Change]) -> Result {
        self.save(state)
    }
    /// Blobopslag is beschikbaar wanneer de runtime een database levert.
    fn blob(&mut self, _: BlobRequest<'_>) -> Result<BlobReply> {
        Err(Error::Conflict("blob storage is not configured"))
    }
}
/// The owner measures the live database without loading object payloads.
pub struct StorageUsage {
    /// Allocated SQLite pages in bytes.
    pub database_bytes: i64,
    /// Total bytes in complete, deduplicated objects.
    pub object_bytes: i64,
    /// Complete object count.
    pub objects: i64,
    /// Public Replica status; null for local-only storage.
    pub replication: d::json::Value,
}

/// De runtime levert verse, cryptografisch willekeurige objectidentiteiten.
pub trait IdSource {
    /// Maakt een ID met de gevraagde prefix; fouten stoppen de hele mutatie.
    fn next(&mut self, prefix: &str) -> Result<alloc::string::String>;
}

/// Eén tijdstip en een expliciete ID-bron voor een samengestelde mutatie.
pub struct Mutation<'a> {
    /// De UTC-tijd van de volledige mutatie.
    pub now: &'a Timestamp,
    /// De ID-bron is eigendom van de aanroepende runtime.
    pub ids: &'a mut dyn IdSource,
}
impl Mutation<'_> {
    fn id(&mut self, prefix: &str) -> Result<alloc::string::String> {
        self.now.time()?;
        let id = self.ids.next(prefix)?;
        let suffix = id.strip_prefix(prefix).and_then(|s| s.strip_prefix('_'));
        if !suffix.is_some_and(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        }) {
            return Err(Error::Identity(Name::new(&id)));
        }
        Ok(id)
    }
}

/// De tijd en unieke ID van één opdracht, geleverd door de boot/runtime-schil.
pub struct Context<'a> {
    /// De UTC-tijd van de opdracht.
    pub now: &'a Timestamp,
    /// Een cryptografisch willekeurige ID met de passende prefix.
    pub id: &'a str,
}
impl Context<'_> {
    pub(crate) fn validate(&self) -> Result {
        if self.id.trim().is_empty() {
            return Err(Error::Identity(Name::new(self.id)));
        }
        self.now.time()?;
        Ok(())
    }
}

/// De app-taak bezit state en opslag en geeft uitsluitend snapshots af.
pub struct Store<P: Persistence> {
    state: PersistedState,
    persistence: P,
    version: u64,
    uncertain: Option<i32>,
    changes: changes::Log,
}
impl<P: Persistence> Store<P> {
    /// A read-only statistics request to the unique storage owner.
    pub fn storage_usage(&mut self) -> Result<Option<StorageUsage>> {
        self.persistence.storage_usage()
    }
    /// Onderhoud vindt uitsluitend plaats tussen volledige Store-opdrachten.
    pub fn maintain(&mut self, now: &Timestamp) -> Result {
        self.persistence.maintain(now)
    }

    /// Neemt reeds geladen en ontsleutelde state en de opslagadapter over.
    pub fn new(state: PersistedState, persistence: P) -> Self {
        Self {
            state,
            persistence,
            version: 0,
            uncertain: None,
            changes: changes::Log::default(),
        }
    }
    /// De entiteiten die na `version` veranderden; `None` als dat niet meer
    /// te zeggen is (te oud, of de hele state werd vervangen).
    pub fn changes_since(&self, version: u64) -> Result<Option<d::List<Change>>> {
        self.changes.since(version, self.version)
    }
    /// Het aantal bevestigde mutaties, voor samengevoegde browsernotificaties.
    pub fn version(&self) -> u64 {
        self.version
    }
    fn edit<T>(&mut self, change: impl FnOnce(&mut PersistedState) -> Result<T>) -> Result<T> {
        if let Some(code) = self.uncertain {
            return Err(Error::StorageUncertain(code));
        }
        let next_version = self
            .version
            .checked_add(1)
            .ok_or(Error::Conflict("state version exhausted"))?;
        let mut candidate = self.state.try_clone()?;
        let result = change(&mut candidate)?;
        let changes = changes::diff(&self.state, &candidate)?;
        if let Err(error) = self.persistence.save_changes(&candidate, &changes) {
            if let Error::StorageUncertain(code) = error {
                self.uncertain = Some(code);
            }
            return Err(error);
        }
        self.state = candidate;
        self.version = next_version;
        self.changes.push(next_version, changes);
        Ok(result)
    }
}

pub(crate) fn require_admin(state: &PersistedState, id: &str) -> Result {
    let user = state.users.get(id.trim()).ok_or(Error::NotFound)?;
    if user.role != d::USER_ADMIN || user.archived_at.is_some() {
        return Err(Error::Conflict("admin identity required"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    use d::Wire;
    pub(crate) struct Memory<'a>(pub &'a Cell<bool>);
    impl Persistence for Memory<'_> {
        fn save(&mut self, _: &PersistedState) -> Result {
            if self.0.get() {
                Err(Error::Storage(10))
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn failed_save_never_publishes_candidate() {
        let fail = Cell::new(true);
        let mut store = Store::new(PersistedState::default(), Memory(&fail));
        let now = Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap();
        let user =
            || d::User::from_json(br#"{"username":"Derek","password_hash":"hash"}"#).unwrap();
        assert_eq!(
            store
                .create_initial_user(
                    user(),
                    Context {
                        now: &now,
                        id: "usr-1"
                    }
                )
                .unwrap_err(),
            Error::Storage(10)
        );
        assert!(!store.has_users());
        assert_eq!(store.version(), 0);
        fail.set(false);
        assert!(
            store
                .create_initial_user(
                    user(),
                    Context {
                        now: &now,
                        id: "usr-1"
                    }
                )
                .is_ok()
        );
        assert!(store.has_users());
        assert_eq!(store.version(), 1);
    }
}
