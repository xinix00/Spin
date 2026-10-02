//! SPINSEG1: paginabeelden met databasegrootte en SHA-256 van de hele body.
use crate::{Error, Result, hash, reserve};
use alloc::vec::Vec;
/// Eén segment mag maximaal 32 MiB kosten, vóór enige allocatie gecontroleerd.
pub const MAX_BYTES: usize = 32 << 20;
const HEADER: usize = 24;
/// Een gevalideerd, geleend segment; de bytes kunnen tijdens gebruik niet wijzigen.
#[derive(Debug)]
pub struct Segment<'a> {
    bytes: &'a [u8],
    page_size: u32,
    database_size: u64,
}
/// SQLite-paginamaten uit het Go-formaat.
pub const fn valid_page_size(n: u32) -> bool {
    n >= 512 && n <= 65536 && n.is_power_of_two()
}
impl<'a> Segment<'a> {
    /// Controleert lengtes, hash, pagina-identiteiten en duplicaten vóór gebruik.
    pub fn decode(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::Limit);
        }
        if bytes.len() < 56 || &bytes[..8] != b"SPINSEG1" {
            return Err(Error::Corrupt);
        }
        let page_size = u32::from_be_bytes(bytes[8..12].try_into().map_err(|_| Error::Corrupt)?);
        let database_size =
            u64::from_be_bytes(bytes[12..20].try_into().map_err(|_| Error::Corrupt)?);
        let count =
            u32::from_be_bytes(bytes[20..24].try_into().map_err(|_| Error::Corrupt)?) as usize;
        if !valid_page_size(page_size)
            || database_size > i64::MAX as u64
            || !database_size.is_multiple_of(u64::from(page_size))
        {
            return Err(Error::Corrupt);
        }
        let size = count
            .checked_mul(page_size as usize + 4)
            .and_then(|n| n.checked_add(56))
            .ok_or(Error::Corrupt)?;
        if size != bytes.len() || hash(&bytes[..size - 32]) != bytes[size - 32..] {
            return Err(Error::Corrupt);
        }
        let out = Self {
            bytes,
            page_size,
            database_size,
        };
        let mut seen = Vec::new();
        reserve(&mut seen, count)?;
        for (number, _) in out.pages() {
            if number == 0 || u64::from(number) * u64::from(page_size) > database_size {
                return Err(Error::Corrupt);
            }
            seen.push(number);
        }
        seen.sort_unstable();
        if seen.windows(2).any(|p| p[0] == p[1]) {
            return Err(Error::Corrupt);
        }
        Ok(out)
    }
    /// Lengte van elke pagina in bytes.
    pub const fn page_size(&self) -> u32 {
        self.page_size
    }
    /// Bestandsgrootte nadat alle pagina's uit dit segment zijn toegepast.
    pub const fn database_size(&self) -> u64 {
        self.database_size
    }
    /// Pagina's in de oorspronkelijke volgorde; nummering begint bij één.
    pub fn pages(&self) -> impl ExactSizeIterator<Item = (u32, &'a [u8])> + '_ {
        self.bytes[HEADER..self.bytes.len() - 32]
            .chunks_exact(self.page_size as usize + 4)
            .map(|p| (u32::from_be_bytes([p[0], p[1], p[2], p[3]]), &p[4..]))
    }
}
/// Schrijft exact het bestaande Go-formaat; ongeldige invoer levert geen segment.
pub fn encode(page_size: u32, database_size: u64, pages: &[(u32, &[u8])]) -> Result<Vec<u8>> {
    if !valid_page_size(page_size)
        || database_size > i64::MAX as u64
        || !database_size.is_multiple_of(u64::from(page_size))
    {
        return Err(Error::Corrupt);
    }
    let size = pages
        .len()
        .checked_mul(page_size as usize + 4)
        .and_then(|n| n.checked_add(56))
        .ok_or(Error::Limit)?;
    if size > MAX_BYTES {
        return Err(Error::Limit);
    }
    let mut out = Vec::new();
    reserve(&mut out, size)?;
    out.extend_from_slice(b"SPINSEG1");
    out.extend_from_slice(&page_size.to_be_bytes());
    out.extend_from_slice(&database_size.to_be_bytes());
    out.extend_from_slice(&(pages.len() as u32).to_be_bytes());
    for &(number, data) in pages {
        if data.len() != page_size as usize {
            return Err(Error::Corrupt);
        }
        out.extend_from_slice(&number.to_be_bytes());
        out.extend_from_slice(data);
    }
    out.extend_from_slice(&hash(&out));
    Segment::decode(&out)?;
    Ok(out)
}
