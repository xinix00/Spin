//! Tenant names are confined to a single leased volume; SQLite's arena has one owner.
use super::*;
use core::{
    cell::RefCell,
    ffi::CStr,
    ops::{Deref, DerefMut},
    task::{Poll, Waker},
};
use replica_sqlite::{FileId, OpenFlags, asynchronous::Suspend};

pub(in crate::boot) struct Arena {
    state: RefCell<ArenaState>,
}
struct ArenaState {
    bytes: Option<Vec<u64>>,
    waiter: Option<Waker>,
}
pub(in crate::boot) struct Loan<'a> {
    arena: &'a Arena,
    bytes: Vec<u64>,
}
impl Arena {
    pub(in crate::boot) fn new() -> spin_server::Result<Self> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(spin_persistence::SQLITE_HEAP_BYTES / 8)
            .map_err(crate::platform::failure)?;
        bytes.resize(spin_persistence::SQLITE_HEAP_BYTES / 8, 0);
        Ok(Self {
            state: RefCell::new(ArenaState {
                bytes: Some(bytes),
                waiter: None,
            }),
        })
    }
    pub(in crate::boot) fn take(&self, wait: &Wait<'_>) -> replica_sqlite::Result<Loan<'_>> {
        wait.wait(core::future::poll_fn(|cx| {
            // Transfer ownership; no RefCell guard crosses a wait or SQLite callback.
            let mut state = self.state.borrow_mut();
            if let Some(bytes) = state.bytes.take() {
                Poll::Ready(Loan { arena: self, bytes })
            } else {
                state.waiter = Some(cx.waker().clone());
                Poll::Pending
            }
        }))
        .map_err(|_| replica_sqlite::Error::IO)
    }
}
impl Deref for Loan<'_> {
    type Target = [u64];
    fn deref(&self) -> &[u64] {
        &self.bytes
    }
}
impl DerefMut for Loan<'_> {
    fn deref_mut(&mut self) -> &mut [u64] {
        &mut self.bytes
    }
}
impl Drop for Loan<'_> {
    fn drop(&mut self) {
        let waiter = {
            let mut state = self.arena.state.borrow_mut();
            state.bytes = Some(core::mem::take(&mut self.bytes));
            state.waiter.take()
        };
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }
}
/// One SystemClient for all tenant I/O; the second allowed connection belongs to the lease.
pub(in crate::boot) struct FilesPool {
    files: RefCell<Option<Files<'static, Environment>>>,
    waiter: RefCell<Option<Waker>>,
}
struct FileLoan<'a> {
    pool: &'a FilesPool,
    files: Option<Files<'static, Environment>>,
}
impl FilesPool {
    pub(in crate::boot) fn exists(&self, wait: &Wait<'_>, name: &str) -> spin_server::Result<bool> {
        let name = replica_core::local::Name::new(name)
            .map_err(|_| spin_server::Error::Http(500, "invalid database name"))?;
        let mut loan = self.take(wait).map_err(crate::platform::failure)?;
        let mut bridge = Bridge::new(
            loan.files
                .as_mut()
                .ok_or(spin_server::Error::Http(503, "files unavailable"))?,
            wait,
        );
        bridge
            .exists(
                name.cstr()
                    .map_err(|_| spin_server::Error::Http(500, "invalid database name"))?,
            )
            .map_err(crate::platform::failure)
    }
    /// Verwijdert een bestand in de root als het er is.
    pub(in crate::boot) fn remove(&self, wait: &Wait<'_>, name: &str) -> spin_server::Result {
        if !self.exists(wait, name)? {
            return Ok(());
        }
        let name = replica_core::local::Name::new(name)
            .map_err(|_| spin_server::Error::Http(500, "invalid file name"))?;
        let mut loan = self.take(wait).map_err(crate::platform::failure)?;
        let mut bridge = Bridge::new(
            loan.files
                .as_mut()
                .ok_or(spin_server::Error::Http(503, "files unavailable"))?,
            wait,
        );
        bridge
            .remove(
                name.cstr()
                    .map_err(|_| spin_server::Error::Http(500, "invalid file name"))?,
                false,
            )
            .map_err(crate::platform::failure)
    }
    pub(in crate::boot) fn new(files: Files<'static, Environment>) -> Self {
        Self {
            files: RefCell::new(Some(files)),
            waiter: RefCell::new(None),
        }
    }
    fn take(&self, wait: &Wait<'_>) -> replica_sqlite::Result<FileLoan<'_>> {
        wait.wait(core::future::poll_fn(|cx| {
            let files = self.files.borrow_mut().take();
            if let Some(files) = files {
                Poll::Ready(FileLoan {
                    pool: self,
                    files: Some(files),
                })
            } else {
                *self.waiter.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        }))
        .map_err(|_| replica_sqlite::Error::IO)
    }
    pub(in crate::boot) fn marker(&self, wait: &Wait<'_>, domain: &str) -> spin_server::Result {
        let name = spin_core::validation::text(format_args!(
            "spin-{}.domain",
            spin_security::digest_hex(domain.as_bytes())?
        ))?;
        let name =
            Name::new(&name).map_err(|_| spin_server::Error::Http(503, "invalid domain marker"))?;
        let mut loan = self.take(wait).map_err(crate::platform::failure)?;
        let mut bridge = Bridge::new(
            loan.files
                .as_mut()
                .ok_or(replica_sqlite::Error::IO)
                .map_err(crate::platform::failure)?,
            wait,
        );
        let file = bridge
            .open(
                name.cstr()
                    .map_err(|_| spin_server::Error::Http(503, "invalid domain marker"))?,
                OpenFlags(6),
            )
            .map_err(crate::platform::failure)?;
        let result = (|| -> replica_sqlite::Result {
            // The checksum is the filename. A torn new marker cannot select a different tenant.
            bridge.write(file, 0, domain.as_bytes())?;
            bridge.truncate(file, domain.len() as u64)?;
            bridge.sync(file, 2)
        })();
        let closed = bridge.close(file);
        result.and(closed).map_err(crate::platform::failure)
    }
}
impl Drop for FileLoan<'_> {
    fn drop(&mut self) {
        *self.pool.files.borrow_mut() = self.files.take();
        let waiter = self.pool.waiter.borrow_mut().take();
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }
}
/// Waar de bestanden van een tenant staan.
#[derive(Clone)]
pub(in crate::boot) enum Location {
    /// Het oude model: los in de root, `spin-<hash>.sqlite` met de sidecars
    /// ernaast. Alleen nog voor een database zonder replicatie (of van Go).
    Root(String),
    /// Een eigen map per tenant: SQLite en Replica kiezen er zelf hun namen
    /// in (ook tijdelijke bestanden); de map is de afscherming.
    Dir(String),
}
pub(in crate::boot) struct Backend<'a> {
    files: &'a FilesPool,
    pub(in crate::boot) wait: &'a Wait<'a>,
    location: Location,
    written: u64,
    /// Welk soort bestand achter een open FileId zit, voor de schrijfteller.
    kinds: [(u32, usize); 16],
    /// Geschreven bytes per soort: database, journal, capture, dirty-logs, overig.
    per_kind: [u64; 5],
}
/// De soort van een SQLite- of Replica-bestand, op het achtervoegsel van zijn naam.
fn kind(name: &str) -> usize {
    let suffix = name.strip_prefix("spin.sqlite").unwrap_or(name);
    if suffix.is_empty() {
        0
    } else if suffix.ends_with("-journal") {
        1
    } else if suffix.contains("replica-capture") {
        2
    } else if suffix.contains("replica-dirty") {
        3
    } else {
        4
    }
}
impl<'a> Backend<'a> {
    pub(in crate::boot) fn new(
        files: &'a FilesPool,
        wait: &'a Wait<'a>,
        location: Location,
    ) -> Self {
        Self {
            files,
            wait,
            location,
            written: 0,
            kinds: [(u32::MAX, 4); 16],
            per_kind: [0; 5],
        }
    }
    /// Waar deze tenant zijn database, Replica-spool en markers heeft.
    pub(in crate::boot) fn location(&self) -> &Location {
        &self.location
    }
    fn name(&self, name: &CStr) -> replica_sqlite::Result<Name> {
        let name = name.to_str().map_err(|_| replica_sqlite::Error::TEXT)?;
        let suffix = match &self.location {
            // Eén naam, nooit een pad: alles blijft in de map van de tenant.
            Location::Dir(dir) => {
                if name.is_empty() || name.contains('/') || name.starts_with('.') {
                    return Err(replica_sqlite::Error::CANNOT_OPEN);
                }
                spin_core::validation::text(format_args!("{dir}/{name}"))
            }
            Location::Root(database) => {
                if let Some(suffix) = name.strip_prefix("spin.sqlite") {
                    spin_core::validation::text(format_args!("{database}{suffix}"))
                } else if let Some(suffix) = name.strip_prefix("spin-restore") {
                    spin_core::validation::text(format_args!("{database}.restore{suffix}"))
                } else if name.strip_prefix("etilqs_").is_some_and(|hex| {
                    hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit())
                }) {
                    // SQLite's tijdelijke bestanden, naast de database.
                    spin_core::validation::text(format_args!("{database}.{name}"))
                } else {
                    return Err(replica_sqlite::Error::CANNOT_OPEN);
                }
            }
        };
        Name::new(&suffix.map_err(|_| replica_sqlite::Error::MEMORY)?)
            .map_err(|_| replica_sqlite::Error::CANNOT_OPEN)
    }
    fn with<T>(
        &mut self,
        operation: impl FnOnce(
            &mut Bridge<'_, Files<'static, Environment>, Wait<'a>>,
        ) -> replica_sqlite::Result<T>,
    ) -> replica_sqlite::Result<T> {
        let mut loan = self.files.take(self.wait)?;
        let files = loan.files.as_mut().ok_or(replica_sqlite::Error::IO)?;
        operation(&mut Bridge::new(files, self.wait))
    }
}
impl Storage for Backend<'_> {
    fn cooperate(&mut self) -> replica_sqlite::Result {
        self.with(|b| b.cooperate())
    }
    fn open(&mut self, name: &CStr, flags: OpenFlags) -> replica_sqlite::Result<FileId> {
        let sort = kind(name.to_str().unwrap_or(""));
        let name = self.name(name)?;
        let file = self.with(|b| {
            b.open(
                name.cstr()
                    .map_err(|_| replica_sqlite::Error::CANNOT_OPEN)?,
                flags,
            )
        })?;
        if let Some(slot) = self
            .kinds
            .iter_mut()
            .find(|(id, _)| *id == file.0 || *id == u32::MAX)
        {
            *slot = (file.0, sort);
        }
        Ok(file)
    }
    fn close(&mut self, file: FileId) -> replica_sqlite::Result {
        if let Some(slot) = self.kinds.iter_mut().find(|(id, _)| *id == file.0) {
            *slot = (u32::MAX, 4);
        }
        self.with(|b| b.close(file))
    }
    fn read(&mut self, file: FileId, offset: u64, dst: &mut [u8]) -> replica_sqlite::Result<usize> {
        self.with(|b| b.read(file, offset, dst))
    }
    fn write(&mut self, file: FileId, offset: u64, src: &[u8]) -> replica_sqlite::Result {
        self.with(|b| b.write(file, offset, src))?;
        let previous = self.written / (64 << 20);
        self.written = self.written.saturating_add(src.len() as u64);
        let sort = self
            .kinds
            .iter()
            .find(|(id, _)| *id == file.0)
            .map_or(4, |(_, sort)| *sort);
        self.per_kind[sort] = self.per_kind[sort].saturating_add(src.len() as u64);
        if self.written / (64 << 20) != previous {
            // Wie schrijft er: de database, het journal, de capture-spool van
            // Replica, zijn dirty-logs of de rest (markers), in MB.
            let mb = |n: u64| n >> 20;
            applib::log!(
                "SPIN_STORAGE_WRITE_PROGRESS written_mb={} db={} journal={} capture={} dirty={} other={}",
                mb(self.written),
                mb(self.per_kind[0]),
                mb(self.per_kind[1]),
                mb(self.per_kind[2]),
                mb(self.per_kind[3]),
                mb(self.per_kind[4])
            );
        }
        Ok(())
    }
    fn truncate(&mut self, file: FileId, size: u64) -> replica_sqlite::Result {
        self.with(|b| b.truncate(file, size))
    }
    fn sync(&mut self, file: FileId, flags: i32) -> replica_sqlite::Result {
        self.with(|b| b.sync(file, flags))
    }
    fn size(&mut self, file: FileId) -> replica_sqlite::Result<u64> {
        self.with(|b| b.size(file))
    }
    fn remove(&mut self, name: &CStr, sync_directory: bool) -> replica_sqlite::Result {
        let name = self.name(name)?;
        self.with(|b| {
            b.remove(
                name.cstr()
                    .map_err(|_| replica_sqlite::Error::CANNOT_OPEN)?,
                sync_directory,
            )
        })
    }
    fn exists(&mut self, name: &CStr) -> replica_sqlite::Result<bool> {
        let name = self.name(name)?;
        self.with(|b| {
            b.exists(
                name.cstr()
                    .map_err(|_| replica_sqlite::Error::CANNOT_OPEN)?,
            )
        })
    }
    fn random(&mut self, dst: &mut [u8]) -> replica_sqlite::Result {
        self.with(|b| b.random(dst))
    }
    fn unix_millis(&mut self) -> replica_sqlite::Result<i64> {
        self.with(|b| b.unix_millis())
    }
}
