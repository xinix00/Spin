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
pub(in crate::boot) struct Backend<'a> {
    files: &'a FilesPool,
    pub(in crate::boot) wait: &'a Wait<'a>,
    database: String,
    written: u64,
}
impl<'a> Backend<'a> {
    pub(in crate::boot) fn new(files: &'a FilesPool, wait: &'a Wait<'a>, database: String) -> Self {
        Self {
            files,
            wait,
            database,
            written: 0,
        }
    }
    /// De databasenaam waaronder ook de Replica-spool en -markers staan.
    pub(in crate::boot) fn database(&self) -> &str {
        &self.database
    }
    fn name(&self, name: &CStr) -> replica_sqlite::Result<Name> {
        let name = name.to_str().map_err(|_| replica_sqlite::Error::TEXT)?;
        let suffix = if let Some(suffix) = name.strip_prefix("spin.sqlite") {
            spin_core::validation::text(format_args!("{}{suffix}", self.database))
        } else if let Some(suffix) = name.strip_prefix("spin-restore") {
            spin_core::validation::text(format_args!("{}.restore{suffix}", self.database))
        } else {
            return Err(replica_sqlite::Error::CANNOT_OPEN);
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
        let name = self.name(name)?;
        self.with(|b| {
            b.open(
                name.cstr()
                    .map_err(|_| replica_sqlite::Error::CANNOT_OPEN)?,
                flags,
            )
        })
    }
    fn close(&mut self, file: FileId) -> replica_sqlite::Result {
        self.with(|b| b.close(file))
    }
    fn read(&mut self, file: FileId, offset: u64, dst: &mut [u8]) -> replica_sqlite::Result<usize> {
        self.with(|b| b.read(file, offset, dst))
    }
    fn write(&mut self, file: FileId, offset: u64, src: &[u8]) -> replica_sqlite::Result {
        self.with(|b| b.write(file, offset, src))?;
        let previous = self.written / (64 << 20);
        self.written = self.written.saturating_add(src.len() as u64);
        if self.written / (64 << 20) != previous {
            applib::log!(
                "SPIN_STORAGE_WRITE_PROGRESS database={} written_bytes={}",
                self.database,
                self.written
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
