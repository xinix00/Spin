//! Begrensde lokale bestandsbewerkingen via dezelfde VFS als SQLite.
use crate::{Error, Result, reserve};
use alloc::vec::Vec;
use core::ffi::CStr;
use replica_sqlite::{FileId, OpenFlags, Storage};
/// Canonieke VFS-naam met ruimte voor SQLite's afsluitende nul.
#[derive(Clone, Copy)]
pub struct Name {
    bytes: [u8; 256],
    len: usize,
}
impl Name {
    /// Nulbytes en een naam van meer dan 255 bytes worden geweigerd.
    pub fn new(name: &str) -> Result<Self> {
        if name.is_empty() || name.len() > 255 || name.as_bytes().contains(&0) {
            return Err(Error::Limit);
        }
        let mut out = Self {
            bytes: [0; 256],
            len: name.len(),
        };
        out.bytes[..name.len()].copy_from_slice(name.as_bytes());
        Ok(out)
    }
    /// Een begrensd sidecar-pad, vóór I/O samengesteld.
    pub fn suffix(self, suffix: &str) -> Result<Self> {
        let end = self.len.checked_add(suffix.len()).ok_or(Error::Limit)?;
        if end > 255 || suffix.as_bytes().contains(&0) {
            return Err(Error::Limit);
        }
        let mut out = self;
        out.bytes[out.len..end].copy_from_slice(suffix.as_bytes());
        out.len = end;
        out.bytes[end] = 0;
        Ok(out)
    }
    /// Leent de C-naam zonder allocatie.
    pub fn cstr(&self) -> Result<&CStr> {
        CStr::from_bytes_with_nul(&self.bytes[..self.len + 1]).map_err(|_| Error::State)
    }
}
/// Eén tijdelijk geopend bestand; alle paden sluiten het handvat.
pub struct File<'a, B: Storage> {
    storage: &'a mut B,
    id: Option<FileId>,
}
impl<'a, B: Storage> File<'a, B> {
    /// Open(create) maakt alleen ontbrekende bestanden, nooit impliciet truncate.
    pub fn open(storage: &'a mut B, name: &Name, create: bool) -> Result<Self> {
        let id = storage.open(name.cstr()?, OpenFlags(2 | if create { 4 } else { 0 }))?;
        Ok(Self {
            storage,
            id: Some(id),
        })
    }
    fn id(&self) -> Result<FileId> {
        self.id.ok_or(Error::State)
    }
    /// Logische lengte in bytes.
    pub fn size(&mut self) -> Result<u64> {
        let id = self.id()?;
        Ok(self.storage.size(id)?)
    }
    /// Bestaande bytes exact lezen; korte reads worden verder gelezen, nul is EOF.
    pub fn read(&mut self, mut offset: u64, mut dst: &mut [u8]) -> Result {
        let id = self.id()?;
        while !dst.is_empty() {
            let len = dst.len().min(64 << 10);
            let n = self.storage.read(id, offset, &mut dst[..len])?;
            if n == 0 || n > len {
                return Err(Error::Corrupt);
            }
            offset = offset.checked_add(n as u64).ok_or(Error::Limit)?;
            dst = &mut dst[n..];
            self.storage.cooperate()?;
        }
        Ok(())
    }
    /// Volledige write, opgesplitst voor de system-API en de executor.
    pub fn write(&mut self, mut offset: u64, src: &[u8]) -> Result {
        let id = self.id()?;
        for part in src.chunks(64 << 10) {
            let next = offset.checked_add(part.len() as u64).ok_or(Error::Limit)?;
            self.storage.write(id, offset, part)?;
            offset = next;
            self.storage.cooperate()?;
        }
        Ok(())
    }
    /// Nieuw zichtbare bytes zijn nul; krimp verwijdert oude pagina's.
    pub fn truncate(&mut self, size: u64) -> Result {
        let id = self.id()?;
        Ok(self.storage.truncate(id, size)?)
    }
    /// Volledige duurzame barrière inclusief een nieuw aangemaakte naam.
    pub fn sync(&mut self) -> Result {
        let id = self.id()?;
        Ok(self.storage.sync(id, 3)?)
    }
    /// Expliciet sluiten kan een opslagfout aan de eigenaar doorgeven.
    pub fn close(mut self) -> Result {
        if let Some(id) = self.id.take() {
            self.storage.close(id)?;
        }
        Ok(())
    }
}
impl<B: Storage> Drop for File<'_, B> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self.storage.close(id);
        }
    }
}
/// Kleine sidecar volledig lezen; controleer de limiet vóór allocatie.
pub fn read<B: Storage>(storage: &mut B, name: &Name, limit: usize) -> Result<Vec<u8>> {
    let mut f = File::open(storage, name, false)?;
    let n = usize::try_from(f.size()?).map_err(|_| Error::Limit)?;
    if n > limit {
        return Err(Error::Limit);
    }
    let mut bytes = Vec::new();
    reserve(&mut bytes, n)?;
    bytes.resize(n, 0);
    f.read(0, &mut bytes)?;
    f.close()?;
    Ok(bytes)
}
/// Duurzame lokale vervanging; bij fout is de uitkomst onbekend en moet herstel beslissen.
pub fn write<B: Storage>(storage: &mut B, name: &Name, bytes: &[u8]) -> Result {
    let mut f = File::open(storage, name, true)?;
    f.write(0, bytes)?;
    f.truncate(bytes.len() as u64)?;
    f.sync()?;
    f.close()
}
