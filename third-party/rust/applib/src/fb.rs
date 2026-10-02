//! Het glas en de invoer van de display-app: wat een app nodig heeft die
//! de framebuffer-grant van de kern houdt (docs/gui.md, "De display-app").
//!
//! Drie delen, en dit module bezit niets: de tabellen zijn van
//! [`crate::mmu`].
//!
//! - [`Glass`]: de `FB_*`-sleutels uit de env (`FB_BASE`, `FB_WIDTH`,
//!   `FB_HEIGHT`, `FB_STRIDE`, `FB_BPP`, `FB_SWAP`), getoetst voordat er
//!   één pixel geschreven wordt: een stride kleiner dan een rij of een
//!   diepte anders dan 16 of 32 is een weigering, geen scheve streep.
//! - [`map`]: het venster in de stage-1 van de app als **Normal-NC**, op
//!   4 KB. Met de MMU uit (een app zonder stage-1) was elke store naar het
//!   glas Device: een 1080p-frame een miljoen losse, geordende
//!   transacties. Normal-NC is wat Linux een framebuffer geeft (write-combine): geen cache, dus
//!   de scanout ziet elke store zonder onderhoud, maar het fabric mag
//!   gatheren (Go `cpu/memattr`, 04-08). `FB_BASE` staat niet op 2 MB
//!   (het IPA is `0x2000_0000` plus de offset in het blok), dus de randen
//!   gaan op 4 KB-pagina's.
//! - [`LineReader`] en [`Input`]: de stroom van `INPUT_ADDR`, één JSON-event
//!   per regel (`{"k":"key",..}`, `move`, `btn`, `wheel`), precies wat
//!   `POST /input` van de browser-KVM aanneemt; een lege regel is een
//!   keepalive. De lezer alloceert niet en een te lange regel valt weg in
//!   plaats van de volgende mee te nemen.
//!
//! Het glas komt in de ene stage-1 die elke app sinds 30-09 heeft
//! ([`crate::mmu`]): `_start` mapt RAM, tabellen en staart, en [`map`] zet
//! er het venster bij als Normal-NC, in dezelfde tabellen. Tot 30-09 legde
//! dit module een eigen tabel met SCTLR.C uit; zie [`crate::mmu`] waarom C
//! nu aan mag. De switcher bewaart het EL1-regime per bewoner (19
//! registers, `abi::layout::CTX_REGIME`), dus een yield naar Hop verliest
//! de map niet.

use crate::App;
use core::fmt;

/// De IPA-basis van het glas in de kooi (`kern::stage2::FB_IPA`): het
/// venster staat daar plus de offset in zijn 2 MB-blok.
pub const FB_IPA: u64 = 0x2000_0000;

/// De langste invoerregel die de lezer bewaart. De langste regel van de
/// kern (`gui_usbin::deliver::LINE_MAX`) is 96 bytes.
pub const LINE_CAP: usize = 128;

/// Waarom het glas of de invoer niet te gebruiken is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbError {
    /// Een verplichte sleutel ontbreekt: dit slot houdt het glas niet.
    Missing(&'static str),
    /// Een sleutel die geen getal (of adres) is.
    Bad(&'static str),
    /// Een diepte die niet getekend wordt (16 of 32).
    Bpp(u32),
    /// Een rij past niet in de stride.
    Stride {
        /// De breedte in pixels.
        width: u32,
        /// De stride in bytes.
        stride: u32,
        /// Bytes per pixel.
        bpx: u32,
    },
    /// Het venster valt buiten het glasvenster van de kooi of loopt om.
    Window {
        /// `FB_BASE`.
        base: u64,
        /// `FB_STRIDE * FB_HEIGHT`.
        size: u64,
    },
    /// Er is geen stage-1 om het glas in te zetten: een ander target, of
    /// `_start` weigerde hem ([`crate::mmu::MmuError`]).
    NoStage1(crate::mmu::MmuError),
    /// De tabellen passen niet in de 64 KB die de ABI ervoor openhoudt.
    Tables,
}

impl fmt::Display for FbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(k) => write!(f, "{k} missing from the env"),
            Self::Bad(k) => write!(f, "{k} is not a number or address"),
            Self::Bpp(b) => write!(f, "FB_BPP={b} is not drawable (16 or 32)"),
            Self::Stride { width, stride, bpx } => write!(
                f,
                "FB_STRIDE={stride} is smaller than FB_WIDTH={width} times {bpx} bytes"
            ),
            Self::Window { base, size } => {
                write!(f, "window {base:#x}+{size:#x} is outside the glass window")
            }
            Self::NoStage1(why) => write!(f, "{why}"),
            Self::Tables => write!(f, "stage-1 tables exceed the 64 KB under the image"),
        }
    }
}

