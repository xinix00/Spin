//! De draadvorm van [`super::OP_READ_MANY`]: een lijst leesopdrachten
//! heen, per opdracht een uitkomst terug. Afgekeken van io_uring (de vorm,
//! niet de omvang): de app zet veel lezingen in één aanvraag, de kern zet
//! ze in één keer bij het device en antwoordt één keer voor allemaal.
//!
//! ```text
//! heen:  path = het bestand, n = het aantal opdrachten,
//!        data = per opdracht  off u64 | len u32 | 0 u32
//! terug: size = de som van de gelezen bytes,
//!        data = per opdracht  got u32 | status u16 | 0 u16,
//!               daarna de gelezen bytes van elke opdracht achter elkaar
//! ```
//!
//! Een opdracht die faalt (een blokfout) krijgt haar eigen status en nul
//! bytes; de andere staan er gewoon. Voorbij het einde van het bestand is
//! geen fout maar minder bytes, zoals bij een gewone lees. Wat de hele
//! lijst ongeldig maakt (leeg, te lang, te groot, een kapotte vorm), is een
//! fout van de call vóór er één opdracht naar het device gaat.

/// Zoveel opdrachten hoogstens per call: de diepte van de hopfs-actor
/// (`FS_DEPTH`); de NVMe-kern (driver-nvme `DEPTH`) neemt er twee bundels
/// van tegelijk. Een grotere bundel wacht langer op zijn traagste lees.
pub const MAX_OPS: usize = 16;
/// Eén opdracht op de draad: `off u64 | len u32 | 0 u32`.
pub const OP_LEN: usize = 16;
/// Eén uitkomst op de draad: `got u32 | status u16 | 0 u16`.
pub const RESULT_LEN: usize = 8;
/// Zoveel bytes hoogstens samen per call. Bundelen is voor kleine lezingen
/// (zestien keer 32 KiB); een grote lees is één `OP_READ` van tot 1 MiB.
pub const MAX_BYTES: usize = 512 << 10;

/// Waarom een lijst niet op de draad mag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// Geen opdrachten, geen hele opdrachten, niet `n` lang, of een
    /// gereserveerd woord dat niet nul is.
    Shape,
    /// Meer dan [`MAX_OPS`] opdrachten.
    TooMany(usize),
    /// Samen meer dan [`MAX_BYTES`] bytes.
    TooLarge(usize),
}

/// Schrijft opdracht `i` in de lijst `dst`.
pub fn put_op(dst: &mut [u8], i: usize, off: u64, len: u32) -> Option<()> {
    let d = dst.get_mut(i * OP_LEN..(i + 1) * OP_LEN)?;
    d[..8].copy_from_slice(&off.to_le_bytes());
    d[8..12].copy_from_slice(&len.to_le_bytes());
    d[12..].fill(0);
    Some(())
}

/// Opdracht `i` uit de lijst `src`: `(off, len)`.
#[must_use]
pub fn op(src: &[u8], i: usize) -> Option<(u64, u32)> {
    let s = src.get(i * OP_LEN..(i + 1) * OP_LEN)?;
    let mut off = [0u8; 8];
    off.copy_from_slice(&s[..8]);
    Some((u64::from_le_bytes(off), word(s, 8)))
}

/// Toetst een lijst van `n` opdrachten (het `n` van de kop) en geeft de som
/// van de lengtes.
pub fn check(src: &[u8], n: u64) -> Result<usize, Invalid> {
    let count = src.len() / OP_LEN;
    if count > MAX_OPS {
        return Err(Invalid::TooMany(count));
    }
    if count == 0 || !src.len().is_multiple_of(OP_LEN) || n != count as u64 {
        return Err(Invalid::Shape);
    }
    let mut sum = 0usize;
    for s in src.chunks_exact(OP_LEN) {
        if word(s, 12) != 0 {
            return Err(Invalid::Shape);
        }
        sum = sum.saturating_add(word(s, 8) as usize);
    }
    if sum > MAX_BYTES {
        return Err(Invalid::TooLarge(sum));
    }
    Ok(sum)
}

/// Schrijft uitkomst `i` in de tabel `dst`.
pub fn put_result(dst: &mut [u8], i: usize, got: u32, status: u16) -> Option<()> {
    let d = dst.get_mut(i * RESULT_LEN..(i + 1) * RESULT_LEN)?;
    d[..4].copy_from_slice(&got.to_le_bytes());
    d[4..6].copy_from_slice(&status.to_le_bytes());
    d[6..].fill(0);
    Some(())
}

/// Uitkomst `i` uit de tabel `src`: `(got, status)`.
#[must_use]
pub fn result(src: &[u8], i: usize) -> Option<(u32, u16)> {
    let s = src.get(i * RESULT_LEN..(i + 1) * RESULT_LEN)?;
    Some((word(s, 0), u16::from_le_bytes([s[4], s[5]])))
}

fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_goes_there_and_back_and_its_bounds_hold() {
        let mut l = [0u8; 3 * OP_LEN];
        put_op(&mut l, 0, 1 << 40, 4096).unwrap();
        put_op(&mut l, 1, 7, 0).unwrap();
        put_op(&mut l, 2, 0, 12).unwrap();
        assert_eq!(op(&l, 0), Some((1 << 40, 4096)));
        assert_eq!(op(&l, 2), Some((0, 12)));
        assert_eq!(op(&l, 3), None);
        assert_eq!(check(&l, 3), Ok(4108));
        assert_eq!(check(&l, 2), Err(Invalid::Shape), "n van de kop");
        assert_eq!(check(&[], 0), Err(Invalid::Shape), "leeg");
        assert_eq!(check(&l[..OP_LEN + 1], 1), Err(Invalid::Shape));
        l[OP_LEN + 12] = 1;
        assert_eq!(check(&l, 3), Err(Invalid::Shape), "gereserveerd");
        let long = [0u8; (MAX_OPS + 1) * OP_LEN];
        assert_eq!(check(&long, 17), Err(Invalid::TooMany(17)));
        let mut big = [0u8; 2 * OP_LEN];
        put_op(&mut big, 0, 0, (MAX_BYTES / 2) as u32).unwrap();
        put_op(&mut big, 1, 0, (MAX_BYTES / 2 + 1) as u32).unwrap();
        assert_eq!(check(&big, 2), Err(Invalid::TooLarge(MAX_BYTES + 1)));
        let mut t = [0u8; 2 * RESULT_LEN];
        put_result(&mut t, 1, 4096, 1).unwrap();
        assert_eq!(result(&t, 1), Some((4096, 1)));
        assert_eq!(result(&t, 0), Some((0, 0)));
        assert!(put_result(&mut t, 2, 0, 0).is_none());
    }
}
