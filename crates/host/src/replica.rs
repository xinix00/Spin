//! Optionele Replica-replicatie naar S3 voor de hostserver, naar het voorbeeld van
//! de HopOS-eigenaar: Prepare vóór SQLite, iedere SQL-opdracht een eigen engine
//! over de getrackte VFS, en de Replica-beurt tussen de opdrachten.
use crate::{
    s3::{Block, Network},
    storage::{Files, Random},
};
use replica_core::{
    local::Name,
    object::Store,
    owner::{Config, Replica},
    time::Time,
};
use replica_sqlite::Storage;
use spin_domain::{Timestamp, Wire, state::PersistedState};
use spin_persistence::{Database, Error, MAX_STATE_BYTES};
use spin_security::Cipher;
use spin_store::{BlobReply, BlobRequest, Persistence};

/// De S3- en Replica-configuratie uit de omgeving.
pub struct Settings {
    /// SigV4-client voor de bucket.
    pub client: leans3::Client,
    /// Namespace `<prefix>/<domain>`, bestemming en cadans.
    pub config: Config,
}
fn invalid(message: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(message.to_string())
}
fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
fn duration(key: &str, value: &str, default: u64) -> std::io::Result<u64> {
    if value.is_empty() {
        return Ok(default);
    }
    let nanos = hop_types::time::parse_duration(value)
        .map_err(|_| invalid(format_args!("{key} is not a duration")))?;
    if nanos < 3_600_000_000_000 {
        return Err(invalid(format_args!("{key} must be at least one hour")));
    }
    Ok(nanos / 1_000_000_000)
}
/// Leest de configuratie; `None` zonder SPIN_S3_ENDPOINT: dan blijft dit een gewone
/// devserver op zijn eigen volume.
pub fn settings(env: impl Fn(&str) -> String) -> std::io::Result<Option<Settings>> {
    let env = |key: &str| env(key).trim().to_owned();
    if env("SPIN_S3_ENDPOINT").is_empty() {
        return Ok(None);
    }
    for key in [
        "SPIN_S3_BUCKET",
        "SPIN_S3_ACCESS_KEY",
        "SPIN_S3_SECRET_KEY",
        "SPIN_S3_PREFIX",
    ] {
        if env(key).is_empty() {
            return Err(invalid(format_args!(
                "SPIN_S3_ENDPOINT is set, so replication also needs {key}"
            )));
        }
    }
    // Eén schrijver per namespace: de Replica-lease leeft alleen op de lokale
    // schijf. Draait dezelfde namespace al elders (bijvoorbeeld op HopOS), stop
    // die eerst; de operator beslist, niet deze code.
    let prefix = env("SPIN_S3_PREFIX");
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        return Err(invalid("SPIN_S3_PREFIX must not be empty"));
    }
    let domain = env("SPIN_DOMAIN");
    let domain = if domain.is_empty() { "local" } else { &domain };
    let endpoint = env("SPIN_S3_ENDPOINT");
    let bucket = env("SPIN_S3_BUCKET");
    let replica = |e: replica_core::Error| invalid(format_args!("invalid Replica setting: {e:?}"));
    let destination =
        replica_core::marker::destination(&endpoint, &bucket, prefix, domain).map_err(replica)?;
    let mut config = Config::new(
        &format!("{prefix}/{domain}"),
        &destination,
        Name::new("spin.sqlite").map_err(replica)?,
        1 << 24,
    )
    .map_err(replica)?;
    config.schedule = replica_core::maintenance::Schedule::parse(&env("SPIN_REPLICA_SCHEDULE"))
        .map_err(replica)?;
    config.generation = duration(
        "SPIN_REPLICA_GENERATION",
        &env("SPIN_REPLICA_GENERATION"),
        7 * 86400,
    )?;
    config.retention = duration(
        "SPIN_REPLICA_RETENTION",
        &env("SPIN_REPLICA_RETENTION"),
        28 * 86400,
    )?;
    config.adopt_local = env("SPIN_REPLICA_ADOPT_LOCAL") == "1";
    let region = env("SPIN_S3_REGION");
    Ok(Some(Settings {
        client: leans3::Client {
            endpoint,
            bucket,
            region: if region.is_empty() {
                "us-east-1".to_owned()
            } else {
                region
            },
            access_key_id: env("SPIN_S3_ACCESS_KEY"),
            secret_access_key: env("SPIN_S3_SECRET_KEY"),
            session_token: String::new(),
            path_style: true,
            now: Some(unix_seconds),
        },
        config,
    }))
}
/// De S3-bucket over de host-TLS-dialer.
pub type Bucket = replica_s3::S3<Network, Block>;
/// De S3-bucket over de host-TLS-dialer.
pub fn bucket(client: leans3::Client) -> std::io::Result<Bucket> {
    replica_s3::S3::new(client, Network::new(), Block)
        .map_err(|_| invalid("invalid Replica S3 configuration"))
}
/// Een bucket die ook de schrijverlease van Replica levert (`S3::lease`).
pub trait Leased: Store {
    /// De lease-backend voor één aanroep.
    type Lease<'a>: replica_core::writer::Backend
    where
        Self: 'a;
    /// De lease op `key` met termijn `ttl_ms`.
    fn lease<'a>(&'a mut self, key: &'a str, ttl_ms: u64) -> Self::Lease<'a>;
}
impl Leased for Bucket {
    type Lease<'a> = replica_s3::Lease<'a, Network, Block>;
    fn lease<'a>(&'a mut self, key: &'a str, ttl_ms: u64) -> Self::Lease<'a> {
        replica_s3::S3::lease(self, key, ttl_ms)
    }
}
/// De lease van de host: ruim, want de host uploadt inline en vernieuwt pas
/// na een beurt (de upload van een volledige snapshot kan minuten duren).
pub const LEASE_TTL_MS: u64 = 300_000;
/// `<namespace>/lease`, zoals op HopOS.
pub fn lease_key(config: &Config) -> String {
    format!("{}/lease", config.namespace)
}
fn now(timestamp: &Timestamp) -> replica_core::Result<Time> {
    Time::parse(timestamp.as_str())
}
/// Replica::prepare vóór SQLite opent: herstelt uit S3 als de namespace een
/// generatie heeft en de lokale database ontbreekt of achterloopt.
pub fn prepare<S: Leased>(
    files: &mut Files,
    heap: &mut [u64],
    store: &mut S,
    config: Config,
) -> std::io::Result<Replica> {
    let key = lease_key(&config);
    let node = format!(
        "macos/{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_owned())
    );
    // Eén schrijver per namespace: wacht tot een andere houder (bijvoorbeeld
    // HopOS) de lease vrijgeeft of laat verlopen.
    let writer = loop {
        let at = now(&crate::server::timestamp()?).map_err(|e| invalid(format_args!("{e:?}")))?;
        match replica_core::writer::claim(
            files,
            &mut store.lease(&key, LEASE_TTL_MS),
            &node,
            LEASE_TTL_MS,
            at,
        )
        .map_err(|e| invalid(format_args!("Replica lease failed: {e:?}")))?
        {
            replica_core::writer::Role::Writer(writer) => break writer,
            replica_core::writer::Role::Reader { leader } => {
                eprintln!(
                    "SPIN_REPLICA_LEASE_WAIT leader={}",
                    leader.as_deref().unwrap_or("unknown")
                );
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }
    };
    let at = now(&crate::server::timestamp()?).map_err(|e| invalid(format_args!("{e:?}")))?;
    let replica = Replica::prepare(files, store, writer, config, at, |storage, path| {
        // SAFETY: Er draait nog geen andere SQLite-runtime; deze engine sluit
        // vóór Prepare verdergaat, en alles blijft op deze thread.
        let mut engine = unsafe { replica_sqlite::Engine::initialize(heap, storage) }?;
        let mut db = engine.open(path.cstr()?)?;
        let mut query = db.prepare(c"PRAGMA quick_check")?;
        if !query.step()? || query.column(0)? != replica_sqlite::Value::Text("ok") {
            return Err(replica_core::Error::Corrupt);
        }
        Ok(())
    })
    .map_err(|e| invalid(format_args!("Replica prepare failed: {e:?}")))?;
    eprintln!("SPIN_REPLICA_READY reason={:?}", replica.status().reason);
    Ok(replica)
}
enum Op<'a> {
    Usage,
    Load,
    /// Alleen de gewijzigde rijen.
    Save(&'a [spin_persistence::Row]),
    /// De hele state, ook bij de migratie van de oude enkele rij.
    Replace(&'a [spin_persistence::Row]),
    Blob(BlobRequest<'a>),
    /// Een begrensde opruimstap van dode objecten.
    Purge,
}
enum Reply {
    Purged(bool),
    Usage(spin_persistence::Usage),
    State(Vec<u8>, bool),
    Empty,
    Blob(BlobReply),
}
fn store_error(error: Error) -> spin_store::Error {
    match error {
        Error::Sql(e) => spin_store::Error::Storage(e.code),
        Error::Uncertain(c) => spin_store::Error::StorageUncertain(c),
        Error::Data(e) => e.into(),
        Error::Security(e) => e.into(),
        Error::NotFound => spin_store::Error::NotFound,
        Error::Invalid(s) => spin_store::Error::Conflict(s),
    }
}
fn replica_error(error: replica_core::Error) -> spin_store::Error {
    eprintln!("SPIN_REPLICA_FAILED error={error:?}");
    spin_store::Error::Storage(10)
}
fn sql<B: Storage>(
    heap: &mut [u64],
    backend: &mut B,
    initialize: bool,
    op: Op<'_>,
) -> spin_persistence::Result<Reply> {
    // SAFETY: De eigenaar leent heap en VFS exclusief; engine en verbinding
    // sluiten vóór deze functie terugkeert, dus er is nooit een tweede runtime.
    let mut engine = unsafe { replica_sqlite::Engine::initialize(heap, backend) }?;
    let connection = engine.open(c"spin.sqlite")?;
    let mut db = if initialize {
        Database::open(connection)?
    } else {
        Database::attach(connection)?
    };
    match op {
        Op::Usage => Ok(Reply::Usage(db.usage()?)),
        Op::Load => match db.read_state(MAX_STATE_BYTES)? {
            Some((bytes, legacy)) => Ok(Reply::State(bytes, legacy)),
            None => Ok(Reply::State(Vec::new(), false)),
        },
        Op::Save(rows) => {
            db.write_rows(rows)?;
            Ok(Reply::Empty)
        }
        Op::Replace(rows) => {
            db.replace_rows(rows)?;
            Ok(Reply::Empty)
        }
        Op::Blob(request) => db.blob(request).map(Reply::Blob),
        Op::Purge => db.purge_step(16).map(Reply::Purged),
    }
}
/// De gerepliceerde opslageigenaar; SQL is gesloten wanneer Replica een beurt krijgt.
pub struct Owner<S: Leased> {
    heap: Vec<u64>,
    files: Files,
    cipher: Cipher,
    entropy: Random,
    replica: Replica,
    store: S,
    lease_key: String,
    initialized: bool,
    poisoned: bool,
    /// Er kunnen dode objecten liggen: na de start en na elke blobopdracht.
    purging: bool,
}
impl<S: Leased> Owner<S> {
    /// Neemt de voorbereide Replica, de bestanden en de SQLite-heap over;
    /// `lease_key` is dezelfde als bij [`prepare`] ([`lease_key`]).
    pub fn new(
        heap: Vec<u64>,
        files: Files,
        cipher: Cipher,
        entropy: Random,
        replica: Replica,
        store: S,
        lease_key: String,
    ) -> Self {
        Self {
            heap,
            files,
            cipher,
            entropy,
            replica,
            store,
            lease_key,
            initialized: false,
            poisoned: false,
            purging: true,
        }
    }
    fn execute(&mut self, op: Op<'_>) -> spin_store::Result<Reply> {
        if self.poisoned {
            return Err(spin_store::Error::StorageUncertain(10));
        }
        let mut tracked = self.replica.vfs(&mut self.files).map_err(replica_error)?;
        match sql(&mut self.heap, &mut tracked, !self.initialized, op) {
            Ok(reply) => {
                self.initialized = true;
                Ok(reply)
            }
            Err(error) => {
                if matches!(error, Error::Uncertain(_)) {
                    self.poisoned = true;
                }
                Err(store_error(error))
            }
        }
    }
    /// Leest en migreert de state; een verkeerde sleutel levert geen lege Store op.
    pub fn load(
        &mut self,
        ids: impl FnMut() -> spin_security::Result<String>,
    ) -> spin_store::Result<PersistedState> {
        let Reply::State(bytes, legacy) = self.execute(Op::Load)? else {
            return Err(spin_store::Error::Storage(21));
        };
        if bytes.is_empty() {
            return Ok(PersistedState::default());
        }
        let sealed = PersistedState::from_json_with_limit(&bytes, MAX_STATE_BYTES)?;
        let loaded = self.cipher.decrypt_state(&sealed, ids)?;
        let mut state = spin_domain::TryClone::try_clone(&loaded)?;
        state.normalize_loaded()?;
        // De oude enkele rij wordt eenmalig rijen; daarna alleen wat de
        // normalisatie veranderde.
        if legacy {
            self.save(&state)?;
        } else {
            self.save_changes(&state, &spin_store::diff(&loaded, &state)?)?;
        }
        Ok(state)
    }
}
impl<S: Leased> Persistence for Owner<S> {
    fn storage_usage(&mut self) -> spin_store::Result<Option<spin_store::StorageUsage>> {
        use spin_domain::json::{Object, Value};
        let Reply::Usage(usage) = self.execute(Op::Usage)? else {
            return Err(spin_store::Error::Storage(21));
        };
        let status = self.replica.status();
        let marker = self.replica.marker();
        let mut value = Object::new();
        value.push("generation", Value::string(&marker.generation)?)?;
        value.push("complete", Value::Bool(marker.complete))?;
        value.push("clean", Value::Bool(marker.clean))?;
        value.push(
            "restored",
            Value::Bool(matches!(
                status.reason,
                replica_core::prepare::Reason::Restored
            )),
        )?;
        value.push(
            "last_sync_at",
            match status.synced {
                Some(time) => Value::string(&time.encode().map_err(replica_error)?)?,
                None => Value::Null,
            },
        )?;
        value.push(
            "last_error",
            Value::string(&match status.error {
                Some(error) => spin_core::validation::text(format_args!("{error:?}"))?,
                None => String::new(),
            })?,
        )?;
        Ok(Some(spin_store::StorageUsage {
            database_bytes: usage.database_bytes,
            object_bytes: usage.object_bytes,
            objects: usage.objects,
            replication: Value::Object(value),
        }))
    }
    fn save(&mut self, state: &PersistedState) -> spin_store::Result {
        let rows = spin_persistence::state_rows(&self.cipher, &mut self.entropy, state, None)
            .map_err(spin_persistence::persistence_to_store)?;
        self.execute(Op::Replace(&rows)).map(|_| ())
    }
    fn save_changes(
        &mut self,
        state: &PersistedState,
        changes: &[spin_store::Change],
    ) -> spin_store::Result {
        let rows =
            spin_persistence::state_rows(&self.cipher, &mut self.entropy, state, Some(changes))
                .map_err(spin_persistence::persistence_to_store)?;
        if rows.is_empty() {
            return Ok(());
        }
        self.execute(Op::Save(&rows)).map(|_| ())
    }
    fn blob(&mut self, request: BlobRequest<'_>) -> spin_store::Result<BlobReply> {
        self.purging = true;
        match self.execute(Op::Blob(request))? {
            Reply::Blob(reply) => Ok(reply),
            _ => Err(spin_store::Error::Storage(21)),
        }
    }
    /// De host is één testproces: capture, upload en afronding lopen hier inline
    /// via `tick`, dus tijdens een upload wachten de verzoeken (HopOS uploadt op
    /// een eigen stack). Vóór het interval doet `tick` geen I/O.
    fn maintain(&mut self, now: &Timestamp) -> spin_store::Result {
        if self.poisoned {
            return Err(spin_store::Error::StorageUncertain(10));
        }
        // Grote blobs gaan in stukken weg: hoogstens 16 MiB per seconde-beurt.
        if self.purging {
            match self.execute(Op::Purge)? {
                Reply::Purged(more) => self.purging = more,
                _ => return Err(spin_store::Error::Storage(21)),
            }
        }
        let at = self::now(now).map_err(replica_error)?;
        if let Err(error) = self
            .replica
            .renew(&mut self.store.lease(&self.lease_key, LEASE_TTL_MS), at)
        {
            eprintln!("SPIN_REPLICA_LEASE_FAILED error={error:?}");
            if error == replica_core::Error::LeaseLost {
                self.poisoned = true;
                return Err(spin_store::Error::StorageUncertain(10));
            }
        }
        if let Some(synced) = self
            .replica
            .tick(&mut self.files, &mut self.store, at)
            .map_err(replica_error)?
            && synced.published
        {
            eprintln!("SPIN_REPLICA_SYNCED");
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> String + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
                .unwrap_or_default()
        }
    }
    const COMPLETE: &[(&str, &str)] = &[
        ("SPIN_S3_ENDPOINT", "http://127.0.0.1:9000"),
        ("SPIN_S3_BUCKET", "spin-test"),
        ("SPIN_S3_ACCESS_KEY", "key"),
        ("SPIN_S3_SECRET_KEY", "secret"),
        ("SPIN_S3_PREFIX", "macbook"),
    ];
    fn with(key: &str, value: &str) -> Vec<(&'static str, String)> {
        let mut out: Vec<_> = COMPLETE
            .iter()
            .filter(|(k, _)| *k != key)
            .map(|(k, v)| (*k, (*v).to_owned()))
            .collect();
        if let Some(k) = COMPLETE.iter().map(|(k, _)| *k).find(|k| *k == key) {
            out.push((k, value.to_owned()));
        }
        out
    }
    fn result(pairs: &[(&'static str, String)]) -> std::io::Result<Option<Settings>> {
        let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        settings(env(&pairs))
    }
    #[test]
    fn replication_is_off_without_endpoint() {
        assert!(settings(env(&[])).unwrap().is_none());
        assert!(result(&with("SPIN_S3_ENDPOINT", "")).unwrap().is_none());
    }
    #[test]
    fn complete_configuration_uses_prefix_and_local_domain() {
        let settings = settings(env(COMPLETE)).unwrap().unwrap();
        assert_eq!(settings.config.namespace, "macbook/local");
        assert_eq!(settings.client.region, "us-east-1");
        assert!(!settings.config.adopt_local);
    }
    #[test]
    fn refuses_missing_variables() {
        for key in [
            "SPIN_S3_BUCKET",
            "SPIN_S3_ACCESS_KEY",
            "SPIN_S3_SECRET_KEY",
            "SPIN_S3_PREFIX",
        ] {
            let error = result(&with(key, "")).err().unwrap();
            assert!(error.to_string().contains(key), "{error}");
        }
    }
    #[test]
    fn prefix_is_required_and_trimmed() {
        assert!(result(&with("SPIN_S3_PREFIX", "/")).is_err());
        assert!(result(&with("SPIN_S3_PREFIX", "/spin/")).unwrap().is_some());
    }
}
