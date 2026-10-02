//! RFC3339Nano met Go's echte nul-tijd, gescheiden van de Unix-epoch.
use crate::{Error, Result};
use alloc::string::String;
/// UTC-tijdstip; de representatie houdt ook jaar 0001 en nanoseconden exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time {
    seconds: i64,
    nanos: u32,
}
impl Time {
    /// Go's `time.Time{}`; dit is niet 1970-01-01.
    pub const ZERO: Self = Self {
        seconds: -62135596800,
        nanos: 0,
    };
    /// Tijdstip in het schrijfbare jaarbereik 0000..=9999.
    pub fn unix(seconds: i64, nanos: u32) -> Result<Self> {
        if !(-62167219200..=253402300799).contains(&seconds) || nanos >= 1_000_000_000 {
            return Err(Error::Corrupt);
        }
        Ok(Self { seconds, nanos })
    }
    /// Gehele seconden sinds 1970, ook voor tijden vóór de epoch.
    pub const fn seconds(self) -> i64 {
        self.seconds
    }
    /// Nanoseconden binnen deze seconde.
    pub const fn nanos(self) -> u32 {
        self.nanos
    }
    /// Eén nanoseconde later, voor strikt monotone committijden.
    pub fn next(self) -> Result<Self> {
        if self.nanos == 999_999_999 {
            Self::unix(self.seconds + 1, 0)
        } else {
            Self::unix(self.seconds, self.nanos + 1)
        }
    }
    /// Leest een datum met echte kalendercontrole en een geldige UTC-offset.
    pub fn parse(s: &str) -> Result<Self> {
        let b = s.as_bytes();
        if b.len() < 20
            || b.len() > 64
            || b[4] != b'-'
            || b[7] != b'-'
            || b[10] != b'T'
            || b[13] != b':'
            || b[16] != b':'
        {
            return Err(Error::Corrupt);
        }
        let num = |i: usize, n: usize| -> Result<i64> {
            let mut v = 0;
            for &c in b.get(i..i + n).ok_or(Error::Corrupt)? {
                if !c.is_ascii_digit() {
                    return Err(Error::Corrupt);
                }
                v = v * 10 + i64::from(c - b'0');
            }
            Ok(v)
        };
        let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
        let (h, mi, se) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
        let days = match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                    29
                } else {
                    28
                }
            }
            _ => 0,
        };
        if d < 1 || d > days || h > 23 || mi > 59 || se > 59 {
            return Err(Error::Corrupt);
        }
        let mut i = 19;
        let mut nanos = 0;
        if matches!(b.get(i), Some(b'.' | b',')) {
            i += 1;
            let start = i;
            while let Some(&c) = b.get(i).filter(|c| c.is_ascii_digit()) {
                if i - start < 9 {
                    nanos = nanos * 10 + u32::from(c - b'0');
                }
                i += 1;
            }
            if i == start {
                return Err(Error::Corrupt);
            }
            for _ in i - start..9 {
                nanos *= 10;
            }
        }
        let offset = match b.get(i) {
            Some(b'Z') if b.len() == i + 1 => 0,
            Some(sign @ (b'+' | b'-')) if b.len() == i + 6 && b[i + 3] == b':' => {
                let hours = num(i + 1, 2)?;
                let minutes = num(i + 4, 2)?;
                if hours > 23 || minutes > 59 {
                    return Err(Error::Corrupt);
                }
                (hours * 3600 + minutes * 60) * if *sign == b'+' { 1 } else { -1 }
            }
            _ => return Err(Error::Corrupt),
        };
        let year = y - i64::from(m <= 2);
        let era = year.div_euclid(400);
        let yo = year - era * 400;
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let days =
            era * 146097 + yo * 365 + yo / 4 - yo / 100 + (153 * mp + 2) / 5 + d - 1 - 719468;
        Self::unix(days * 86400 + h * 3600 + mi * 60 + se - offset, nanos)
    }
    /// Canonieke UTC-vorm zoals Go met `RFC3339Nano`; geen onnauwkeurige floats.
    pub fn encode(self) -> Result<String> {
        let z = self.seconds.div_euclid(86400) + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yo = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let doy = doe - (365 * yo + yo / 4 - yo / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yo + era * 400 + i64::from(m <= 2);
        let rem = self.seconds.rem_euclid(86400);
        let mut buf = *b"0000-00-00T00:00:00.000000000Z";
        for (start, len, value) in [
            (0, 4, y),
            (5, 2, m),
            (8, 2, d),
            (11, 2, rem / 3600),
            (14, 2, rem / 60 % 60),
            (17, 2, rem % 60),
            (20, 9, i64::from(self.nanos)),
        ] {
            let mut value = value;
            for p in buf[start..start + len].iter_mut().rev() {
                *p = b'0' + (value % 10) as u8;
                value /= 10;
            }
        }
        let mut end = 29;
        while end > 20 && buf[end - 1] == b'0' {
            end -= 1;
        }
        if end == 20 {
            end = 19;
        }
        buf[end] = b'Z';
        crate::string(core::str::from_utf8(&buf[..end + 1]).map_err(|_| Error::Corrupt)?)
    }
}
