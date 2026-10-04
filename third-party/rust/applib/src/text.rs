//! Tekst schrijven zonder te groeien: [`Room`], een `fmt::Write` in een
//! vooraf gereserveerde `Vec`, en [`json_str`], een string als JSON.
//!
//! Een tekst van vaste maat op de stack is `bounded::Text`; [`Room`] is voor
//! wat daar te groot voor is (een pagina, de staat van vitals).

use alloc::vec::Vec;
use core::fmt::{self, Write};

/// Een schrijver in een vooraf gereserveerde `Vec` die nooit groeit: past
/// een stuk niet meer binnen de capaciteit, dan komt er niets van bij en is
/// het een `fmt::Error`. Zo is een antwoord nooit een verborgen allocatie
/// die bij een volle heap de app afbreekt (handboek §6).
pub struct Room<'a>(pub &'a mut Vec<u8>);

impl Write for Room<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.0.len().saturating_add(s.len()) > self.0.capacity() {
            return Err(fmt::Error);
        }
        // Binnen de capaciteit: geen hertoewijzing.
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// `s` als JSON-string, met aanhalingstekens en de escapes die de spec
/// eist (RFC 8259 §7).
pub fn json_str(w: &mut impl Write, s: &str) -> fmt::Result {
    w.write_str("\"")?;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let esc = match c {
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\n' => Some("\\n"),
            '\r' => Some("\\r"),
            '\t' => Some("\\t"),
            _ => None,
        };
        if esc.is_some() || c < ' ' {
            w.write_str(s.get(start..i).unwrap_or_default())?;
            match esc {
                Some(e) => w.write_str(e)?,
                None => write!(w, "\\u{:04x}", u32::from(c))?,
            }
            start = i + c.len_utf8();
        }
    }
    w.write_str(s.get(start..).unwrap_or_default())?;
    w.write_str("\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    #[test]
    fn room_refuses_what_does_not_fit_and_never_grows() {
        let mut v = Vec::new();
        v.try_reserve_exact(4).unwrap();
        let cap = v.capacity();
        let mut w = Room(&mut v);
        assert!(w.write_str("abc").is_ok());
        assert!(w.write_str("de").is_err());
        assert!(w.write_str("d").is_ok());
        assert_eq!(v, b"abcd");
        assert_eq!(v.capacity(), cap);
    }

    #[test]
    fn json_strings_escape_quotes_and_control_characters() {
        let mut s = String::new();
        json_str(&mut s, "a\"b\\c\nd\u{1}\u{e9}\r\t").unwrap();
        assert_eq!(s, "\"a\\\"b\\\\c\\nd\\u0001\u{e9}\\r\\t\"");
    }
}
