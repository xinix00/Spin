//! Spins SQLite-schema boven Replica's veilige, single-owner verbinding.
//!
//! De runtime initialiseert Replica en levert de VFS. Deze crate bevat geen
//! C-callbacks, executor, filesystem of verborgen tweede databaseverbinding.
#![no_std]
#![forbid(unsafe_code)]
extern crate alloc;
mod restore;
mod rows;
mod uploads;
use alloc::{string::String, vec::Vec};
use core::ffi::CStr;
pub use replica_sqlite::{Connection, Storage};
use replica_sqlite::{Statement, Value};
pub use rows::{Row, state_rows};
use spin_domain::{self as d, TryClone, Wire, state::PersistedState, try_string};
use spin_security::{Cipher, Entropy, Sha256};

/// De bestaande chunkgrens; nooit een volledig Docker-image in het geheugen.
pub const BLOB_CHUNK_SIZE: usize = 1 << 20;
/// De runtime kiest een kleinere limiet waar zijn geheugenbudget dat vereist.
pub const MAX_STATE_BYTES: usize = 64 << 20;
/// Fixed SQLite workspace: transient bindings and records coexist with cached pages.
pub const SQLITE_HEAP_BYTES: usize = 4 * MAX_STATE_BYTES;
/// Fouten bewaren de SQLite-code en onderscheiden een onzekere commit.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// Een logische sleutel of blob ontbreekt.
    NotFound,
    /// Invoer of opgeslagen data schendt een invariant.
    Invalid(&'static str),
    /// SQLite of de VFS weigerde de operatie.
    Sql(replica_sqlite::Error),
    /// Een allocatie of parser faalde.
    Data(d::Error),
    /// Versleuteling, sleutel of entropie faalde.
    Security(spin_security::Error),
    /// Deze verbinding moet sluiten en herstellen vóór er meer opdrachten komen.
    Uncertain(i32),
}
impl From<replica_sqlite::Error> for Error {
    fn from(e: replica_sqlite::Error) -> Self {
        Self::Sql(e)
    }
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
            Self::Invalid(s) => f.write_str(s),
            Self::Sql(e) => e.fmt(f),
            Self::Data(e) => e.fmt(f),
            Self::Security(e) => e.fmt(f),
            Self::Uncertain(code) => write!(f, "database needs recovery: code={code}"),
        }
    }
}
impl core::error::Error for Error {}
/// Een faalbare opslagbewerking.
pub type Result<T = ()> = core::result::Result<T, Error>;
fn copy(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(bytes.len())
        .map_err(|_| d::Error::OutOfMemory)?;
    out.extend_from_slice(bytes);
    Ok(out)
}
fn integer(statement: &mut Statement<'_>, column: u32) -> Result<i64> {
    match statement.column(column)? {
        Value::Integer(n) => Ok(n),
        _ => Err(Error::Invalid("expected SQLite integer")),
    }
}
fn string(statement: &mut Statement<'_>, column: u32) -> Result<String> {
    match statement.column(column)? {
        Value::Text(s) => Ok(try_string(s)?),
        _ => Err(Error::Invalid("expected SQLite text")),
    }
}
fn digest(hash: Sha256) -> Result<String> {
    let mut out = try_string("sha256:")?;
    for byte in hash.finish() {
        let hex = b"0123456789abcdef";
        let chars = [
            *hex.get(usize::from(byte >> 4))
                .ok_or(Error::Invalid("hex"))?,
            *hex.get(usize::from(byte & 15))
                .ok_or(Error::Invalid("hex"))?,
        ];
        d::try_push_str(
            &mut out,
            core::str::from_utf8(&chars).map_err(|_| Error::Invalid("hex"))?,
        )?;
    }
    Ok(out)
}
/// De metadata van een complete blob; de hash controleert de volledige inhoud.
#[derive(Debug, PartialEq)]
pub struct BlobInfo {
    /// De logische referentie.
    pub reference: String,
    /// SHA-256 met het bestaande prefix.
    pub digest: String,
    /// Bijvoorbeeld `docker-snapshot` of `attachment`.
    pub kind: String,
    /// Het totale aantal bytes.
    pub size: i64,
}
/// De omvang van de database en zijn complete objecten.
#[derive(Debug, PartialEq)]
pub struct Usage {
    /// Het aantal pagina's maal hun grootte.
    pub database_bytes: i64,
    /// De totale logische objectomvang.
    pub object_bytes: i64,
    /// Het aantal gededupliceerde objecten.
    pub objects: i64,
}
/// Eén verbinding is van één eigenaar; een commitfout sluit verdere toegang af.
pub struct Database<'e, 'a, B: Storage> {
    connection: Connection<'e, 'a, B>,
    uncertain: Option<i32>,
}
impl<'e, 'a, B: Storage> Database<'e, 'a, B> {
    /// Opent hetzelfde schema en ruimt onvoltooide uploads van de vorige levensduur op.
    pub fn open(mut connection: Connection<'e, 'a, B>) -> Result<Self> {
        // 05-09 Go-meting: exclusive caching 8423 -> 68222 lookups/s; WAL maakte
        // bulk 429 -> 190 MB/s met 200 ms stalls. De rollback-journal blijft.
        // 64 KiB-pagina's maken van 257 reads per MiB er 17. Alleen nieuwe DBs
        // nemen de page_size over. Grote payloads blijven buiten b-tree keys.
        // The cache shares a fixed arena with bindings and record construction.
        // Leave room for large existing state rows, including legacy b-tree keys.
        connection.execute(c"PRAGMA locking_mode=EXCLUSIVE; PRAGMA cache_size=-16384; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA page_size=65536; PRAGMA journal_mode=DELETE;
CREATE TABLE IF NOT EXISTS spin_kv(key TEXT PRIMARY KEY,value BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS spin_rows(collection TEXT NOT NULL,id TEXT NOT NULL,value BLOB NOT NULL,UNIQUE(collection,id));
CREATE TABLE IF NOT EXISTS spin_objects(id INTEGER PRIMARY KEY,digest TEXT,kind TEXT NOT NULL,size INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0);
CREATE UNIQUE INDEX IF NOT EXISTS spin_objects_digest ON spin_objects(digest) WHERE complete=1;
CREATE TABLE IF NOT EXISTS spin_object_chunks(id INTEGER PRIMARY KEY,object_id INTEGER NOT NULL REFERENCES spin_objects(id) ON DELETE CASCADE,sequence INTEGER NOT NULL,data BLOB NOT NULL,UNIQUE(object_id,sequence));
CREATE TABLE IF NOT EXISTS spin_object_refs(ref TEXT PRIMARY KEY,object_id INTEGER NOT NULL REFERENCES spin_objects(id),FOREIGN KEY(object_id) REFERENCES spin_objects(id)) WITHOUT ROWID;
DELETE FROM spin_objects WHERE complete=0;")?;
        Ok(Self {
            connection,
            uncertain: None,
        })
    }
    /// Heropent binnen dezelfde app-levensduur zonder actieve uploads te verwijderen.
    /// Alleen gebruiken nadat `open` het schema en bootherstel heeft afgerond.
    pub fn attach(mut connection: Connection<'e, 'a, B>) -> Result<Self> {
        connection.execute(c"PRAGMA locking_mode=EXCLUSIVE; PRAGMA cache_size=-16384; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA journal_mode=DELETE;")?;
        Ok(Self {
            connection,
            uncertain: None,
        })
    }
    fn ready(&self) -> Result {
        match self.uncertain {
            Some(code) => Err(Error::Uncertain(code)),
            None => Ok(()),
        }
    }
    fn execute(&mut self, sql: &CStr, values: &[Value<'_>]) -> Result {
        self.ready()?;
        let mut statement = self.connection.prepare(sql)?;
        for (index, value) in values.iter().enumerate() {
            let bound = match value {
                Value::Null => Value::Null,
                Value::Integer(n) => Value::Integer(*n),
                Value::Real(n) => Value::Real(*n),
                Value::Text(s) => Value::Text(s),
                Value::Blob(b) => Value::Blob(b),
            };
            statement.bind(
                u32::try_from(index + 1).map_err(|_| Error::Invalid("too many bindings"))?,
                bound,
            )?;
        }
        if statement.step()? {
            return Err(Error::Invalid("unexpected SQL row"));
        }
        Ok(())
    }
    fn transaction<T>(&mut self, operation: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.ready()?;
        self.connection.execute(c"BEGIN IMMEDIATE")?;
        match operation(self) {
            Ok(value) => match self.connection.execute(c"COMMIT") {
                Ok(()) => Ok(value),
                Err(e) => {
                    self.uncertain = Some(e.code);
                    let _ = self.connection.execute(c"ROLLBACK");
                    Err(Error::Uncertain(e.code))
                }
            },
            Err(error) => {
                if let Err(e) = self.connection.execute(c"ROLLBACK") {
                    // SQLite may already have rolled back after FULL or NOMEM.
                    // Preserve that cause instead of masking it with "no transaction".
                    let code = match error {
                        Error::Sql(cause) => cause.code,
                        _ => e.code,
                    };
                    self.uncertain = Some(code);
                    return Err(Error::Uncertain(code));
                }
                Err(error)
            }
        }
    }
    /// Leest een logische sleutel onder een expliciet bytebudget.
    pub fn read_file(&mut self, key: &str, limit: usize) -> Result<Vec<u8>> {
        self.ready()?;
        let mut s = self
            .connection
            .prepare(c"SELECT value FROM spin_kv WHERE key=?")?;
        s.bind(1, Value::Text(key))?;
        if !s.step()? {
            return Err(Error::NotFound);
        }
        match s.column(0)? {
            Value::Blob(b) if b.len() <= limit => copy(b),
            _ => Err(Error::Invalid(
                "state value exceeds budget or has wrong type",
            )),
        }
    }
    /// Publiceert een sleutel in een bevestigde SQLite-transactie.
    pub fn write_file(&mut self, key: &str, bytes: &[u8]) -> Result {
        self.transaction(|db|db.execute(c"INSERT INTO spin_kv(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",&[Value::Text(key),Value::Blob(bytes)]))
    }
    /// Verwijderen van een ontbrekende sleutel is idempotent.
    pub fn delete_file(&mut self, key: &str) -> Result {
        self.transaction(|db| db.execute(c"DELETE FROM spin_kv WHERE key=?", &[Value::Text(key)]))
    }
    /// Controleert de bestaande database zonder schemawijzigingen.
    pub fn quick_check(&mut self) -> Result {
        self.ready()?;
        let mut s = self.connection.prepare(c"PRAGMA quick_check")?;
        if !s.step()? || string(&mut s, 0)? != "ok" {
            return Err(Error::Invalid("SQLite quick_check failed"));
        }
        Ok(())
    }
    /// Streamt een blob, dedupliceert op inhoud en publiceert de referentie als laatste.
    /// De lezer geeft nul uitsluitend bij EOF en nooit meer dan de aangeboden ruimte.
    pub fn put_blob(
        &mut self,
        reference: &str,
        kind: &str,
        mut read: impl FnMut(&mut [u8]) -> Result<usize>,
    ) -> Result<BlobInfo> {
        let reference = reference.trim();
        let kind = kind.trim();
        if reference.is_empty() {
            return Err(Error::Invalid("blob reference is required"));
        }
        self.transaction(|db| {
            db.execute(c"INSERT INTO spin_objects(kind) VALUES(?)",&[Value::Text(kind)])?;
            let object={let mut s=db.connection.prepare(c"SELECT last_insert_rowid()")?;if !s.step()?{return Err(Error::Invalid("missing rowid"));}integer(&mut s,0)?};
            let mut hash=Sha256::new();let mut size=0_i64;let mut sequence=0_i64;
            let mut buffer=Vec::new();buffer.try_reserve_exact(BLOB_CHUNK_SIZE).map_err(|_|d::Error::OutOfMemory)?;buffer.resize(BLOB_CHUNK_SIZE,0);
            loop {
                let mut count=0;
                while count<BLOB_CHUNK_SIZE {
                    let available=buffer.get_mut(count..).ok_or(Error::Invalid("chunk offset"))?;
                    let n=read(available)?;
                    if n>available.len(){return Err(Error::Invalid("reader overflow"));}
                    if n==0{break;}
                    count+=n;
                }
                if count>0 {
                    let chunk=buffer.get(..count).ok_or(Error::Invalid("chunk size"))?;
                    hash.update(chunk);size=size.checked_add(i64::try_from(count).map_err(|_|Error::Invalid("blob too large"))?).ok_or(Error::Invalid("blob too large"))?;
                    db.execute(c"INSERT INTO spin_object_chunks(object_id,sequence,data) VALUES(?,?,?)",&[Value::Integer(object),Value::Integer(sequence),Value::Blob(chunk)])?;
                    sequence=sequence.checked_add(1).ok_or(Error::Invalid("too many chunks"))?;
                }
                if count<BLOB_CHUNK_SIZE{break;}
            }
            let digest=digest(hash)?;
            let existing={let mut s=db.connection.prepare(c"SELECT id FROM spin_objects WHERE digest=? AND complete=1")?;s.bind(1,Value::Text(&digest))?;if s.step()?{Some(integer(&mut s,0)?)}else{None}};
            let id=if let Some(existing)=existing {db.execute(c"DELETE FROM spin_objects WHERE id=?",&[Value::Integer(object)])?;existing}
                else {db.execute(c"UPDATE spin_objects SET digest=?,size=?,complete=1 WHERE id=?",&[Value::Text(&digest),Value::Integer(size),Value::Integer(object)])?;object};
            db.execute(c"INSERT INTO spin_object_refs(ref,object_id) VALUES(?,?) ON CONFLICT(ref) DO UPDATE SET object_id=excluded.object_id",&[Value::Text(reference),Value::Integer(id)])?;
            Ok(BlobInfo{reference:try_string(reference)?,digest,kind:try_string(kind)?,size})
        })
    }
    /// Metadata is uitsluitend zichtbaar voor complete objecten.
    pub fn blob_info(&mut self, reference: &str) -> Result<BlobInfo> {
        self.ready()?;
        let mut s=self.connection.prepare(c"SELECT o.digest,o.kind,o.size FROM spin_object_refs r JOIN spin_objects o ON o.id=r.object_id WHERE r.ref=? AND o.complete=1")?;
        s.bind(1, Value::Text(reference))?;
        if !s.step()? {
            return Err(Error::NotFound);
        }
        Ok(BlobInfo {
            reference: try_string(reference)?,
            digest: string(&mut s, 0)?,
            kind: string(&mut s, 1)?,
            size: integer(&mut s, 2)?,
        })
    }
    /// Leest één uitgelijnde chunk, voor hervatbare downloads zonder hele imagekopie.
    pub fn read_blob_chunk(&mut self, reference: &str, offset: i64) -> Result<(Vec<u8>, BlobInfo)> {
        let info = self.blob_info(reference)?;
        if offset < 0 {
            return Err(Error::Invalid("negative chunk offset"));
        }
        if offset >= info.size {
            return Ok((Vec::new(), info));
        }
        let unit = i64::try_from(BLOB_CHUNK_SIZE).map_err(|_| Error::Invalid("chunk size"))?;
        if offset % unit != 0 {
            return Err(Error::Invalid("unaligned chunk offset"));
        }
        let mut s=self.connection.prepare(c"SELECT c.data FROM spin_object_refs r JOIN spin_object_chunks c ON c.object_id=r.object_id WHERE r.ref=? AND c.sequence=?")?;
        s.bind(1, Value::Text(reference))?;
        s.bind(2, Value::Integer(offset / unit))?;
        if !s.step()? {
            return Err(Error::Invalid("missing blob chunk"));
        }
        let chunk = match s.column(0)? {
            Value::Blob(b) if b.len() <= BLOB_CHUNK_SIZE => copy(b)?,
            _ => return Err(Error::Invalid("invalid blob chunk")),
        };
        if i64::try_from(chunk.len()).map_err(|_| Error::Invalid("chunk size"))?
            != (info.size - offset).min(unit)
        {
            return Err(Error::Invalid("truncated blob chunk"));
        }
        Ok((chunk, info))
    }
    /// Streamt alle chunks en controleert aan het einde grootte en SHA-256.
    pub fn write_blob_to(
        &mut self,
        reference: &str,
        mut write: impl FnMut(&[u8]) -> Result,
    ) -> Result<BlobInfo> {
        let info = self.blob_info(reference)?;
        let mut s=self.connection.prepare(c"SELECT c.sequence,c.data FROM spin_object_refs r JOIN spin_object_chunks c ON c.object_id=r.object_id WHERE r.ref=? ORDER BY c.sequence")?;
        s.bind(1, Value::Text(reference))?;
        let mut sequence = 0;
        let mut size = 0_i64;
        let mut hash = Sha256::new();
        while s.step()? {
            if integer(&mut s, 0)? != sequence {
                return Err(Error::Invalid("missing blob chunk"));
            }
            let chunk = match s.column(1)? {
                Value::Blob(b) => b,
                _ => return Err(Error::Invalid("invalid chunk")),
            };
            write(chunk)?;
            hash.update(chunk);
            size = size
                .checked_add(i64::try_from(chunk.len()).map_err(|_| Error::Invalid("chunk size"))?)
                .ok_or(Error::Invalid("blob too large"))?;
            sequence += 1;
        }
        if size != info.size || digest(hash)? != info.digest {
            return Err(Error::Invalid("blob content hash or size mismatch"));
        }
        Ok(info)
    }
    /// Kleine objecten mogen onder een expliciete bovengrens in één buffer.
    pub fn read_blob(&mut self, reference: &str, limit: usize) -> Result<(Vec<u8>, BlobInfo)> {
        let info = self.blob_info(reference)?;
        let size = usize::try_from(info.size).map_err(|_| Error::Invalid("blob size"))?;
        if size > limit {
            return Err(Error::Invalid("blob exceeds limit"));
        }
        let mut out = Vec::new();
        out.try_reserve_exact(size)
            .map_err(|_| d::Error::OutOfMemory)?;
        let info = self.write_blob_to(reference, |chunk| {
            if chunk.len() > size - out.len() {
                return Err(Error::Invalid("blob exceeds declared size"));
            }
            out.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok((out, info))
    }
    /// Verwijdert bytes pas wanneer de laatste referentie is verdwenen.
    pub fn delete_blob(&mut self, reference: &str) -> Result {
        self.transaction(|db| {
            let id={let mut s=db.connection.prepare(c"SELECT object_id FROM spin_object_refs WHERE ref=?")?;s.bind(1,Value::Text(reference))?;if !s.step()?{return Err(Error::NotFound);}integer(&mut s,0)?};
            db.execute(c"DELETE FROM spin_object_refs WHERE ref=?",&[Value::Text(reference)])?;
            db.execute(c"DELETE FROM spin_objects WHERE id=? AND NOT EXISTS(SELECT 1 FROM spin_object_refs WHERE object_id=?)",&[Value::Integer(id),Value::Integer(id)])
        })
    }
    /// De database meet zichzelf, onafhankelijk van de VFS.
    pub fn usage(&mut self) -> Result<Usage> {
        self.ready()?;
        let scalar = |conn: &mut Connection<'_, '_, B>, sql: &CStr| -> Result<i64> {
            let mut s = conn.prepare(sql)?;
            if !s.step()? {
                return Err(Error::Invalid("missing statistic"));
            }
            integer(&mut s, 0)
        };
        let pages = scalar(&mut self.connection, c"PRAGMA page_count")?;
        let page_size = scalar(&mut self.connection, c"PRAGMA page_size")?;
        let mut s = self
            .connection
            .prepare(c"SELECT COUNT(*),COALESCE(SUM(size),0) FROM spin_objects WHERE complete=1")?;
        if !s.step()? {
            return Err(Error::Invalid("missing object statistic"));
        }
        Ok(Usage {
            database_bytes: pages
                .checked_mul(page_size)
                .ok_or(Error::Invalid("database size"))?,
            objects: integer(&mut s, 0)?,
            object_bytes: integer(&mut s, 1)?,
        })
    }
}

/// Een opslagfout zoals de Store hem kent.
pub fn persistence_to_store(error: Error) -> spin_store::Error {
    match error {
        Error::Sql(e) => spin_store::Error::Storage(e.code),
        Error::Uncertain(code) => spin_store::Error::StorageUncertain(code),
        Error::Data(e) => spin_store::Error::Data(e),
        Error::Security(e) => spin_store::Error::Security(e),
        Error::NotFound => spin_store::Error::NotFound,
        Error::Invalid(reason) => spin_store::Error::Conflict(reason),
    }
}
fn store_to_persistence(error: spin_store::Error) -> Error {
    match error {
        spin_store::Error::Data(e) => Error::Data(e),
        spin_store::Error::Security(e) => Error::Security(e),
        _ => Error::Invalid("state comparison failed"),
    }
}
/// De Store-adapter bezit de database, masterkey en entropiebron samen.
pub struct Encrypted<'e, 'a, B: Storage, E: Entropy> {
    database: Database<'e, 'a, B>,
    cipher: Cipher,
    entropy: E,
}
impl<'e, 'a, B: Storage, E: Entropy> Encrypted<'e, 'a, B, E> {
    /// Neemt de reeds geopende verbinding en sleutel over.
    pub fn new(database: Database<'e, 'a, B>, cipher: Cipher, entropy: E) -> Self {
        Self {
            database,
            cipher,
            entropy,
        }
    }
    /// Leest de bestaande state; een verkeerde sleutel mag geen lege Store opleveren.
    pub fn load(
        &mut self,
        login_id: impl FnMut() -> spin_security::Result<String>,
    ) -> Result<PersistedState> {
        let Some((bytes, legacy)) = self.database.read_state(MAX_STATE_BYTES)? else {
            return Ok(PersistedState::default());
        };
        let sealed = PersistedState::from_json_with_limit(&bytes, MAX_STATE_BYTES)?;
        let loaded = self.cipher.decrypt_state(&sealed, login_id)?;
        let mut state = loaded.try_clone()?;
        state.normalize_loaded()?;
        // De oude enkele rij wordt eenmalig rijen; daarna alleen wat de
        // normalisatie veranderde.
        let changes = if legacy {
            None
        } else {
            Some(spin_store::diff(&loaded, &state).map_err(store_to_persistence)?)
        };
        let rows = state_rows(
            &self.cipher,
            &mut self.entropy,
            &state,
            changes.as_ref().map(|c| c.as_slice()),
        )?;
        if legacy {
            self.database.replace_rows(&rows)?;
        } else {
            self.database.write_rows(&rows)?;
        }
        Ok(state)
    }
    /// Dezelfde eigenaar voert blobopdrachten tussen Store-opdrachten uit.
    pub fn database(&mut self) -> &mut Database<'e, 'a, B> {
        &mut self.database
    }
}
impl<B: Storage, E: Entropy> spin_store::Persistence for Encrypted<'_, '_, B, E> {
    fn storage_usage(&mut self) -> spin_store::Result<Option<spin_store::StorageUsage>> {
        let usage = self.database.usage().map_err(uploads::store_error)?;
        Ok(Some(spin_store::StorageUsage {
            database_bytes: usage.database_bytes,
            object_bytes: usage.object_bytes,
            objects: usage.objects,
            replication: d::json::Value::Null,
        }))
    }
    fn blob(
        &mut self,
        request: spin_store::BlobRequest<'_>,
    ) -> spin_store::Result<spin_store::BlobReply> {
        self.database.blob(request).map_err(uploads::store_error)
    }
    fn save(&mut self, state: &PersistedState) -> spin_store::Result {
        let rows = state_rows(&self.cipher, &mut self.entropy, state, None)
            .map_err(persistence_to_store)?;
        self.database
            .replace_rows(&rows)
            .map_err(persistence_to_store)
    }
    fn save_changes(
        &mut self,
        state: &PersistedState,
        changes: &[spin_store::Change],
    ) -> spin_store::Result {
        let rows = state_rows(&self.cipher, &mut self.entropy, state, Some(changes))
            .map_err(persistence_to_store)?;
        self.database
            .write_rows(&rows)
            .map_err(persistence_to_store)
    }
}

impl<B: Storage, E: Entropy> spin_store::backup::PortablePersistence for Encrypted<'_, '_, B, E> {
    fn export(
        &mut self,
        state: &PersistedState,
    ) -> spin_store::Result<spin_store::backup::PortableState> {
        spin_store::backup::encrypt(state, &self.cipher, &mut self.entropy)
    }
}
