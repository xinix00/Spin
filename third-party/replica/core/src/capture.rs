//! Begrensde lokale spool: database-eigendom vrijgeven vóór de netwerkupload.
use crate::{
    Error, Result,
    coverage::growth_gap,
    hash,
    local::{File, Name},
    manifest::{MAX_PARTS, Part},
    object::{self, Store},
    reserve, segment,
    tracking::Tracking,
};
use alloc::vec::Vec;
struct Chunk {
    offset: u64,
    len: usize,
    hash: [u8; 32],
}
/// Eén consistent databasebeeld of dirty-setbeeld, los van de actieve SQLite-engine.
/// De eigenaar laat maximaal één capture tegelijk publiceren; een nieuwere spool
/// kan door de hashes nooit stil als de oudere capture worden verstuurd.
pub struct Capture {
    spool: Name,
    chunks: Vec<Chunk>,
    /// Databasegrootte van dit beeld.
    pub size: u64,
    /// SQLite-paginamaten.
    pub page_size: u32,
    /// Revisie op het moment van capture; latere writes blijven dirty bij bevestiging.
    pub revision: u64,
    /// Een volledig snapshot in plaats van een increment.
    pub snapshot: bool,
    pub(crate) counter: u32,
    pub(crate) witness: Option<(u32, [u8; 32])>,
}
impl Capture {
    /// SQLite moet gesloten zijn of de eigenaar moet een echte leestransactie
    /// vasthouden. Alle writes gaan via Tracking; deze exclusieve opslaglening
    /// mag niet naast een verborgen schrijver of padalias bestaan.
    pub fn read<B: replica_sqlite::Storage>(
        b: &mut B,
        path: Name,
        tracking: &Tracking,
        snapshot: bool,
        previous_size: u64,
        segment_bytes: usize,
    ) -> Result<Self> {
        if !(65536..=16 << 20).contains(&segment_bytes) {
            return Err(Error::Limit);
        }
        if tracking.needs_snapshot() && !snapshot {
            return Err(Error::State);
        }
        let mut file = File::open(b, &path, false)?;
        let size = file.size()?;
        let mut header = [0u8; 100];
        file.read(0, &mut header)?;
        file.close()?;
        if &header[..16] != b"SQLite format 3\0" {
            return Err(Error::Corrupt);
        }
        let raw = u16::from_be_bytes([header[16], header[17]]);
        let page_size = if raw == 1 { 65536 } else { u32::from(raw) };
        if !segment::valid_page_size(page_size) || !size.is_multiple_of(u64::from(page_size)) {
            return Err(Error::Corrupt);
        }
        let count = size / u64::from(page_size);
        if count > u64::from(tracking.page_limit()) {
            return Err(Error::Limit);
        }
        if !snapshot && tracking.page_size() != 0 && tracking.page_size() != page_size {
            return Err(Error::State);
        }
        let counter = u32::from_be_bytes([header[24], header[25], header[26], header[27]]);
        if !snapshot {
            if tracking.source_counter.is_some_and(|old| old != counter)
                && tracking.pending()?.is_empty()
            {
                return Err(Error::ForeignWrite);
            }
            if let Some((page, expected)) = tracking.witness
                && !tracking.is_dirty(page)
                && u64::from(page) <= count
            {
                let mut bytes = Vec::new();
                reserve(&mut bytes, page_size as usize)?;
                bytes.resize(page_size as usize, 0);
                let mut source = File::open(b, &path, false)?;
                source.read(u64::from(page - 1) * u64::from(page_size), &mut bytes)?;
                source.close()?;
                if hash(&bytes) != expected {
                    return Err(Error::ForeignWrite);
                }
            }
        }
        // Een snapshot is een bereik, geen paginanummervector ter grootte van de DB.
        let mut pages = if snapshot {
            Vec::new()
        } else {
            tracking.pending()?
        };
        pages.retain(|&page| u64::from(page) <= count);
        if !snapshot
            && growth_gap(
                previous_size,
                size,
                page_size,
                &pages,
                tracking.page_limit(),
            )?
            .is_some()
        {
            return Err(Error::Gap);
        }
        if !snapshot && pages.is_empty() && size == previous_size {
            return Ok(Self {
                spool: path.suffix(".replica-capture")?,
                chunks: Vec::new(),
                size,
                page_size,
                revision: tracking.revision(),
                snapshot,
                counter,
                witness: None,
            });
        }
        let per_segment = segment_bytes / page_size as usize;
        let page_count = if snapshot {
            count as usize
        } else {
            pages.len()
        };
        let chunk_count = page_count.div_ceil(per_segment).max(1);
        if chunk_count > MAX_PARTS {
            return Err(Error::Limit);
        }
        let spool = path.suffix(".replica-capture")?;
        let mut output = File::open(b, &spool, true)?;
        output.truncate(0)?;
        output.close()?;
        let mut chunks = Vec::new();
        reserve(&mut chunks, chunk_count)?;
        let mut offset = 0;
        let mut witness = None;
        let mut snapshot_pages = Vec::new();
        if snapshot {
            reserve(&mut snapshot_pages, per_segment.min(page_count))?;
        }
        // Ook een groottewijziging zonder pagina's krijgt een leeg segment.
        for index in 0..chunk_count {
            let from = (index * per_segment).min(page_count);
            let through = (from + per_segment).min(page_count);
            let numbers = if snapshot {
                snapshot_pages.clear();
                snapshot_pages.extend((from as u32 + 1)..=through as u32);
                snapshot_pages.as_slice()
            } else {
                &pages[from..through]
            };
            let mut data = Vec::new();
            reserve(&mut data, numbers.len() * page_size as usize)?;
            data.resize(numbers.len() * page_size as usize, 0);
            let mut source = File::open(b, &path, false)?;
            let mut start = 0;
            while start < numbers.len() {
                let mut end = start + 1;
                while end < numbers.len() && numbers[end] == numbers[end - 1] + 1 {
                    end += 1;
                }
                source.read(
                    u64::from(numbers[start] - 1) * u64::from(page_size),
                    &mut data[start * page_size as usize..end * page_size as usize],
                )?;
                start = end;
            }
            source.close()?;
            if let Some(&last) = numbers.last() {
                witness = Some((last, hash(&data[data.len() - page_size as usize..])));
            }
            let mut records = Vec::new();
            reserve(&mut records, numbers.len())?;
            for (&page, bytes) in numbers.iter().zip(data.chunks_exact(page_size as usize)) {
                records.push((page, bytes));
            }
            let encoded = segment::encode(page_size, size, &records)?;
            let checksum = hash(&encoded);
            let len = encoded.len();
            let mut output = File::open(b, &spool, false)?;
            output.write(offset, &encoded)?;
            output.close()?;
            chunks.push(Chunk {
                offset,
                len,
                hash: checksum,
            });
            offset = offset.checked_add(len as u64).ok_or(Error::Limit)?;
        }
        let mut output = File::open(b, &spool, false)?;
        output.sync()?;
        output.close()?;
        Ok(Self {
            spool,
            chunks,
            size,
            page_size,
            revision: tracking.revision(),
            snapshot,
            counter,
            witness,
        })
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
    /// Uploadt naar een verse `.../data/<poging>/`-prefix, manifest komt later.
    /// Het geheugen houdt hoogstens één segment, niet een tweede hele database.
    pub fn upload<B: replica_sqlite::Storage, S: Store>(
        &self,
        b: &mut B,
        store: &mut S,
        prefix: &str,
    ) -> Result<Vec<Part>> {
        if !prefix.ends_with('/') || prefix.contains("..") {
            return Err(Error::State);
        }
        let mut parts = Vec::new();
        reserve(&mut parts, self.chunks.len())?;
        for (i, chunk) in self.chunks.iter().enumerate() {
            let mut bytes = Vec::new();
            reserve(&mut bytes, chunk.len)?;
            bytes.resize(chunk.len, 0);
            let mut source = File::open(b, &self.spool, false)?;
            source.read(chunk.offset, &mut bytes)?;
            source.close()?;
            // encode valideerde het segment; de hash bewijst dat de spool gelijk bleef.
            if hash(&bytes) != chunk.hash {
                return Err(Error::Corrupt);
            }
            let mut name = *b"000000.seg";
            let mut n = i + 1;
            for b in name[..6].iter_mut().rev() {
                *b = b'0' + (n % 10) as u8;
                n /= 10;
            }
            let key = object::key(
                prefix,
                core::str::from_utf8(&name).map_err(|_| Error::State)?,
            )?;
            store.put(&key, &bytes)?;
            parts.push(Part {
                key,
                size: bytes.len() as u64,
                hash: chunk.hash,
            });
        }
        Ok(parts)
    }
}
