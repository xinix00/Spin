//! Eerst alle committed bytes naar scratch, dan verificatie en duurzame restore-intent.
use crate::{
    Error, Result,
    coverage::Pages,
    local::{self, File, Name},
    manifest::Layout,
    object::{self, Store},
    reserve,
    time::Time,
};
use alloc::vec::Vec;
use replica_sqlite::{FileId, OpenFlags, Storage};
/// Het bewezen herstelpunt; de lokale Replica-marker kan hierop worden bijgewerkt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restored {
    /// Laatste aaneengesloten sequence.
    pub sequence: u64,
    /// Laatste transactietijd.
    pub at: Time,
    /// Definitieve databasegrootte.
    pub size: u64,
    /// SQLite-paginamaten.
    pub page_size: u32,
    /// Gedownloade segmentbytes.
    pub bytes: u64,
}
/// Een complete scratchkopie; nog geen toestemming om de database te vervangen.
pub struct Staged {
    destination: Name,
    scratch: Name,
    result: Restored,
}
/// De verificatiecallback heeft de complete scratchdatabase goedgekeurd.
pub struct Verified(Staged);
/// Kopieert een consistent herstelplan met een expliciet maximumpaginabudget.
/// De aanroeper is de exclusieve eigenaar; SQLite moet gesloten zijn.
pub fn stage<B: Storage, S: Store>(
    b: &mut B,
    store: &mut S,
    layout: &Layout,
    at: Option<Time>,
    destination: Name,
    page_limit: u32,
) -> Result<Staged> {
    let plan = layout.plan(at)?;
    let scratch = destination.suffix(".replica-restore-data")?;
    let mut file = File::open(b, &scratch, true)?;
    file.truncate(0)?;
    let mut shipped = Pages::new(page_limit);
    // Keep RPC-sized contiguous runs together; sparse or reordered pages still
    // flush in segment order. Never allocate according to the database size.
    let mut batch = Vec::new();
    reserve(&mut batch, 64 << 10)?;
    let mut result = Restored {
        sequence: 0,
        at: Time::ZERO,
        size: 0,
        page_size: 0,
        bytes: 0,
    };
    for m in plan {
        // Dit gebeurt voor ieder niveau: krimp/groei mag oude baselinepagina's niet doen herleven.
        if m.min_size > u64::from(page_limit) * 65536 {
            return Err(Error::Limit);
        }
        file.truncate(m.min_size)?;
        for part in &m.parts {
            let bytes = object::committed(
                store,
                &part.key,
                usize::try_from(part.size).map_err(|_| Error::Limit)?,
            )?;
            let seg = part.read(&bytes)?;
            let size = seg.page_size();
            if seg.database_size() / u64::from(size) > u64::from(page_limit) {
                return Err(Error::Limit);
            }
            if (result.page_size != 0 && result.page_size != size)
                || m.min_size > seg.database_size()
                || !m.min_size.is_multiple_of(u64::from(size))
            {
                return Err(Error::Corrupt);
            }
            let mut start = 0;
            for (page, data) in seg.pages() {
                let offset = u64::from(page - 1) * u64::from(size);
                if !batch.is_empty()
                    && (offset != start + batch.len() as u64 || batch.len() + data.len() > 64 << 10)
                {
                    file.write(start, &batch)?;
                    batch.clear();
                }
                if batch.is_empty() {
                    start = offset;
                }
                batch.extend_from_slice(data);
                shipped.add(page)?;
            }
            if !batch.is_empty() {
                file.write(start, &batch)?;
                batch.clear();
            }
            file.truncate(seg.database_size())?;
            result.page_size = size;
            result.size = seg.database_size();
            result.bytes = result.bytes.checked_add(part.size).ok_or(Error::Limit)?;
        }
        result.sequence = m.sequence;
        result.at = m.at;
    }
    if shipped.shortfall(result.size, result.page_size)?.is_some() {
        return Err(Error::Gap);
    }
    file.sync()?;
    file.close()?;
    Ok(Staged {
        destination,
        scratch,
        result,
    })
}
impl Staged {
    /// Laat bijvoorbeeld SQLite's integrity_check lopen, vóór de bestaande DB wordt aangeraakt.
    pub fn verify<B: Storage>(
        self,
        b: &mut B,
        verify: impl FnOnce(&mut B, &Name) -> Result,
    ) -> Result<Verified> {
        verify(b, &self.scratch)?;
        Ok(Verified(self))
    }
}
impl Verified {
    /// Zet intent vóór journalverwijdering en kopiëren. Bij elke fout na intent
    /// moet Prepare opnieuw downloaden/publiceren vóór SQLite mag openen.
    /// Een achtergebleven scratchbestand wordt bij de volgende restore overschreven.
    pub fn publish<B: Storage>(self, b: &mut B, generation: &str) -> Result<Restored> {
        if generation.is_empty() || generation.len() > 255 || generation.contains(['\0', '/', '\\'])
        {
            return Err(Error::State);
        }
        let s = self.0;
        let intent = s.destination.suffix(".replica-restoring")?;
        local::write(b, &intent, generation.as_bytes())?;
        let journal = s.destination.suffix("-journal")?;
        if b.exists(journal.cstr()?)? {
            b.remove(journal.cstr()?, true)?;
        }
        copy(b, &s.scratch, &s.destination, s.result.size)?;
        b.remove(intent.cstr()?, true)?;
        // De bestemming is al bevestigd; cleanup verandert de commit niet.
        let _ = b.remove(s.scratch.cstr()?, true);
        Ok(s.result)
    }
}
struct Pair<'a, B: Storage> {
    storage: &'a mut B,
    source: Option<FileId>,
    destination: Option<FileId>,
}
impl<B: Storage> Drop for Pair<'_, B> {
    fn drop(&mut self) {
        if let Some(id) = self.source.take() {
            let _ = self.storage.close(id);
        }
        if let Some(id) = self.destination.take() {
            let _ = self.storage.close(id);
        }
    }
}
fn copy<B: Storage>(storage: &mut B, source: &Name, destination: &Name, size: u64) -> Result {
    let mut bytes = Vec::new();
    reserve(&mut bytes, 64 << 10)?;
    bytes.resize(64 << 10, 0);
    let source = storage.open(source.cstr()?, OpenFlags(2))?;
    let mut pair = Pair {
        storage,
        source: Some(source),
        destination: None,
    };
    let destination = pair.storage.open(destination.cstr()?, OpenFlags(6))?;
    pair.destination = Some(destination);
    let mut offset = 0;
    while offset < size {
        let n = (size - offset).min(bytes.len() as u64) as usize;
        let mut read = 0;
        while read < n {
            let got = pair
                .storage
                .read(source, offset + read as u64, &mut bytes[read..n])?;
            if got == 0 || got > n - read {
                return Err(Error::Corrupt);
            }
            read += got;
        }
        pair.storage.write(destination, offset, &bytes[..n])?;
        offset += n as u64;
        pair.storage.cooperate()?;
    }
    pair.storage.truncate(destination, size)?;
    pair.storage.sync(destination, 3)?;
    // Close-fouten komen terug vóór de intent verwijderd kan worden.
    pair.destination = None;
    pair.storage.close(destination)?;
    pair.source = None;
    pair.storage.close(source)?;
    Ok(())
}
