//! Begrensde RAM-opslag voor tests en een eerste HopOS-bewoner; geen duurzaamheid.
use crate::{Error, FileId, OpenFlags, Result, Storage};
use core::ffi::CStr;
/// Maximale naam inclusief ruimte voor een afsluitende nul in SQLite.
pub const NAME_BYTES: usize = 256;
/// Eén bestandsbuffer; naam en lengte horen bij dezelfde eigenaar.
pub struct Slot<'a> {
    buf: &'a mut [u8],
    name: [u8; NAME_BYTES],
    name_len: usize,
    len: usize,
    open: bool,
}
impl<'a> Slot<'a> {
    /// Leent een vaste capaciteit zonder de hele buffer te alloceren of kopiëren.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            name: [0; NAME_BYTES],
            name_len: 0,
            len: 0,
            open: false,
        }
    }
    fn matches(&self, name: &CStr) -> bool {
        self.name_len != 0 && self.name.get(..self.name_len) == Some(name.to_bytes())
    }
}
/// Meetlat van de VFS, zodat een test echte reads/writes/syncs kan aantonen.
#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    /// Read-callbacks.
    pub reads: u64,
    /// Write-callbacks.
    pub writes: u64,
    /// Sync-callbacks; RAM wordt daarmee niet stroomvast.
    pub syncs: u64,
    /// Geslaagde naamverwijderingen.
    pub deletes: u64,
}
/// De eigenaar van een vast aantal RAM-bestanden.
///
/// De pseudowillekeurige generator is alleen voor tests/demo's; een productie-
/// backend levert zijn eigen entropie via `Storage::random`.
pub struct Memory<'a, const N: usize> {
    slots: [Slot<'a>; N],
    stats: Stats,
    millis: i64,
    random: u64,
}
impl<'a, const N: usize> Memory<'a, N> {
    /// Neemt de geleende bestandsbuffers over met een vaste testklok en seed.
    pub fn new(slots: [Slot<'a>; N], unix_millis: i64, seed: u64) -> Self {
        Self {
            slots,
            stats: Stats::default(),
            millis: unix_millis,
            random: if seed == 0 { 1 } else { seed },
        }
    }
    /// Meetlat zonder verborgen reset.
    pub fn stats(&self) -> Stats {
        self.stats
    }
    /// Leent de actuele bytes wanneer de SQLite-runtime de opslag heeft teruggegeven.
    pub fn contents(&self, name: &CStr) -> Result<&[u8]> {
        self.slots
            .iter()
            .find(|s| s.matches(name))
            .and_then(|s| s.buf.get(..s.len))
            .ok_or(Error::CANNOT_OPEN)
    }
    fn slot(&mut self, id: FileId) -> Result<&mut Slot<'a>> {
        self.slots
            .get_mut(id.0 as usize)
            .filter(|s| s.open)
            .ok_or(Error::IO)
    }
}
impl<const N: usize> Storage for Memory<'_, N> {
    fn open(&mut self, name: &CStr, flags: OpenFlags) -> Result<FileId> {
        let bytes = name.to_bytes();
        if bytes.is_empty() || bytes.len() >= NAME_BYTES {
            return Err(Error::CANNOT_OPEN);
        }
        let existing = self.slots.iter().position(|s| s.matches(name));
        let i = match existing {
            Some(i) => i,
            None if flags.is_create() => self
                .slots
                .iter()
                .position(|s| s.name_len == 0)
                .ok_or(Error::FULL)?,
            None => return Err(Error::CANNOT_OPEN),
        };
        let id = u32::try_from(i).map_err(|_| Error::FULL)?;
        let slot = self.slots.get_mut(i).ok_or(Error::IO)?;
        if slot.open {
            return Err(Error { code: 5 });
        }
        if existing.is_none() {
            slot.name
                .get_mut(..bytes.len())
                .ok_or(Error::CANNOT_OPEN)?
                .copy_from_slice(bytes);
            slot.name_len = bytes.len();
            slot.len = 0;
        }
        slot.open = true;
        Ok(FileId(id))
    }
    fn close(&mut self, id: FileId) -> Result {
        self.slot(id)?.open = false;
        Ok(())
    }
    fn read(&mut self, id: FileId, offset: u64, dst: &mut [u8]) -> Result<usize> {
        self.stats.reads = self.stats.reads.wrapping_add(1);
        let slot = self.slot(id)?;
        let off = usize::try_from(offset).map_err(|_| Error::IO)?;
        let n = slot.len.saturating_sub(off).min(dst.len());
        if n == 0 {
            return Ok(0);
        }
        dst.get_mut(..n)
            .ok_or(Error::IO)?
            .copy_from_slice(slot.buf.get(off..off + n).ok_or(Error::IO)?);
        Ok(n)
    }
    fn write(&mut self, id: FileId, offset: u64, src: &[u8]) -> Result {
        self.stats.writes = self.stats.writes.wrapping_add(1);
        let slot = self.slot(id)?;
        let off = usize::try_from(offset).map_err(|_| Error::FULL)?;
        let end = off
            .checked_add(src.len())
            .filter(|n| *n <= slot.buf.len())
            .ok_or(Error::FULL)?;
        if off > slot.len {
            slot.buf.get_mut(slot.len..off).ok_or(Error::FULL)?.fill(0)
        }
        slot.buf
            .get_mut(off..end)
            .ok_or(Error::FULL)?
            .copy_from_slice(src);
        slot.len = slot.len.max(end);
        Ok(())
    }
    fn truncate(&mut self, id: FileId, size: u64) -> Result {
        let slot = self.slot(id)?;
        let n = usize::try_from(size).map_err(|_| Error::FULL)?;
        if n > slot.buf.len() {
            return Err(Error::FULL);
        }
        if n > slot.len {
            slot.buf.get_mut(slot.len..n).ok_or(Error::FULL)?.fill(0)
        }
        slot.len = n;
        Ok(())
    }
    fn sync(&mut self, id: FileId, _flags: i32) -> Result {
        self.slot(id)?;
        self.stats.syncs = self.stats.syncs.wrapping_add(1);
        Ok(())
    }
    fn size(&mut self, id: FileId) -> Result<u64> {
        Ok(self.slot(id)?.len as u64)
    }
    fn remove(&mut self, name: &CStr, _sync_directory: bool) -> Result {
        if let Some(slot) = self.slots.iter_mut().find(|s| s.matches(name)) {
            if slot.open {
                return Err(Error { code: 5 });
            }
            slot.name_len = 0;
            slot.len = 0;
            self.stats.deletes = self.stats.deletes.wrapping_add(1);
        }
        Ok(())
    }
    fn exists(&mut self, name: &CStr) -> Result<bool> {
        Ok(self.slots.iter().any(|s| s.matches(name)))
    }
    fn random(&mut self, dst: &mut [u8]) -> Result {
        for byte in dst {
            self.random ^= self.random << 13;
            self.random ^= self.random >> 7;
            self.random ^= self.random << 17;
            *byte = self.random as u8;
        }
        Ok(())
    }
    fn unix_millis(&mut self) -> Result<i64> {
        Ok(self.millis)
    }
}
