//! Eén SQLite-epoch per opslagopdracht maakt de bestaande Replica-eigenaar bereikbaar.
//! De engine sluit vóór capture/upload; er bestaan nooit twee SQL-runtimes tegelijk.
use crate::{
    outbound::Dial,
    platform::{Environment, Random},
    s3::Network,
};
use alloc::{string::String, vec::Vec};
use applib::{App, appnet::Net};
use replica_core::{local::Name, owner::Replica, time::Time};
use replica_hopos::{Files, Wait};
use replica_sqlite::{Storage, asynchronous::Bridge};
use spin_domain::{Timestamp, Wire, state::PersistedState};
use spin_persistence::{Database, Error, MAX_STATE_BYTES};
use spin_security::Cipher;
use spin_store::{BlobReply, BlobRequest, Persistence};
mod backend;
mod restore;
mod upload;
pub(super) use backend::{Arena, Backend, FilesPool};
pub(super) use upload::Uploads;

pub(super) type Bucket<'a> = replica_s3::S3<Network, Wait<'a>>;
pub(super) struct Owner<'a> {
    heap: &'a Arena,
    backend: Backend<'a>,
    cipher: Cipher,
    entropy: Random,
    initialized: bool,
    poisoned: bool,
    exporting: Option<u64>,
    raw_upload: Option<restore::RawUpload>,
    next_upload: i64,
    importing: Option<restore::Import>,
    restore_fetch: Option<(String, Option<Time>)>,
    namespace: String,
    replica: Option<Replica>,
    bucket: Option<Bucket<'a>>,
    uploads: &'a Uploads,
    /// `<namespace>/lease`: de schrijverlease van Replica in de bucket.
    lease_key: String,
}
/// De schrijverlease: lang genoeg voor een onderhoudsbeurt van Replica op de
/// eigenaar (compactie haalde 17 s op een trage lijn), Replica vernieuwt hem
/// hoogstens eens per zesde en weigert writes een derde vóór het verlopen.
const LEASE_TTL_MS: u64 = 60_000;
enum Op<'a> {
    Usage,
    Load,
    Save(&'a [u8]),
    Blob(BlobRequest<'a>),
    Restore(&'a [u8]),
}
enum Reply {
    Usage(spin_persistence::Usage),
    State(Vec<u8>),
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
    applib::log!("SPIN_REPLICA_FAILED error={error:?}");
    spin_store::Error::Storage(10)
}
fn time(now: &Timestamp) -> spin_store::Result<Time> {
    Time::parse(now.as_str()).map_err(replica_error)
}
/// De grootte van de snapshot in de bucket: het totaal op de openingspagina
/// tijdens een herstel. Twee kleine GET's; 0 als er niets of iets onleesbaars is.
fn snapshot_bytes(store: &mut impl replica_core::object::Store, namespace: &str) -> u64 {
    let read = |store: &mut dyn FnMut(&str, usize) -> replica_core::Result<Vec<u8>>| -> replica_core::Result<u64> {
        let current = store(&replica_core::object::key(namespace, "/current")?, 255)?;
        let generation = core::str::from_utf8(&current)
            .map_err(|_| replica_core::Error::Corrupt)?
            .trim();
        let prefix = replica_core::replication::generation_prefix(namespace, generation)?;
        let bytes = store(
            &replica_core::object::key(&prefix, "snapshot")?,
            replica_core::manifest::MAX_MANIFEST_BYTES,
        )?;
        let manifest = replica_core::manifest::Manifest::decode(&bytes, &prefix)?;
        Ok(manifest.parts.iter().map(|part| part.size).sum())
    };
    read(&mut |key, limit| store.get(key, limit).map_err(Into::into)).unwrap_or(0)
}
fn wall() -> u64 {
    applib::app().and_then(|a| a.wall_ns()).unwrap_or(0) / 1_000_000_000
}
/// Alleen `SPIN_REPLICATION=off` laat een database zonder Replica draaien.
pub(super) fn replicating(app: &App) -> bool {
    !app.env("SPIN_REPLICATION")
        .unwrap_or("")
        .eq_ignore_ascii_case("off")
}
/// De S3-configuratie uit de omgeving; de eigenaar en zijn uploader delen haar,
/// ieder met een eigen verbinding.
pub(super) fn s3_client(app: &App) -> spin_store::Result<leans3::Client> {
    let env = |key| app.env(key).unwrap_or("").trim();
    for key in [
        "SPIN_S3_ENDPOINT",
        "SPIN_S3_BUCKET",
        "SPIN_S3_ACCESS_KEY",
        "SPIN_S3_SECRET_KEY",
    ] {
        if env(key).is_empty() {
            return Err(spin_store::Error::Conflict(
                "Replica needs SPIN_S3_* configuration; use SPIN_REPLICATION=off for development",
            ));
        }
    }
    Ok(leans3::Client {
        endpoint: spin_domain::try_string(env("SPIN_S3_ENDPOINT"))?,
        bucket: spin_domain::try_string(env("SPIN_S3_BUCKET"))?,
        region: spin_domain::try_string(if env("SPIN_S3_REGION").is_empty() {
            "us-east-1"
        } else {
            env("SPIN_S3_REGION")
        })?,
        access_key_id: spin_domain::try_string(env("SPIN_S3_ACCESS_KEY"))?,
        secret_access_key: spin_domain::try_string(env("SPIN_S3_SECRET_KEY"))?,
        session_token: String::new(),
        path_style: true,
        now: Some(wall),
    })
}
fn duration(value: &str, default: u64) -> spin_store::Result<u64> {
    if value.is_empty() {
        return Ok(default);
    }
    let nanos = hop_types::time::parse_duration(value)
        .map_err(|_| spin_store::Error::Conflict("invalid Replica duration"))?;
    if nanos < 3_600_000_000_000 {
        return Err(spin_store::Error::Conflict(
            "Replica duration must be at least one hour",
        ));
    }
    Ok(nanos / 1_000_000_000)
}
fn sql<B: Storage>(
    heap: &mut [u64],
    backend: &mut B,
    initialize: bool,
    op: Op<'_>,
) -> spin_persistence::Result<Reply> {
    // SAFETY: Owner holds the exclusive arena loan on its parked stack.
    // De engine en verbinding sluiten vóór deze functie de VFS/heap teruggeeft.
    let mut engine = unsafe { replica_sqlite::Engine::initialize(heap, backend) }?;
    let connection = engine.open(c"spin.sqlite")?;
    let mut db = if initialize {
        Database::open(connection)?
    } else {
        Database::attach(connection)?
    };
    match op {
        Op::Usage => Ok(Reply::Usage(db.usage()?)),
        Op::Load => match db.read_file("state", MAX_STATE_BYTES) {
            Ok(bytes) => Ok(Reply::State(bytes)),
            Err(Error::NotFound) => Ok(Reply::State(Vec::new())),
            Err(e) => Err(e),
        },
        Op::Save(bytes) => {
            db.write_file("state", bytes)?;
            Ok(Reply::Empty)
        }
        Op::Blob(request) => db.blob(request).map(Reply::Blob),
        Op::Restore(bytes) => {
            db.install_restore(bytes)?;
            Ok(Reply::Empty)
        }
    }
}
impl<'a> Owner<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        heap: &'a Arena,
        mut backend: Backend<'a>,
        cipher: Cipher,
        entropy: Random,
        app: &'static App,
        net: &'static Net,
        domain: &str,
        uploads: &'a Uploads,
        restore: alloc::rc::Rc<spin_runtime::Restore>,
    ) -> spin_store::Result<Self> {
        let wait = backend.wait;
        let mut replica = None;
        let mut bucket = None;
        let mut saved_namespace = String::new();
        let mut lease_key = String::new();
        if replicating(app) {
            let client = s3_client(app)?;
            let env = |key| app.env(key).unwrap_or("").trim();
            let prefix = env("SPIN_S3_PREFIX").trim_matches('/');
            let prefix = if prefix.is_empty() { "spin" } else { prefix };
            let domain = if domain.is_empty() { "spin" } else { domain };
            let namespace = spin_core::validation::text(format_args!("{prefix}/{domain}"))?;
            saved_namespace = spin_domain::try_string(&namespace)?;
            let destination = replica_core::marker::destination(
                env("SPIN_S3_ENDPOINT"),
                env("SPIN_S3_BUCKET"),
                prefix,
                domain,
            )
            .map_err(replica_error)?;
            let mut config = replica_core::owner::Config::new(
                &namespace,
                &destination,
                Name::new("spin.sqlite").map_err(replica_error)?,
                1 << 24,
            )
            .map_err(replica_error)?;
            config.schedule =
                replica_core::maintenance::Schedule::parse(env("SPIN_REPLICA_SCHEDULE"))
                    .map_err(replica_error)?;
            config.generation = duration(env("SPIN_REPLICA_GENERATION"), 7 * 86400)?;
            config.retention = duration(env("SPIN_REPLICA_RETENTION"), 28 * 86400)?;
            config.adopt_local = env("SPIN_REPLICA_ADOPT_LOCAL") == "1";
            let mut remote = Bucket::new(
                client,
                Network::new(Dial { app, net }).counting(restore.clone()),
                Wait(wait.0),
            )
            .map_err(|_| spin_store::Error::Conflict("invalid Replica S3 configuration"))?;
            restore.total.set(snapshot_bytes(&mut remote, &namespace));
            // Eén schrijver per namespace: wie de lease niet krijgt, wacht tot de
            // vorige houder hem vrijgeeft of laat verlopen (een herstart van
            // dezelfde node wacht zijn vorige leven af).
            let key = spin_core::validation::text(format_args!("{namespace}/lease"))?;
            let node = spin_core::validation::text(format_args!("hopos/{domain}"))?;
            let writer = loop {
                let now =
                    crate::platform::timestamp(app).map_err(|_| spin_store::Error::Storage(10))?;
                match replica_core::writer::claim(
                    &mut backend,
                    &mut remote.lease(&key, LEASE_TTL_MS),
                    &node,
                    LEASE_TTL_MS,
                    time(&now)?,
                )
                .map_err(replica_error)?
                {
                    replica_core::writer::Role::Writer(writer) => break writer,
                    replica_core::writer::Role::Reader { leader } => {
                        applib::log!(
                            "SPIN_REPLICA_LEASE_WAIT leader={}",
                            leader.as_deref().unwrap_or("unknown")
                        );
                        wait.0
                            .wait(applib::EXEC.get().after(core::time::Duration::from_secs(5)))
                            .map_err(|_| spin_store::Error::Storage(10))?;
                    }
                }
            };
            lease_key = key;
            let now =
                crate::platform::timestamp(app).map_err(|_| spin_store::Error::Storage(10))?;
            let owner = Replica::prepare(
                &mut backend,
                &mut remote,
                writer,
                config,
                time(&now)?,
                |storage, path| {
                    let mut arena = heap.take(wait)?;
                    // SAFETY: The exclusive arena loan excludes every other SQLite engine.
                    let mut engine =
                        unsafe { replica_sqlite::Engine::initialize(&mut arena, storage) }?;
                    let mut db = engine.open(path.cstr()?)?;
                    let mut query = db.prepare(c"PRAGMA quick_check")?;
                    if !query.step()? || query.column(0)? != replica_sqlite::Value::Text("ok") {
                        return Err(replica_core::Error::Corrupt);
                    }
                    Ok(())
                },
            )
            .map_err(replica_error)?;
            applib::log!("SPIN_REPLICA_READY reason={:?}", owner.status().reason);
            replica = Some(owner);
            bucket = Some(remote);
        }
        Ok(Self {
            heap,
            backend,
            cipher,
            entropy,
            initialized: false,
            poisoned: false,
            exporting: None,
            raw_upload: None,
            next_upload: -1,
            importing: None,
            restore_fetch: None,
            namespace: saved_namespace,
            replica,
            bucket,
            uploads,
            lease_key,
        })
    }
    fn execute(&mut self, op: Op<'_>) -> spin_store::Result<Reply> {
        if self.exporting.is_some() {
            return Err(spin_store::Error::Conflict(
                "database writes paused for backup",
            ));
        }
        if self.poisoned {
            return Err(spin_store::Error::StorageUncertain(10));
        }
        let mut heap = self
            .heap
            .take(self.backend.wait)
            .map_err(Error::from)
            .map_err(store_error)?;
        let result = if let Some(replica) = &mut self.replica {
            let mut tracked = replica.vfs(&mut self.backend).map_err(replica_error)?;
            sql(&mut heap, &mut tracked, !self.initialized, op)
        } else {
            sql(&mut heap, &mut self.backend, !self.initialized, op)
        };
        match result {
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
    pub(super) fn load(
        &mut self,
        ids: impl FnMut() -> spin_security::Result<String>,
    ) -> spin_store::Result<PersistedState> {
        let Reply::State(bytes) = self.execute(Op::Load)? else {
            return Err(spin_store::Error::Storage(21));
        };
        applib::log!("SPIN_STATE_LOADED bytes={}", bytes.len());
        // Loading has first recovered any live SQLite journal. Interrupted uploads and
        // read-only import staging can now be discarded under the database lease.
        self.restore_step(spin_store::backup::RestoreRequest::Abort)?;
        if bytes.is_empty() {
            return Ok(PersistedState::default());
        }
        let sealed = PersistedState::from_json_with_limit(&bytes, MAX_STATE_BYTES)?;
        let mut state = self.cipher.decrypt_state(&sealed, ids)?;
        state.normalize_loaded()?;
        applib::log!("SPIN_STATE_DECRYPTED users={}", state.users.len());
        self.save(&state)?;
        Ok(state)
    }
}
impl Persistence for Owner<'_> {
    fn storage_usage(&mut self) -> spin_store::Result<Option<spin_store::StorageUsage>> {
        use spin_domain::json::{Object, Value};
        let Reply::Usage(usage) = self.execute(Op::Usage)? else {
            return Err(spin_store::Error::Storage(21));
        };
        let mut replication = Value::Null;
        if let Some(replica) = &self.replica {
            let status = replica.status();
            let marker = replica.marker();
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
            replication = Value::Object(value);
        }
        Ok(Some(spin_store::StorageUsage {
            database_bytes: usage.database_bytes,
            object_bytes: usage.object_bytes,
            objects: usage.objects,
            replication,
        }))
    }
    fn replica_points(
        &mut self,
    ) -> spin_store::Result<spin_domain::List<spin_store::backup::ReplicaPoint>> {
        let mut out = spin_domain::List::new();
        if let (Some(replica), Some(bucket)) = (&self.replica, &mut self.bucket) {
            for point in
                replica_core::archive::points(bucket, &self.namespace, replica.marker(), 4096)
                    .map_err(replica_error)?
            {
                out.push(spin_store::backup::ReplicaPoint {
                    generation: point.generation,
                    at: point.at.encode().map_err(replica_error)?,
                    level: point.level,
                    current: point.current,
                })?;
            }
        }
        Ok(out)
    }
    fn stage_restore(
        &mut self,
        request: spin_store::backup::RestoreRequest,
    ) -> spin_store::Result<spin_store::backup::RestoreReply> {
        self.restore_step(request)
    }
    fn install_restore(&mut self, state: &PersistedState) -> spin_store::Result {
        self.restore_install(state)
    }
    fn backup(
        &mut self,
        request: spin_store::backup::Request,
    ) -> spin_store::Result<spin_store::backup::Reply> {
        use spin_store::backup::{Reply as R, Request as Q};
        if matches!(request, Q::End) {
            self.exporting = None;
            return Ok(R::Done);
        }
        if self.poisoned {
            return Err(spin_store::Error::StorageUncertain(10));
        }
        let name = Name::new("spin.sqlite").map_err(replica_error)?;
        match request {
            Q::Begin => {
                if self.exporting.is_some() {
                    return Err(spin_store::Error::Conflict("backup already active"));
                }
                // Every SQL epoch has closed and synchronous=FULL has committed its pages.
                let key = self.cipher.portable_key()?;
                let mut file = replica_core::local::File::open(&mut self.backend, &name, false)
                    .map_err(replica_error)?;
                file.sync().map_err(replica_error)?;
                let size = file.size().map_err(replica_error)?;
                file.close().map_err(replica_error)?;
                if size == 0 || size > spin_core::backup::MAX_DATABASE {
                    return Err(spin_store::Error::Conflict(
                        "database exceeds backup budget",
                    ));
                }
                self.exporting = Some(size);
                Ok(R::Ready { size, key })
            }
            Q::Read { offset, length } => {
                let size = self
                    .exporting
                    .ok_or(spin_store::Error::Conflict("backup is not active"))?;
                if length == 0
                    || length > spin_core::backup::CHUNK
                    || offset.checked_add(length as u64).is_none_or(|n| n > size)
                {
                    return Err(spin_store::Error::Conflict("invalid backup read range"));
                }
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(length)
                    .map_err(|_| spin_domain::Error::OutOfMemory)?;
                bytes.resize(length, 0);
                let mut file = replica_core::local::File::open(&mut self.backend, &name, false)
                    .map_err(replica_error)?;
                if file.size().map_err(replica_error)? != size {
                    return Err(spin_store::Error::Conflict(
                        "database changed during backup",
                    ));
                }
                file.read(offset, &mut bytes).map_err(replica_error)?;
                file.close().map_err(replica_error)?;
                Ok(R::Bytes(bytes))
            }
            Q::End => Ok(R::Done),
        }
    }
    fn save(&mut self, state: &PersistedState) -> spin_store::Result {
        let bytes = self
            .cipher
            .encrypt_state(state, &mut self.entropy)?
            .to_json()?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(spin_store::Error::Conflict("state exceeds storage budget"));
        }
        if let Err(error) = self.execute(Op::Save(bytes.as_bytes())) {
            applib::log!("SPIN_STATE_SAVE_FAILED bytes={} error={error}", bytes.len());
            return Err(error);
        }
        Ok(())
    }
    fn blob(&mut self, request: BlobRequest<'_>) -> spin_store::Result<BlobReply> {
        if restore::raw_request(&request) {
            return self.raw_blob(request);
        }
        match self.execute(Op::Blob(request))? {
            Reply::Blob(reply) => Ok(reply),
            _ => Err(spin_store::Error::Storage(21)),
        }
    }
    /// De Replica-beurt in drie stappen: de capture hier, de upload van de delen
    /// op de stack van de uploader, de afronding weer hier. Zo bedient deze
    /// eigenaar verzoeken terwijl de delen naar S3 gaan.
    fn maintain(&mut self, now: &Timestamp) -> spin_store::Result {
        if self.poisoned {
            return Err(spin_store::Error::StorageUncertain(10));
        }
        let (Some(replica), Some(bucket)) = (&mut self.replica, &mut self.bucket) else {
            return Ok(());
        };
        let now = time(now)?;
        // Elke seconde: Replica schrijft hoogstens eens per zesde TTL. Een
        // transportfout is nog geen verlies; pas na de deadline sluit de eigenaar.
        if let Err(error) = replica.renew(&mut bucket.lease(&self.lease_key, LEASE_TTL_MS), now) {
            applib::log!("SPIN_REPLICA_LEASE_FAILED error={error:?}");
            if error == replica_core::Error::LeaseLost {
                self.poisoned = true;
                return Err(spin_store::Error::StorageUncertain(10));
            }
        }
        if let Some((pending, uploaded)) = self.uploads.take_done() {
            if replica
                .finish(&mut self.backend, bucket, pending, uploaded, now)
                .map_err(replica_error)?
                .published
            {
                applib::log!("SPIN_REPLICA_SYNCED");
            }
            return Ok(());
        }
        if self.uploads.busy() {
            return Ok(());
        }
        if let Some(pending) = replica
            .begin(&mut self.backend, bucket, now)
            .map_err(replica_error)?
        {
            let database = spin_domain::try_string(self.backend.database())?;
            self.uploads.start(pending, database);
        }
        Ok(())
    }
}