/// Het glas zoals de grant het meegaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glass {
    /// Het IPA van de eerste pixel (`FB_BASE`).
    pub base: u64,
    /// De breedte in pixels.
    pub width: u32,
    /// De hoogte in pixels.
    pub height: u32,
    /// Bytes per rij.
    pub stride: u32,
    /// 32 (x8r8g8b8) of 16 (r5g6b5).
    pub bpp: u32,
    /// Rood en blauw ruilen (GOP-formaat RGB).
    pub swap: bool,
}

impl Glass {
    /// Leest en toetst de `FB_*`-sleutels van `env` (in de app:
    /// `|k| app.env(k)`).
    pub fn from_env<'a>(env: impl Fn(&str) -> Option<&'a str>) -> Result<Self, FbError> {
        let num = |k: &'static str| -> Result<u64, FbError> {
            let v = env(k).ok_or(FbError::Missing(k))?;
            parse_u64(v).ok_or(FbError::Bad(k))
        };
        let small = |k: &'static str| -> Result<u32, FbError> {
            u32::try_from(num(k)?).map_err(|_| FbError::Bad(k))
        };
        let g = Glass {
            base: num("FB_BASE")?,
            width: small("FB_WIDTH")?,
            height: small("FB_HEIGHT")?,
            stride: small("FB_STRIDE")?,
            bpp: small("FB_BPP")?,
            swap: env("FB_SWAP") == Some("1"),
        };
        g.check()?;
        Ok(g)
    }

    /// Bytes per pixel.
    #[must_use]
    pub const fn bpx(&self) -> u32 {
        self.bpp / 8
    }

    /// De maat van het venster: `stride * height`.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.stride as u64 * self.height as u64
    }

    fn check(&self) -> Result<(), FbError> {
        if self.bpp != 16 && self.bpp != 32 {
            return Err(FbError::Bpp(self.bpp));
        }
        if self.width == 0 || self.height == 0 {
            return Err(FbError::Bad("FB_WIDTH/FB_HEIGHT"));
        }
        let row = u64::from(self.width) * u64::from(self.bpx());
        if row > u64::from(self.stride) {
            return Err(FbError::Stride {
                width: self.width,
                stride: self.stride,
                bpx: self.bpx(),
            });
        }
        // Het glas ligt in het venster van de grant: onder de canonieke
        // app-basis, boven FB_IPA, en op een pixel.
        let end = self.base.checked_add(self.size());
        if self.base < FB_IPA
            || end.is_none_or(|e| e > abi::layout::SLOTS_BASE)
            || !self.base.is_multiple_of(u64::from(self.bpx()))
        {
            return Err(FbError::Window {
                base: self.base,
                size: self.size(),
            });
        }
        Ok(())
    }

    /// Het rauwe pixelwoord voor `rgb` (0x00RRGGBB) in het formaat van dit
    /// glas: geruild bij `FB_SWAP`, r5g6b5 bij 16 bpp. Dezelfde regels als
    /// de console van de kern (`driver_fb::Desc::encode`).
    #[must_use]
    pub const fn encode(&self, rgb: u32) -> u32 {
        let rgb = if self.swap {
            rgb & 0xFF00_FF00 | (rgb & 0xFF) << 16 | (rgb >> 16) & 0xFF
        } else {
            rgb
        };
        if self.bpp == 16 {
            let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
            return (r >> 3) << 11 | (g >> 2) << 5 | (b >> 3);
        }
        rgb
    }
}

/// Een getal als `123` of `0x1a2b`.
fn parse_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

/// `INPUT_ADDR` (`10.100.0.1:7879`) als IPv4 en poort. `None` zonder
/// sleutel: dit board heeft geen werkende USB, er valt niets te bellen.
pub fn input_addr<'a>(
    env: impl Fn(&str) -> Option<&'a str>,
) -> Option<Result<([u8; 4], u16), FbError>> {
    let v = env("INPUT_ADDR")?;
    let bad = FbError::Bad("INPUT_ADDR");
    let parsed = v.trim().rsplit_once(':').and_then(|(ip, port)| {
        let ip = crate::appnet::parse_ip4(ip)?;
        let port = port.parse::<u16>().ok().filter(|p| *p != 0)?;
        Some((ip, port))
    });
    Some(parsed.ok_or(bad))
}

/// Hoe het venster op het glas staat na [`map`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapped {
    /// Het venster, naar buiten afgerond op 4 KB.
    pub base: u64,
    /// De maat daarvan.
    pub size: u64,
    /// Hoeveel van de 64 KB aan tabellen de map gebruikt.
    pub table_bytes: u64,
}

