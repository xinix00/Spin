//! Paginadekking; voorkomt een geldig ogende generatie met nooit verstuurde gaten.
use crate::{Error, Result, reserve, segment::valid_page_size};
use alloc::vec::Vec;
/// Begrensde bitmap; het budget is van de eigenaar, geen allocatie naar externe DBSize.
pub struct Pages {
    bits: Vec<u64>,
    limit: u32,
}
impl Pages {
    /// Het door de eigenaar gekozen maximum.
    pub const fn limit(&self) -> u32 {
        self.limit
    }
    /// Verwijdert alle bits zonder opnieuw te alloceren.
    pub fn clear(&mut self) {
        self.bits.fill(0);
    }
    /// Geeft de genoteerde nummers in oplopende volgorde.
    pub fn numbers(&self) -> Result<Vec<u32>> {
        let count = self.bits.iter().map(|w| w.count_ones() as usize).sum();
        let mut out = Vec::new();
        reserve(&mut out, count)?;
        for (index, &word) in self.bits.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                let bit = word.trailing_zeros();
                out.push(index as u32 * 64 + bit + 1);
                word &= word - 1;
            }
        }
        Ok(out)
    }
    /// Maximaal aantal pagina's dat deze eigenaar toestaat.
    pub const fn new(limit: u32) -> Self {
        Self {
            bits: Vec::new(),
            limit,
        }
    }
    /// Noteert een verstuurde pagina, ook als een later segment haar weer wegtruncatet.
    pub fn add(&mut self, page: u32) -> Result {
        if page == 0 || page > self.limit {
            return Err(Error::Limit);
        }
        let index = ((page - 1) / 64) as usize;
        if index >= self.bits.len() {
            let n = index + 1 - self.bits.len();
            crate::grow(
                &mut self.bits,
                n,
                (u64::from(self.limit).div_ceil(64)) as usize,
            )?;
            self.bits.resize(index + 1, 0);
        }
        self.bits[index] |= 1u64 << ((page - 1) % 64);
        Ok(())
    }
    /// Is deze pagina ooit verstuurd?
    pub fn contains(&self, page: u32) -> bool {
        page != 0
            && self
                .bits
                .get(((page - 1) / 64) as usize)
                .is_some_and(|w| w & (1u64 << ((page - 1) % 64)) != 0)
    }
    /// Eerste ontbrekende pagina en totaal; SQLite's lock-byte-pagina telt niet mee.
    pub fn shortfall(&self, size: u64, page_size: u32) -> Result<Option<(u32, u64)>> {
        if !valid_page_size(page_size) || !size.is_multiple_of(u64::from(page_size)) {
            return Err(Error::Corrupt);
        }
        let count = size / u64::from(page_size);
        if count > u64::from(self.limit) {
            return Err(Error::Limit);
        }
        let lock = (1u64 << 30) / u64::from(page_size) + 1;
        let mut first = 0;
        let mut missing = 0;
        for page in 1..=count {
            if page != lock && !self.contains(page as u32) {
                if missing == 0 {
                    first = page as u32;
                }
                missing += 1;
            }
        }
        Ok(if missing == 0 {
            None
        } else {
            Some((first, missing))
        })
    }
}
/// Groei moet alle nieuwe pagina's bevatten; geheugen hangt alleen van de nieuwe staart af.
pub fn growth_gap(
    previous: u64,
    size: u64,
    page_size: u32,
    pages: &[u32],
    limit: u32,
) -> Result<Option<(u32, u64)>> {
    if !valid_page_size(page_size)
        || !previous.is_multiple_of(u64::from(page_size))
        || !size.is_multiple_of(u64::from(page_size))
    {
        return Err(Error::Corrupt);
    }
    if size <= previous {
        return Ok(None);
    }
    let from = previous / u64::from(page_size) + 1;
    let through = size / u64::from(page_size);
    if through > u64::from(u32::MAX) || through - from + 1 > u64::from(limit) {
        return Err(Error::Limit);
    }
    let mut have = Pages::new(limit);
    for &page in pages {
        if u64::from(page) >= from && u64::from(page) <= through {
            have.add((u64::from(page) - from + 1) as u32)?;
        }
    }
    let lock = (1u64 << 30) / u64::from(page_size) + 1;
    let mut first = 0;
    let mut missing = 0;
    for page in from..=through {
        if page != lock && !have.contains((page - from + 1) as u32) {
            if missing == 0 {
                first = page as u32;
            }
            missing += 1;
        }
    }
    Ok(if missing == 0 {
        None
    } else {
        Some((first, missing))
    })
}
