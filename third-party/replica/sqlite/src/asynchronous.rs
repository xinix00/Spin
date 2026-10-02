//! Synchrone SQLite-callbacks op een door de runtime parkeerbare eigenaar-stack.
use crate::{Error, FileId, OpenFlags, Result, Storage};
use core::{ffi::CStr, future::Future};
/// Eén async opslag-eigenaar; dezelfde duurzaamheidsregels als Storage.
pub trait AsyncStorage {
    /// Geef de executor ruimte tijdens CPU-werk in SQLite.
    fn cooperate(&mut self) -> impl Future<Output = Result>;
    /// Maak deze eigenaar blijvend onbruikbaar na een onderbroken callback.
    fn invalidate(&mut self);
    /// Open binnen het exclusieve opslagdomein.
    fn open(&mut self, name: &CStr, flags: OpenFlags) -> impl Future<Output = Result<FileId>>;
    /// Sluit het handvat.
    fn close(&mut self, file: FileId) -> impl Future<Output = Result>;
    /// Lees; korte read betekent EOF.
    fn read(
        &mut self,
        file: FileId,
        offset: u64,
        dst: &mut [u8],
    ) -> impl Future<Output = Result<usize>>;
    /// Schrijf de hele buffer of faal.
    fn write(&mut self, file: FileId, offset: u64, src: &[u8]) -> impl Future<Output = Result>;
    /// Wijzig de logische maat.
    fn truncate(&mut self, file: FileId, size: u64) -> impl Future<Output = Result>;
    /// Wacht op bevestigde duurzame data en metadata.
    fn sync(&mut self, file: FileId, flags: i32) -> impl Future<Output = Result>;
    /// Actuele bestandsgrootte.
    fn size(&mut self, file: FileId) -> impl Future<Output = Result<u64>>;
    /// Bij sync_directory moet de naamverwijdering duurzaam bevestigd zijn.
    fn remove(&mut self, name: &CStr, sync_directory: bool) -> impl Future<Output = Result>;
    /// Bestaat de naam binnen het domein?
    fn exists(&mut self, name: &CStr) -> impl Future<Output = Result<bool>>;
    /// Vult de hele buffer; geen ongevulde entropie retourneren.
    fn random(&mut self, dst: &mut [u8]) -> Result;
    /// Unixmilliseconden van de platformklok.
    fn unix_millis(&mut self) -> Result<i64>;
}
/// Annulering van de private C-stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;
/// De runtime parkeert uitsluitend deze C-stack en laat de gewone executor lopen.
/// Geen geneste executor, spinloop of heruitvoering van SQL na Pending.
pub trait Suspend {
    /// Behoud de future en al zijn leningen tot voltooiing of annulering.
    fn wait<F: Future>(&self, future: F) -> core::result::Result<F::Output, Cancelled>;
}
/// VFS-adapter die uitsluitend binnen zijn runtime-stack gebruikt wordt.
pub struct Bridge<'a, I, W> {
    io: &'a mut I,
    waiter: &'a W,
    poisoned: bool,
}
impl<'a, I: AsyncStorage, W: Suspend> Bridge<'a, I, W> {
    /// Leent de I/O-eigenaar zolang SQLite callbacks kan doen.
    pub fn new(io: &'a mut I, waiter: &'a W) -> Self {
        Self {
            io,
            waiter,
            poisoned: false,
        }
    }
    fn ready(&self) -> Result {
        if self.poisoned {
            Err(Error::IO)
        } else {
            Ok(())
        }
    }
    fn finish<T>(&mut self, result: core::result::Result<Result<T>, Cancelled>) -> Result<T> {
        match result {
            Ok(v) => v,
            Err(Cancelled) => {
                self.poisoned = true;
                self.io.invalidate();
                Err(Error::IO)
            }
        }
    }
}
impl<I: AsyncStorage, W: Suspend> Storage for Bridge<'_, I, W> {
    fn cooperate(&mut self) -> Result {
        self.ready()?;
        let r = self.waiter.wait(self.io.cooperate());
        self.finish(r)
    }
    fn open(&mut self, name: &CStr, flags: OpenFlags) -> Result<FileId> {
        self.ready()?;
        let r = self.waiter.wait(self.io.open(name, flags));
        self.finish(r)
    }
    fn close(&mut self, file: FileId) -> Result {
        self.ready()?;
        let r = self.waiter.wait(self.io.close(file));
        self.finish(r)
    }
    fn read(&mut self, file: FileId, offset: u64, dst: &mut [u8]) -> Result<usize> {
        self.ready()?;
        let r = self.waiter.wait(self.io.read(file, offset, dst));
        self.finish(r)
    }
    fn write(&mut self, file: FileId, offset: u64, src: &[u8]) -> Result {
        self.ready()?;
        let r = self.waiter.wait(self.io.write(file, offset, src));
        self.finish(r)
    }
    fn truncate(&mut self, file: FileId, size: u64) -> Result {
        self.ready()?;
        let r = self.waiter.wait(self.io.truncate(file, size));
        self.finish(r)
    }
    fn sync(&mut self, file: FileId, flags: i32) -> Result {
        self.ready()?;
        let r = self.waiter.wait(self.io.sync(file, flags));
        self.finish(r)
    }
    fn size(&mut self, file: FileId) -> Result<u64> {
        self.ready()?;
        let r = self.waiter.wait(self.io.size(file));
        self.finish(r)
    }
    fn remove(&mut self, name: &CStr, sync_directory: bool) -> Result {
        self.ready()?;
        let r = self.waiter.wait(self.io.remove(name, sync_directory));
        self.finish(r)
    }
    fn exists(&mut self, name: &CStr) -> Result<bool> {
        self.ready()?;
        let r = self.waiter.wait(self.io.exists(name));
        self.finish(r)
    }
    fn random(&mut self, dst: &mut [u8]) -> Result {
        self.ready()?;
        self.io.random(dst)
    }
    fn unix_millis(&mut self) -> Result<i64> {
        self.ready()?;
        self.io.unix_millis()
    }
}
