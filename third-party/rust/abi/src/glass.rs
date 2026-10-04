//! Het glas en de invoer: het contract tussen de framebuffer-grant van de
//! kern en de app die hem houdt (docs/gui.md, "De display-app").
//!
//! - De grant zet de `FB_*`-sleutels in de env van de houder
//!   (`gui-fbgrant`), de app leest en toetst ze (`applib::fb::Glass`). Het
//!   venster staat op [`crate::layout::FB_IPA`] plus de offset in zijn
//!   2 MB-blok.
//! - Een pixel tekent elke kant met dezelfde regel ([`encode`]): de console
//!   van de kern (`driver_fb::Desc`) en de app.
//! - Het glas, het toetsenbord en de muis zijn één zitplaats: wie het glas
//!   houdt, krijgt [`INPUT_ADDR`] erbij, het gateway-adres op
//!   [`INPUT_PORT`]. Daar serveert de kern de USB-invoer (`gui-usbin`) als
//!   één JSON-object per regel, precies wat `POST /input` van de
//!   browser-KVM aanneemt ([`Input`]); een lege regel is een keepalive.

use core::fmt;

/// Het IPA van de eerste pixel, hex (`0x...`).
pub const FB_BASE: &str = "FB_BASE";
/// De breedte in pixels.
pub const FB_WIDTH: &str = "FB_WIDTH";
/// De hoogte in pixels.
pub const FB_HEIGHT: &str = "FB_HEIGHT";
/// Bytes per rij.
pub const FB_STRIDE: &str = "FB_STRIDE";
/// Bits per pixel: 32 (x8r8g8b8) of 16 (r5g6b5).
pub const FB_BPP: &str = "FB_BPP";
/// `1`: rood en blauw geruild (GOP-formaat RGB); anders afwezig.
pub const FB_SWAP: &str = "FB_SWAP";
/// Het adres van de invoerstroom, `10.100.0.1:7879`. Afwezig: dit board
/// heeft geen werkende USB.
pub const INPUT_ADDR: &str = "INPUT_ADDR";

/// De poort van de invoerstroom op het gateway-adres. Naast 7878 (SURF),
/// omdat het de andere helft van hetzelfde kanaal is; het nummer reist mee
/// in [`INPUT_ADDR`].
pub const INPUT_PORT: u16 = 7879;

/// De langste regel die de kern schrijft:
/// `{"k":"btn","c":..,"v":1,"x":..,"y":..}` met drie volle i32's past ruim.
pub const LINE_MAX: usize = 96;

/// Het pixelwoord voor `rgb` (0x00RRGGBB; een alfabyte gaat mee) in het
/// formaat van het glas: rood en blauw geruild bij `swap` ([`FB_SWAP`]),
/// r5g6b5 bij 16 bpp.
#[must_use]
pub const fn encode(rgb: u32, bpp: u32, swap: bool) -> u32 {
    let rgb = if swap {
        rgb & 0xFF00_FF00 | (rgb & 0xFF) << 16 | (rgb >> 16) & 0xFF
    } else {
        rgb
    };
    if bpp == 16 {
        let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
        return (r >> 3) << 11 | (g >> 2) << 5 | (b >> 3);
    }
    rgb
}

/// Eén invoergebeurtenis van de stroom, in de taal van de browser-KVM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// Een toets: de code van de KVM (een JavaScript-keycode) en neer/op.
    Key {
        /// De code.
        code: i32,
        /// Ingedrukt.
        down: bool,
    },
    /// De cursor staat op `(x, y)` (absoluut, de kern klemt hem op het
    /// scherm).
    Move {
        /// Horizontaal.
        x: i32,
        /// Verticaal.
        y: i32,
    },
    /// Een muisknop op `(x, y)`.
    Button {
        /// 0 links, 1 midden, 2 rechts.
        code: i32,
        /// Ingedrukt.
        down: bool,
        /// Horizontaal.
        x: i32,
        /// Verticaal.
        y: i32,
    },
    /// Het wiel: `v` klikken.
    Wheel {
        /// Klikken, negatief is naar boven.
        v: i32,
        /// Horizontaal.
        x: i32,
        /// Verticaal.
        y: i32,
    },
    /// Een lege regel: de stroom leeft.
    Keepalive,
}

