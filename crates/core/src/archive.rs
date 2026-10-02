//! Metadata van de Docker-tarstroom. De host houdt payloads op schijf of in kleine blokken.
use crate::validation::{invalid, text};
use alloc::{string::String, vec::Vec};
use spin_domain::{self as d, TryClone, try_push_str, try_string};
/// Een fysiek tarblok heeft altijd 512 bytes.
pub const BLOCK: usize = 512;
/// POSIX pax en GNU longname mogen samen maximaal 1 MiB metadata per bestand dragen.
pub const METADATA_LIMIT: usize = 1 << 20;
/// De relevante velden van één logische tar-entry; tijden en eigenaar blijven in de ruwe kop.
#[derive(Debug)]
pub struct Header {
    /// Pad zoals de archiefmaker het heeft opgeslagen.
    pub name: String,
    /// Linkdoel voor een harde of symbolische link.
    pub link: String,
    /// POSIX typeflag; NUL wordt het equivalente gewone-bestandstype '0'.
    pub kind: u8,
    /// Payloadlengte, zonder padding.
    pub size: u64,
    /// Modebits uit de kop.
    pub mode: u64,
}
impl Header {
    /// Een gewone tar-entry voor de twee vaste deltavelden, inclusief GNU-groottes.
    pub fn regular(name: &str, size: u64) -> d::Fallible<[u8; BLOCK]> {
        if name.is_empty() || name.len() > 100 || name.contains('\0') {
            return Err(invalid("tar", "invalid regular entry name"));
        }
        let mut block = [0; BLOCK];
        block[..name.len()].copy_from_slice(name.as_bytes());
        block[100..108].copy_from_slice(b"0000600\0");
        block[108..116].copy_from_slice(b"0000000\0");
        block[116..124].copy_from_slice(b"0000000\0");
        if size < (1 << 33) {
            let value = text(format_args!("{size:011o}"))?;
            block[124..135].copy_from_slice(value.as_bytes());
        } else {
            block[124] = 0x80;
            block[128..136].copy_from_slice(&size.to_be_bytes());
        }
        block[136..148].copy_from_slice(b"00000000000\0");
        block[148..156].fill(b' ');
        block[156] = b'0';
        block[257..263].copy_from_slice(b"ustar\0");
        block[263..265].copy_from_slice(b"00");
        let sum: u64 = block.iter().map(|b| u64::from(*b)).sum();
        let checksum = text(format_args!("{sum:06o}\0 "))?;
        block[148..156].copy_from_slice(checksum.as_bytes());
        Ok(block)
    }
    /// Een nulblok is het einde; elke andere kop moet een kloppende checksum hebben.
    pub fn decode(block: &[u8; BLOCK]) -> d::Fallible<Option<Self>> {
        if block.iter().all(|b| *b == 0) {
            return Ok(None);
        }
        let stored = number(&block[148..156])?;
        let mut unsigned = 0_u64;
        let mut signed = 0_i64;
        for (index, byte) in block.iter().copied().enumerate() {
            let byte = if (148..156).contains(&index) {
                b' '
            } else {
                byte
            };
            unsigned += u64::from(byte);
            signed += i64::from(i8::from_ne_bytes([byte]));
        }
        if stored != unsigned && i64::try_from(stored).ok() != Some(signed) {
            return Err(invalid("tar", "header checksum mismatch"));
        }
        let mut name = field_text(&block[..100])?;
        // GNU's old header uses the prefix range for numeric extension fields.
        if &block[257..263] == b"ustar\0" {
            let prefix = field_text(&block[345..500])?;
            if !prefix.is_empty() {
                name = text(format_args!("{prefix}/{name}"))?;
            }
        }
        let kind = if block[156] == 0 {
            if name.ends_with('/') { b'5' } else { b'0' }
        } else {
            block[156]
        };
        if kind == b'S' {
            return Err(invalid(
                "tar",
                "GNU sparse archive requires explicit sparse reconstruction",
            ));
        }
        Ok(Some(Self {
            name,
            link: field_text(&block[157..257])?,
            kind,
            size: number(&block[124..136])?,
            mode: number(&block[100..108])?,
        }))
    }
    /// Afronden zonder overflow, ook voor gemanipuleerde groottevelden.
    pub fn padded_size(&self) -> d::Fallible<u64> {
        self.size
            .checked_add(511)
            .map(|n| n / 512 * 512)
            .ok_or_else(|| invalid("tar", "entry length overflows"))
    }
}
/// Zowel octaal als GNU base-256; negatieve lengtes zijn ongeldig.
pub fn number(bytes: &[u8]) -> d::Fallible<u64> {
    if let Some((&first, tail)) = bytes.split_first()
        && first & 0x80 != 0
    {
        if first & 0x40 != 0 {
            return Err(invalid("tar", "negative unsigned header field"));
        }
        let mut value = u64::from(first & 0x7f);
        for byte in tail {
            value = value
                .checked_mul(256)
                .and_then(|n| n.checked_add(u64::from(*byte)))
                .ok_or_else(|| invalid("tar", "base-256 field overflows"))?;
        }
        return Ok(value);
    }
    let start = bytes
        .iter()
        .position(|b| !matches!(b, b' ' | 0))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !matches!(b, b' ' | 0))
        .map_or(start, |n| n + 1);
    let mut value = 0_u64;
    for byte in &bytes[start..end] {
        if !(b'0'..=b'7').contains(byte) {
            return Err(invalid("tar", "invalid octal field"));
        }
        value = value
            .checked_mul(8)
            .and_then(|n| n.checked_add(u64::from(*byte - b'0')))
            .ok_or_else(|| invalid("tar", "octal field overflows"))?;
    }
    Ok(value)
}
fn field_text(bytes: &[u8]) -> d::Fallible<String> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    // Tar-paden blijven bytes in de host. Metadata die het JSON-contract bereikt
    // moet ondubbelzinnig UTF-8 zijn; ongeldige namen veroorzaken geen verkeerde kopie.
    try_string(
        core::str::from_utf8(&bytes[..end])
            .map_err(|_| invalid("tar", "archive path is not UTF-8"))?,
    )
}
/// Faalbaar gelezen GNU- en pax-velden die op één volgende kop slaan.
#[derive(Default)]
pub struct Extensions {
    name: Option<String>,
    link: Option<String>,
    size: Option<u64>,
}
impl Extensions {
    /// Verwerkt één metadata-entry, zonder de gewone bestandsinhoud te bufferen.
    pub fn read(&mut self, kind: u8, bytes: &[u8]) -> d::Fallible {
        if bytes.len() > METADATA_LIMIT {
            return Err(invalid("tar", "extension exceeds metadata budget"));
        }
        match kind {
            b'L' => {
                self.name = Some(field_text(bytes)?);
                Ok(())
            }
            b'K' => {
                self.link = Some(field_text(bytes)?);
                Ok(())
            }
            b'x' | b'g' => self.pax(bytes),
            _ => Err(invalid("tar", "unsupported tar extension")),
        }
    }
    fn pax(&mut self, mut bytes: &[u8]) -> d::Fallible {
        while !bytes.is_empty() {
            let at = bytes
                .iter()
                .position(|b| *b == b' ')
                .ok_or_else(|| invalid("pax", "missing record length"))?;
            let length = core::str::from_utf8(&bytes[..at])
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or_else(|| invalid("pax", "invalid record length"))?;
            if length <= at + 2 || length > bytes.len() || bytes[length - 1] != b'\n' {
                return Err(invalid("pax", "truncated pax record"));
            }
            let record = &bytes[at + 1..length - 1];
            let equals = record
                .iter()
                .position(|b| *b == b'=')
                .ok_or_else(|| invalid("pax", "missing record value"))?;
            let key = &record[..equals];
            let value = &record[equals + 1..];
            if value.contains(&0) {
                return Err(invalid("pax", "NUL in pax value"));
            }
            match key {
                b"path" => self.name = Some(field_text(value)?),
                b"linkpath" => self.link = Some(field_text(value)?),
                b"size" => {
                    self.size = Some(
                        core::str::from_utf8(value)
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .ok_or_else(|| invalid("pax", "invalid pax size"))?,
                    )
                }
                key if key.starts_with(b"GNU.sparse.") => {
                    return Err(invalid(
                        "pax",
                        "sparse archive requires explicit sparse reconstruction",
                    ));
                }
                _ => {} // Ownership, times and xattrs remain in the copied raw metadata.
            }
            bytes = &bytes[length..];
        }
        Ok(())
    }
    /// Consumeert de overrides, zodat ze nooit op het volgende bestand lekken.
    pub fn apply(&mut self, header: &mut Header) {
        if let Some(name) = self.name.take() {
            header.name = name;
        }
        if let Some(link) = self.link.take() {
            header.link = link;
        }
        if let Some(size) = self.size.take() {
            header.size = size;
        }
    }
}
/// Normalizeert een archiefpad; een parentstap mag de containerwortel niet verlaten.
pub fn clean_path(value: &str) -> d::Fallible<String> {
    if value.contains('\0') {
        return Err(invalid("tar", "NUL in archive path"));
    }
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(invalid("tar", "archive path escapes root"));
                }
            }
            part => d::try_push(&mut parts, part)?,
        }
    }
    let mut out = String::new();
    for part in parts {
        if !out.is_empty() {
            try_push_str(&mut out, "/")?;
        }
        try_push_str(&mut out, part)?;
    }
    Ok(out)
}
/// Kernelmounts en Docker's eigen netwerkbestanden mogen nooit door een imagecopy heen.
pub fn managed_path(value: &str) -> d::Fallible<bool> {
    let name = clean_path(value)?;
    Ok(matches!(
        name.as_str(),
        "proc" | "sys" | "dev" | "etc/hosts" | "etc/hostname" | "etc/resolv.conf" | ".dockerenv"
    ) || ["proc/", "sys/", "dev/"]
        .iter()
        .any(|prefix| name.starts_with(prefix)))
}
/// Een OCI-whiteout wist een pad of alleen de bestaande inhoud van een map.
#[derive(Debug, PartialEq)]
pub enum Whiteout {
    /// Verwijder dit absolute pad.
    File(String),
    /// Maak deze absolute map leeg, ook verborgen bestanden.
    Opaque(String),
}
/// Herkent whiteouts nadat het pad veilig genormaliseerd is.
pub fn whiteout(value: &str) -> d::Fallible<Option<Whiteout>> {
    let name = clean_path(value)?;
    if managed_path(&name)? {
        return Ok(None);
    }
    let (dir, base) = name.rsplit_once('/').unwrap_or(("", &name));
    if base == ".wh..wh..opq" {
        return Ok(Some(Whiteout::Opaque(text(format_args!("/{dir}"))?)));
    }
    if let Some(file) = base.strip_prefix(".wh.") {
        if matches!(file, "" | "." | "..") {
            return Err(invalid("whiteout", "invalid deletion target"));
        }
        return Ok(Some(Whiteout::File(if dir.is_empty() {
            text(format_args!("/{file}"))?
        } else {
            text(format_args!("/{dir}/{file}"))?
        })));
    }
    Ok(None)
}
/// Quote één shellwaarde; alleen de gegenereerde commandotekst krijgt shellsemantiek.
pub fn shell_quote(value: &str) -> d::Fallible<String> {
    let mut result = try_string("'")?;
    for (index, part) in value.split('\'').enumerate() {
        if index > 0 {
            try_push_str(&mut result, "'\\''")?;
        }
        try_push_str(&mut result, part)?;
    }
    try_push_str(&mut result, "'")?;
    Ok(result)
}
/// De weergave behoudt totalen, maar toont maximaal de 50.000 grootste bestanden.
pub fn summarize(entries: &[d::ContentEntry]) -> d::Fallible<d::LayerContents> {
    let mut sorted = Vec::new();
    let mut result = d::LayerContents::default();
    for entry in entries {
        result.files = result
            .files
            .checked_add(1)
            .ok_or_else(|| invalid("manifest", "file count overflows"))?;
        result.bytes = result
            .bytes
            .checked_add(entry.bytes)
            .ok_or_else(|| invalid("manifest", "byte count overflows"))?;
        d::try_push(&mut sorted, entry)?;
    }
    sorted.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes));
    for entry in sorted.into_iter().take(50_000) {
        result.entries.push(entry.try_clone()?)?;
    }
    Ok(result)
}