/// Zet het glas als Normal-NC in de stage-1 van de app (zie de
/// moduledoc). Eén keer, vóór de eerste pixel.
///
/// [`FbError::NoStage1`] is geen reden om niet te tekenen: dan draait de
/// app zonder MMU en is het glas Device, trager maar correct.
/// [`FbError::Tables`] wel: dan staat de MMU aan en is het glas voor stage
/// 1 niet gemapt, dus de eerste pixel is een vertaalfout.
pub fn map(_app: &App, g: &Glass) -> Result<Mapped, FbError> {
    let (lo, hi) = g.pages();
    let table_bytes = crate::mmu::map_glass(lo, hi).map_err(|e| match e {
        crate::mmu::MmuError::Tables => FbError::Tables,
        e => FbError::NoStage1(e),
    })?;
    Ok(Mapped {
        base: lo,
        size: hi - lo,
        table_bytes,
    })
}

/// Een pagina van de stage-1.
const PAGE: u64 = crate::mmu::PAGE;

impl Glass {
    /// Het venster, naar buiten afgerond op 4 KB: `[lo, hi)`.
    #[must_use]
    pub const fn pages(&self) -> (u64, u64) {
        let lo = self.base & !(PAGE - 1);
        let hi = self
            .base
            .saturating_add(self.size())
            .saturating_add(PAGE - 1)
            & !(PAGE - 1);
        (lo, hi)
    }
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

/// Knipt een bytestroom in regels, zonder allocatie. De verbinding leest
/// in [`LineReader::spare`], meldt met [`LineReader::commit`] hoeveel er
/// kwam, en haalt de regels op met [`LineReader::pop`].
///
/// # Invariants
///
/// `len <= LINE_CAP`; `skip` betekent dat de bytes tot de volgende newline
/// bij een te lange regel horen en wegvallen.
pub struct LineReader {
    buf: [u8; LINE_CAP],
    len: usize,
    skip: bool,
    /// Regels die te lang waren en wegvielen.
    pub overlong: u64,
}

impl Default for LineReader {
    fn default() -> Self {
        Self::new()
    }
}

impl LineReader {
    /// Een lege lezer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; LINE_CAP],
            len: 0,
            skip: false,
            overlong: 0,
        }
    }

    /// De vrije ruimte om in te lezen. Is die op (een regel zonder newline
    /// die de hele buffer vult), dan valt de regel weg en is de buffer weer
    /// leeg.
    pub fn spare(&mut self) -> &mut [u8] {
        if self.len == LINE_CAP {
            // INVARIANT: de regel is te lang; alles tot de volgende newline
            // valt weg.
            self.len = 0;
            self.skip = true;
            self.overlong = self.overlong.wrapping_add(1);
        }
        self.buf.get_mut(self.len..).unwrap_or_default()
    }

    /// `n` bytes zijn in [`LineReader::spare`] gelezen.
    pub fn commit(&mut self, n: usize) {
        self.len = self.len.saturating_add(n).min(LINE_CAP);
    }

    /// De volgende hele regel, zonder newline, gekopieerd naar `out`; geeft
    /// de lengte.
    pub fn pop(&mut self, out: &mut [u8; LINE_CAP]) -> Option<usize> {
        loop {
            let nl = self.buf.get(..self.len)?.iter().position(|&b| b == b'\n')?;
            let line = nl;
            if self.skip {
                self.skip = false;
            } else if let (Some(src), Some(dst)) = (self.buf.get(..line), out.get_mut(..line)) {
                dst.copy_from_slice(src);
                self.consume(nl + 1);
                return Some(line);
            }
            self.consume(nl + 1);
        }
    }

    /// Schuift `n` bytes uit de buffer.
    fn consume(&mut self, n: usize) {
        let n = n.min(self.len);
        self.buf.copy_within(n..self.len, 0);
        self.len -= n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&'static str, &'static str)]) -> HashMap<&'static str, &'static str> {
        pairs.iter().copied().collect()
    }

    const RAMFB: &[(&str, &str)] = &[
        ("FB_BASE", "0x20000000"),
        ("FB_WIDTH", "1280"),
        ("FB_HEIGHT", "800"),
        ("FB_STRIDE", "5120"),
        ("FB_BPP", "32"),
        ("INPUT_ADDR", "10.100.0.1:7879"),
    ];

    #[test]
    fn the_env_of_the_grant_is_the_glass() {
        let e = env(RAMFB);
        let g = Glass::from_env(|k| e.get(k).copied()).unwrap();
        assert_eq!(
            g,
            Glass {
                base: 0x2000_0000,
                width: 1280,
                height: 800,
                stride: 5120,
                bpp: 32,
                swap: false
            }
        );
        assert_eq!(g.size(), 5120 * 800);
        assert_eq!(
            input_addr(|k| e.get(k).copied()),
            Some(Ok(([10, 100, 0, 1], 7879)))
        );
        assert_eq!(input_addr(|_| None), None);
        assert_eq!(
            input_addr(|_| Some("10.100.0.1")),
            Some(Err(FbError::Bad("INPUT_ADDR")))
        );
    }

    #[test]
    fn a_bad_glass_is_refused_before_a_pixel() {
        let mut e = env(RAMFB);
        e.insert("FB_STRIDE", "4000");
        assert!(matches!(
            Glass::from_env(|k| e.get(k).copied()),
            Err(FbError::Stride { .. })
        ));
        let mut e = env(RAMFB);
        e.insert("FB_BPP", "24");
        assert_eq!(
            Glass::from_env(|k| e.get(k).copied()),
            Err(FbError::Bpp(24))
        );
        let mut e = env(RAMFB);
        e.remove("FB_BASE");
        assert_eq!(
            Glass::from_env(|k| e.get(k).copied()),
            Err(FbError::Missing("FB_BASE"))
        );
        let mut e = env(RAMFB);
        e.insert("FB_BASE", "0x50000000");
        assert!(matches!(
            Glass::from_env(|k| e.get(k).copied()),
            Err(FbError::Window { .. })
        ));
    }

    #[test]
    fn encode_follows_the_console() {
        let mut g = Glass {
            base: FB_IPA,
            width: 1,
            height: 1,
            stride: 4,
            bpp: 32,
            swap: false,
        };
        assert_eq!(g.encode(0x0011_2233), 0x0011_2233);
        g.swap = true;
        assert_eq!(g.encode(0x0011_2233), 0x0033_2211);
        g.swap = false;
        g.bpp = 16;
        assert_eq!(g.encode(0x00FF_0000), 0xF800);
    }

    #[test]
    fn the_glass_is_rounded_out_to_pages() {
        let e = env(RAMFB);
        let mut g = Glass::from_env(|k| e.get(k).copied()).unwrap();
        assert_eq!(g.pages(), (0x2000_0000, 0x2000_0000 + 0x3e_8000));
        g.base += 4;
        assert_eq!(g.pages(), (0x2000_0000, 0x2000_0000 + 0x3e_9000));
    }

    #[test]
    fn the_four_shapes_of_the_kvm() {
        let p = |s: &str| Input::parse(s.as_bytes());
        assert_eq!(
            p(r#"{"k":"key","c":65,"v":1}"#),
            Some(Input::Key {
                code: 65,
                down: true
            })
        );
        assert_eq!(
            p(r#"{"k":"move","x":640,"y":400}"#),
            Some(Input::Move { x: 640, y: 400 })
        );
        assert_eq!(
            p(r#"{"k":"btn","c":2,"v":0,"x":1,"y":2}"#),
            Some(Input::Button {
                code: 2,
                down: false,
                x: 1,
                y: 2
            })
        );
        assert_eq!(
            p(r#"{"k":"wheel","c":0,"v":-3,"x":5,"y":6}"#),
            Some(Input::Wheel { v: -3, x: 5, y: 6 })
        );
        assert_eq!(p(""), Some(Input::Keepalive));
        assert_eq!(p(r#"{"k":"paste"}"#), None);
        assert_eq!(p("not json"), None);
    }

    #[test]
    fn lines_are_cut_and_overlong_ones_dropped() {
        let mut r = LineReader::new();
        let mut out = [0u8; LINE_CAP];
        let feed = |r: &mut LineReader, b: &[u8]| {
            let s = r.spare();
            s[..b.len()].copy_from_slice(b);
            r.commit(b.len());
        };
        feed(&mut r, b"{\"k\":\"key\",\"c\":65,\"v\":1}\n\n{\"k\":\"mo");
        let n = r.pop(&mut out).unwrap();
        assert_eq!(&out[..n], b"{\"k\":\"key\",\"c\":65,\"v\":1}");
        assert_eq!(r.pop(&mut out), Some(0));
        assert_eq!(r.pop(&mut out), None);
        feed(&mut r, b"ve\",\"x\":1,\"y\":2}\n");
        let n = r.pop(&mut out).unwrap();
        assert_eq!(Input::parse(&out[..n]), Some(Input::Move { x: 1, y: 2 }));
        // Een regel langer dan de buffer valt weg, de volgende niet.
        let long = [b'x'; LINE_CAP];
        feed(&mut r, &long);
        assert_eq!(r.pop(&mut out), None);
        feed(&mut r, b"xxx\n{\"k\":\"key\",\"c\":1,\"v\":0}\n");
        let n = r.pop(&mut out).unwrap();
        assert_eq!(&out[..n], b"{\"k\":\"key\",\"c\":1,\"v\":0}");
        assert_eq!(r.overlong, 1);
    }
}