impl Input {
    /// Ontleedt één regel (zonder de newline). `None` voor wat geen van de
    /// vier vormen is: een regel die de app niet begrijpt, slaat hij over.
    #[must_use]
    pub fn parse(line: &[u8]) -> Option<Input> {
        let s = core::str::from_utf8(line).ok()?.trim();
        if s.is_empty() {
            return Some(Input::Keepalive);
        }
        let n = |k: &str| field_num(s, k);
        let (x, y) = (n("x").unwrap_or(0), n("y").unwrap_or(0));
        match field_str(s, "k")? {
            "key" => Some(Input::Key {
                code: n("c")?,
                down: n("v")? != 0,
            }),
            "move" => Some(Input::Move {
                x: n("x")?,
                y: n("y")?,
            }),
            "btn" => Some(Input::Button {
                code: n("c")?,
                down: n("v")? != 0,
                x,
                y,
            }),
            "wheel" => Some(Input::Wheel { v: n("v")?, x, y }),
            _ => None,
        }
    }
}

/// De regel zonder newline, zoals de kern hem schrijft (Go `body`, met de
/// hand: vier velden, en dit pad loopt per toetsaanslag).
impl fmt::Display for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Key { code, down } => {
                write!(f, r#"{{"k":"key","c":{code},"v":{}}}"#, u8::from(down))
            }
            Self::Move { x, y } => write!(f, r#"{{"k":"move","x":{x},"y":{y}}}"#),
            Self::Button { code, down, x, y } => write!(
                f,
                r#"{{"k":"btn","c":{code},"v":{},"x":{x},"y":{y}}}"#,
                u8::from(down)
            ),
            Self::Wheel { v, x, y } => {
                write!(f, r#"{{"k":"wheel","c":0,"v":{v},"x":{x},"y":{y}}}"#)
            }
            Self::Keepalive => Ok(()),
        }
    }
}

/// De waarde van `"k":"..."` in een plat JSON-object.
fn field_str<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let rest = after_key(s, key)?;
    let rest = rest.strip_prefix('"')?;
    rest.split_once('"').map(|(v, _)| v)
}

/// De waarde van `"k":123` in een plat JSON-object.
fn field_num(s: &str, key: &str) -> Option<i32> {
    let rest = after_key(s, key)?;
    let end = rest
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && c == '-')))
        .map_or(rest.len(), |(i, _)| i);
    rest.get(..end)?.parse().ok()
}

/// Wat er na `"key":` komt (spaties overgeslagen).
fn after_key<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = s;
    loop {
        let at = rest.find('"')?;
        rest = rest.get(at + 1..)?;
        let (name, tail) = rest.split_once('"')?;
        let tail = tail.trim_start();
        if name == key
            && let Some(v) = tail.strip_prefix(':')
        {
            return Some(v.trim_start());
        }
        rest = tail;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_swaps_and_packs() {
        assert_eq!(encode(0x0011_2233, 32, false), 0x0011_2233);
        assert_eq!(encode(0x0011_2233, 32, true), 0x0033_2211);
        assert_eq!(encode(0x00FF_0000, 16, false), 0xF800);
    }

    /// Wat de kern schrijft, leest de app terug: de vier vormen en de
    /// keepalive, en de langste regel past in [`LINE_MAX`].
    #[test]
    fn what_the_kern_writes_the_app_parses() {
        let all = [
            Input::Key {
                code: 65,
                down: true,
            },
            Input::Move { x: 640, y: 400 },
            Input::Button {
                code: 2,
                down: false,
                x: 1,
                y: 2,
            },
            Input::Wheel { v: -3, x: 5, y: 6 },
            Input::Keepalive,
        ];
        for i in all {
            assert_eq!(Input::parse(i.to_string().as_bytes()), Some(i));
        }
        let big = Input::Button {
            code: i32::MIN,
            down: true,
            x: i32::MIN,
            y: i32::MIN,
        };
        assert!(big.to_string().len() < LINE_MAX);
        assert_eq!(Input::parse(br#"{"k":"paste"}"#), None);
        assert_eq!(Input::parse(b"not json"), None);
    }
}
