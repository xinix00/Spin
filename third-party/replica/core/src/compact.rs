//! Gehele, aaneengesloten vensters samenvoegen; krimpgeschiedenis blijft behouden.
use crate::{
    Error, Result, hash,
    manifest::{MAX_PARTS, Manifest, Part},
    marker::{self, LocalMarker},
    object::{self, Store},
    replication::generation_prefix,
    reserve, segment,
    time::Time,
};
use alloc::{string::String, vec::Vec};
use core::fmt::Write;
use replica_sqlite::Storage;
const NONE: u32 = u32::MAX;
/// Het gesloten venster dat de eigenaar wil publiceren.
pub struct Window {
    /// Doelniveau; ieder invoermanifest ligt precies één niveau lager.
    pub level: u32,
    /// Exclusief begin.
    pub start: Time,
    /// Inclusief einde.
    pub end: Time,
    /// Expliciet databasebudget voor de winnende-paginakaart.
    pub page_limit: u32,
    /// Maximum paginabytes per uitvoersegment.
    pub segment_bytes: usize,
}
/// Houdt per pagina alleen het nummer van het laatste bronsegment bij; de data
/// wordt in een tweede pass nogmaals gehasht en met één segment tegelijk gelezen.
pub fn merge<B: Storage, S: Store>(
    b: &mut B,
    store: &mut S,
    namespace: &str,
    local: &mut LocalMarker,
    window: Window,
    inputs: &[&Manifest],
) -> Result<Manifest> {
    let Window {
        level,
        start,
        end,
        page_limit,
        segment_bytes,
    } = window;
    if !local.value.complete
        || local.value.uncertain != 0
        || inputs.is_empty()
        || level == 0
        || level > 32
        || start >= end
        || start.nanos() != 0
        || end.nanos() != 0
        || !(65536..=16 << 20).contains(&segment_bytes)
    {
        return Err(Error::State);
    }
    let prefix = generation_prefix(namespace, &local.value.generation)?;
    let mut refs = Vec::new();
    let mut seq = inputs[0].first.checked_sub(1).ok_or(Error::Corrupt)?;
    if seq == 0 {
        return Err(Error::State);
    }
    for m in inputs {
        m.validate(&prefix)?;
        if m.first != seq + 1
            || m.level + 1 != level
            || m.at <= start
            || m.at > end
            || (m.level > 0 && (m.start < start || m.end > end))
        {
            return Err(Error::Gap);
        }
        if refs
            .len()
            .checked_add(m.parts.len())
            .is_none_or(|n| n > 65536)
        {
            return Err(Error::Limit);
        }
        reserve(&mut refs, m.parts.len())?;
        refs.extend(m.parts.iter());
        seq = m.sequence;
    }
    // Frontier vóór remote publicatie: na een crash landen nieuwe commits erbuiten.
    if end > local.value.sealed_at {
        local.value.sealed_at = end;
        local.save(b)?;
    }
    let attempt = marker::new_generation(b, end)?;
    let data_prefix = object::key(
        &object::key(&prefix, "data/")?,
        &object::key(&attempt, "/")?,
    )?;
    let mut winners: Vec<u32> = Vec::new();
    let mut page_size = 0;
    let mut size = 0;
    let mut source_index = 0;
    let mut minimum = inputs[0].min_size;
    for m in inputs {
        minimum = minimum.min(m.min_size);
        if page_size != 0 && m.min_size < size {
            winners.truncate((m.min_size / u64::from(page_size)) as usize);
        }
        for part in &m.parts {
            let bytes = object::committed(store, &part.key, part.size as usize)?;
            let seg = part.read(&bytes)?;
            if (page_size != 0 && page_size != seg.page_size())
                || m.min_size > seg.database_size()
                || !m.min_size.is_multiple_of(u64::from(seg.page_size()))
            {
                return Err(Error::Corrupt);
            }
            if seg.database_size() / u64::from(seg.page_size()) > u64::from(page_limit) {
                return Err(Error::Limit);
            }
            if seg.database_size() < size {
                winners.truncate((seg.database_size() / u64::from(seg.page_size())) as usize);
            }
            for (page, _) in seg.pages() {
                let index = (page - 1) as usize;
                if index >= winners.len() {
                    let n = index + 1 - winners.len();
                    crate::grow(&mut winners, n, page_limit as usize)?;
                    winners.resize(index + 1, NONE);
                }
                winners[index] = source_index;
            }
            page_size = seg.page_size();
            size = seg.database_size();
            source_index += 1;
        }
    }
    let mut output = Output {
        parts: Vec::new(),
        numbers: Vec::new(),
        data: Vec::new(),
        page_size,
        size,
        prefix: &data_prefix,
    };
    let segment_pages = segment_bytes / page_size as usize;
    reserve(&mut output.numbers, segment_pages)?;
    reserve(&mut output.data, segment_pages * page_size as usize)?;
    for (index, part) in refs.into_iter().enumerate() {
        let bytes = object::committed(store, &part.key, part.size as usize)?;
        let seg = part.read(&bytes)?;
        for (page, data) in seg.pages() {
            if winners.get((page - 1) as usize).copied() != Some(index as u32) {
                continue;
            }
            output.numbers.push(page);
            output.data.extend_from_slice(data);
            if output.numbers.len() >= segment_pages {
                output.flush(store)?;
            }
        }
    }
    if !output.numbers.is_empty() || output.parts.is_empty() {
        output.flush(store)?;
    }
    let last = inputs.last().ok_or(Error::State)?;
    let manifest = Manifest {
        min_size: minimum,
        first: inputs[0].first,
        sequence: seq,
        at: last.at,
        level,
        start,
        end,
        parts: output.parts,
    };
    let key = window_key(&prefix, level, start, end)?;
    object::publish(store, &key, &manifest, &prefix)?;
    Ok(manifest)
}
struct Output<'a> {
    parts: Vec<Part>,
    numbers: Vec<u32>,
    data: Vec<u8>,
    page_size: u32,
    size: u64,
    prefix: &'a str,
}
impl Output<'_> {
    fn flush<S: Store>(&mut self, store: &mut S) -> Result {
        if self.parts.len() >= MAX_PARTS {
            return Err(Error::Limit);
        }
        let mut records = Vec::new();
        reserve(&mut records, self.numbers.len())?;
        for (&page, data) in self
            .numbers
            .iter()
            .zip(self.data.chunks_exact(self.page_size as usize))
        {
            records.push((page, data));
        }
        let bytes = segment::encode(self.page_size, self.size, &records)?;
        let mut name = *b"000000.seg";
        let mut n = self.parts.len() + 1;
        for b in name[..6].iter_mut().rev() {
            *b = b'0' + (n % 10) as u8;
            n /= 10;
        }
        let key = object::key(
            self.prefix,
            core::str::from_utf8(&name).map_err(|_| Error::State)?,
        )?;
        crate::grow(&mut self.parts, 1, MAX_PARTS)?;
        store.put(&key, &bytes)?;
        self.parts.push(Part {
            key,
            size: bytes.len() as u64,
            hash: hash(&bytes),
        });
        self.numbers.clear();
        self.data.clear();
        Ok(())
    }
}
/// De bestaande Go-venstersleutel, met gehele UTC-seconden.
pub fn window_key(prefix: &str, level: u32, start: Time, end: Time) -> Result<String> {
    if level == 0
        || level > 32
        || start.seconds() < 0
        || start >= end
        || start.nanos() != 0
        || end.nanos() != 0
    {
        return Err(Error::State);
    }
    let mut suffix = String::new();
    suffix.try_reserve_exact(64).map_err(|_| Error::Memory)?;
    write!(
        &mut suffix,
        "L{level}/{:010}-{:010}/complete",
        start.seconds(),
        end.seconds()
    )
    .map_err(|_| Error::Memory)?;
    object::key(prefix, &suffix)
}
/// Verwijder zichtbaarheid vóór data, uitsluitend na een bevestigd dekkend venster.
/// `live` bevat alle overblijvende manifesten; gedeelde onderdelen blijven staan.
pub fn prune<S: Store>(
    store: &mut S,
    key: &str,
    old: &Manifest,
    replacement: &Manifest,
    live: &[&Manifest],
) -> Result {
    // Het object moet zelf nog terugleesbaar zijn; alleen een in-memory manifest
    // is geen bewijs dat een vorige (mogelijk mislukte) PUT echt is gepubliceerd.
    let data_prefix = replacement
        .parts
        .first()
        .and_then(|p| p.key.rsplit_once("/data/"))
        .map(|(head, _)| head)
        .ok_or(Error::Corrupt)?;
    let prefix = object::key(data_prefix, "/")?;
    old.validate(&prefix)?;
    replacement.validate(&prefix)?;
    if replacement.level != old.level + 1
        || replacement.first > old.first
        || replacement.sequence < old.sequence
    {
        return Err(Error::State);
    }
    let replacement_key = window_key(
        &prefix,
        replacement.level,
        replacement.start,
        replacement.end,
    )?;
    if key == replacement_key || !key.starts_with(&prefix) {
        return Err(Error::State);
    }
    let actual_old = Manifest::decode(
        &object::committed(store, key, hop_types::json::MAX_INPUT)?,
        &prefix,
    )?;
    if actual_old != *old {
        return Err(Error::Corrupt);
    }
    let actual = Manifest::decode(
        &object::committed(store, &replacement_key, hop_types::json::MAX_INPUT)?,
        &prefix,
    )?;
    if actual != *replacement {
        return Err(Error::Corrupt);
    }
    // Geen kandidaatdelen weggooien wanneer de caller de vervanger uit live vergat.
    store.delete(key)?;
    for part in &old.parts {
        if replacement.parts.iter().any(|p| p.key == part.key)
            || live
                .iter()
                .any(|m| m.parts.iter().any(|p| p.key == part.key))
        {
            continue;
        }
        store.delete(&part.key)?;
    }
    Ok(())
}
