//! SPINDRT1, inclusief het afgebroken laatste append-record en dubbele logkeuze.
use crate::{Error, Result, hash, reserve};
use alloc::vec::Vec;
/// Budget voor een ingelezen dirty log; grotere logs vragen een nieuw snapshot.
pub const MAX_BYTES: usize = 64 << 20;
/// Een volledig gevalideerd log; alleen een onvolledig laatste record mag ontbreken.
#[derive(Debug)]
pub struct Log<'a> {
    /// Generatie waartoe de sequence behoort.
    pub generation: &'a str,
    /// Laatste manifest vóór de bijgehouden veranderingen.
    pub sequence: u64,
    records: &'a [u8],
}
impl<'a> Log<'a> {
    /// Beschadiging, inclusief een broken-record nul, vraagt een volledig snapshot.
    pub fn decode(data: &'a [u8]) -> Result<Self> {
        if data.len() > MAX_BYTES {
            return Err(Error::Limit);
        }
        if data.len() < 34 || &data[..8] != b"SPINDRT1" {
            return Err(Error::Corrupt);
        }
        let sequence = u64::from_be_bytes(data[8..16].try_into().map_err(|_| Error::Corrupt)?);
        if sequence > i64::MAX as u64 || data[16..24] != (!sequence).to_be_bytes() {
            return Err(Error::Corrupt);
        }
        let n = usize::from(u16::from_be_bytes([data[24], data[25]]));
        let end = 34 + n;
        if data.len() < end || hash(&data[..end - 8])[..8] != data[end - 8..end] {
            return Err(Error::Corrupt);
        }
        let generation = core::str::from_utf8(&data[26..end - 8]).map_err(|_| Error::Corrupt)?;
        let records = &data[end..];
        for r in records.chunks_exact(8) {
            let number = u32::from_be_bytes([r[0], r[1], r[2], r[3]]);
            if number == 0 || r[4..] != (!number).to_be_bytes() {
                return Err(Error::Corrupt);
            }
        }
        Ok(Self {
            generation,
            sequence,
            records,
        })
    }
    /// De nummers; duplicaten zijn toegestaan, want append kan worden herhaald.
    pub fn pages(&self) -> impl ExactSizeIterator<Item = u32> + '_ {
        self.records
            .chunks_exact(8)
            .map(|r| u32::from_be_bytes([r[0], r[1], r[2], r[3]]))
    }
}
/// Header apart: herschrijven legt eerst records en hun barrier vast, daarna deze header.
pub fn header(generation: &str, sequence: u64) -> Result<Vec<u8>> {
    if generation.len() > u16::MAX as usize || sequence > i64::MAX as u64 {
        return Err(Error::Limit);
    }
    let mut out = Vec::new();
    reserve(&mut out, 34 + generation.len())?;
    out.extend_from_slice(b"SPINDRT1");
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&(!sequence).to_be_bytes());
    out.extend_from_slice(&(generation.len() as u16).to_be_bytes());
    out.extend_from_slice(generation.as_bytes());
    out.extend_from_slice(&hash(&out)[..8]);
    Ok(out)
}
/// Append-record; nul markeert expliciet een niet aan pagina's toe te wijzen write.
pub fn record(page: u32) -> [u8; 8] {
    let mut out = [0; 8];
    out[..4].copy_from_slice(&page.to_be_bytes());
    out[4..].copy_from_slice(&(!page).to_be_bytes());
    out
}
/// Beide bestanden worden getoetst; kies hoogste sequence die niet voorbij de marker ligt.
pub fn select<'a>(
    a: Option<&'a [u8]>,
    b: Option<&'a [u8]>,
    generation: &str,
    sequence: u64,
) -> Result<Log<'a>> {
    let mut best: Option<Log<'a>> = None;
    for bytes in [a, b].into_iter().flatten() {
        let log = Log::decode(bytes)?;
        if log.generation != generation {
            continue;
        }
        if log.sequence > sequence {
            return Err(Error::Corrupt);
        }
        if best.as_ref().is_none_or(|old| log.sequence > old.sequence) {
            best = Some(log);
        }
    }
    best.ok_or(Error::Corrupt)
}
