//! ZIP-bundels met vaste grenzen, veilige paden en gecontroleerde CRCs.
use alloc::{string::String, vec::Vec};
use spin_domain::{try_push, try_string};
/// Hetzelfde bestand- en bytebudget als de Go-bundler.
pub const MAX_BYTES: usize = 25 << 20;
/// Hoogstens tweeduizend bestanden per bundel.
pub const MAX_FILES: usize = 2000;
/// Ongeldige archives en allocatiefouten verlaten de operatie vóór extractie.
#[derive(Debug)]
pub enum Error {
    /// Beschadigd of niet ondersteund ZIP-bestand.
    Invalid,
    /// Byte- of bestandsbudget bereikt.
    Limit,
    /// Geheugen niet beschikbaar.
    Allocation,
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "bundle: {self:?}")
    }
}
impl core::error::Error for Error {}
type Result<T> = core::result::Result<T, Error>;
fn bytes(data: &[u8], at: usize, n: usize) -> Result<&[u8]> {
    data.get(at..at.checked_add(n).ok_or(Error::Invalid)?)
        .ok_or(Error::Invalid)
}
fn u16_at(data: &[u8], at: usize) -> Result<usize> {
    let b = bytes(data, at, 2)?;
    Ok(usize::from(u16::from_le_bytes([b[0], b[1]])))
}
fn u32_at(data: &[u8], at: usize) -> Result<usize> {
    let b = bytes(data, at, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
}
/// ZIP gebruikt de gereflecteerde IEEE CRC-32.
pub fn crc(data: &[u8]) -> u32 {
    let mut c = !0_u32;
    for b in data {
        c ^= u32::from(*b);
        for _ in 0..8 {
            c = (c >> 1) ^ (0xedb88320 & (0_u32.wrapping_sub(c & 1)));
        }
    }
    !c
}
/// Geen absolute paden, traversals, Windows-paden of lege componenten.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 4096
        && !name.contains(['\\', ':'])
        && !name.chars().any(char::is_control)
        && name.split('/').all(|p| !matches!(p, "" | "." | ".."))
}
/// Een gecontroleerde centrale ZIP-entry; de bytes blijven bij de archive-eigenaar.
pub struct Entry<'a> {
    /// Relatief UTF-8-pad.
    pub name: &'a str,
    data: &'a [u8],
    size: usize,
    method: usize,
    crc: u32,
}
impl Entry<'_> {
    /// Pakt één entry uit, met opgegeven lengte en CRC als onafhankelijke controles.
    pub fn read(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.try_reserve_exact(self.size)
            .map_err(|_| Error::Allocation)?;
        out.resize(self.size, 0);
        match self.method {
            0 => {
                if self.data.len() != self.size {
                    return Err(Error::Invalid);
                }
                out.copy_from_slice(self.data);
            }
            8 => {
                let n = miniz_oxide::inflate::decompress_slice_iter_to_slice(
                    &mut out,
                    core::iter::once(self.data),
                    false,
                    true,
                )
                .map_err(|_| Error::Invalid)?;
                if n != self.size {
                    return Err(Error::Invalid);
                }
            }
            _ => return Err(Error::Invalid),
        }
        if crc(&out) != self.crc {
            return Err(Error::Invalid);
        }
        Ok(out)
    }
}
/// Controleert de complete directory, inclusief duplicaten en het uitgepakte budget.
pub fn entries(data: &[u8]) -> Result<Vec<Entry<'_>>> {
    if data.len() > MAX_BYTES {
        return Err(Error::Limit);
    }
    let end = (data.len().saturating_sub(65557)..data.len().saturating_sub(21))
        .rev()
        .find(|&at| {
            data.get(at..at + 4) == Some(b"PK\x05\x06")
                && u16_at(data, at + 20).is_ok_and(|n| at + 22 + n == data.len())
        })
        .ok_or(Error::Invalid)?;
    if u16_at(data, end + 4)? != 0 || u16_at(data, end + 6)? != 0 {
        return Err(Error::Invalid);
    }
    let count = u16_at(data, end + 10)?;
    if count != u16_at(data, end + 8)? || count > MAX_FILES {
        return Err(Error::Limit);
    }
    let mut at = u32_at(data, end + 16)?;
    if at.checked_add(u32_at(data, end + 12)?) != Some(end) {
        return Err(Error::Invalid);
    }
    let central = at;
    let mut total = 0_usize;
    let mut result: Vec<Entry<'_>> = Vec::new();
    for _ in 0..count {
        if bytes(data, at, 4)? != b"PK\x01\x02" {
            return Err(Error::Invalid);
        }
        let flags = u16_at(data, at + 8)?;
        let method = u16_at(data, at + 10)?;
        if flags & !(8 | 2048 | 6) != 0 || !matches!(method, 0 | 8) || u16_at(data, at + 34)? != 0 {
            return Err(Error::Invalid);
        }
        let crc = u32_at(data, at + 16)? as u32;
        let compressed = u32_at(data, at + 20)?;
        let size = u32_at(data, at + 24)?;
        total = total.checked_add(size).ok_or(Error::Limit)?;
        if total > MAX_BYTES {
            return Err(Error::Limit);
        }
        let name_size = u16_at(data, at + 28)?;
        let local = u32_at(data, at + 42)?;
        let name =
            core::str::from_utf8(bytes(data, at + 46, name_size)?).map_err(|_| Error::Invalid)?;
        let mode = u32_at(data, at + 38)? >> 16;
        if mode & 0o170000 == 0o120000 {
            return Err(Error::Invalid);
        }
        if !safe_name(name.trim_end_matches('/')) {
            return Err(Error::Invalid);
        }
        if bytes(data, local, 4)? != b"PK\x03\x04"
            || u16_at(data, local + 6)? != flags
            || u16_at(data, local + 8)? != method
            || u16_at(data, local + 26)? != name_size
            || bytes(data, local + 30, name_size)? != name.as_bytes()
        {
            return Err(Error::Invalid);
        }
        let start = local
            .checked_add(30 + name_size + u16_at(data, local + 28)?)
            .ok_or(Error::Invalid)?;
        if start.checked_add(compressed).is_none_or(|n| n > central) {
            return Err(Error::Invalid);
        }
        if !name.ends_with('/') {
            if result.iter().any(|e| {
                e.name == name
                    || e.name
                        .strip_prefix(name)
                        .is_some_and(|s| s.starts_with('/'))
                    || name
                        .strip_prefix(e.name)
                        .is_some_and(|s| s.starts_with('/'))
            }) {
                return Err(Error::Invalid);
            }
            try_push(
                &mut result,
                Entry {
                    name,
                    data: bytes(data, start, compressed)?,
                    size,
                    method,
                    crc,
                },
            )
            .map_err(|_| Error::Allocation)?;
        }
        at = at
            .checked_add(46 + name_size + u16_at(data, at + 30)? + u16_at(data, at + 32)?)
            .ok_or(Error::Invalid)?;
    }
    if at != end || result.is_empty() {
        return Err(Error::Invalid);
    }
    Ok(result)
}
struct Written {
    name: String,
    size: u32,
    crc: u32,
    offset: u32,
}
/// Een portable ZIP-writer: stored entries vereisen geen hostcompressor.
#[derive(Default)]
pub struct Writer {
    bytes: Vec<u8>,
    files: Vec<Written>,
}
fn append(out: &mut Vec<u8>, data: &[u8]) -> Result<()> {
    if data.len() > MAX_BYTES.saturating_sub(out.len()) {
        return Err(Error::Limit);
    }
    out.try_reserve(data.len()).map_err(|_| Error::Allocation)?;
    out.extend_from_slice(data);
    Ok(())
}
fn word(out: &mut Vec<u8>, n: u16) -> Result<()> {
    append(out, &n.to_le_bytes())
}
fn long(out: &mut Vec<u8>, n: u32) -> Result<()> {
    append(out, &n.to_le_bytes())
}
impl Writer {
    /// Voegt één regulier bestand toe; de header bevat al grootte en CRC.
    pub fn add(&mut self, name: &str, data: &[u8]) -> Result<()> {
        if !safe_name(name) || self.files.iter().any(|f| f.name == name) {
            return Err(Error::Invalid);
        }
        if self.files.len() >= MAX_FILES || data.len() > MAX_BYTES {
            return Err(Error::Limit);
        }
        let record = Written {
            name: try_string(name).map_err(|_| Error::Allocation)?,
            size: data.len() as u32,
            crc: crc(data),
            offset: self.bytes.len() as u32,
        };
        self.files.try_reserve(1).map_err(|_| Error::Allocation)?;
        append(&mut self.bytes, b"PK\x03\x04")?;
        for n in [20, 2048, 0, 0, 33] {
            word(&mut self.bytes, n)?;
        }
        for n in [record.crc, record.size, record.size] {
            long(&mut self.bytes, n)?;
        }
        word(&mut self.bytes, name.len() as u16)?;
        word(&mut self.bytes, 0)?;
        append(&mut self.bytes, name.as_bytes())?;
        append(&mut self.bytes, data)?;
        self.files.push(record);
        Ok(())
    }
    /// Schrijft de centrale directory en beëindigt de archive-eigenaar.
    pub fn finish(mut self) -> Result<Vec<u8>> {
        if self.files.is_empty() {
            return Err(Error::Invalid);
        }
        let start = self.bytes.len() as u32;
        for file in &self.files {
            append(&mut self.bytes, b"PK\x01\x02")?;
            for n in [20, 20, 2048, 0, 0, 33] {
                word(&mut self.bytes, n)?;
            }
            for n in [file.crc, file.size, file.size] {
                long(&mut self.bytes, n)?;
            }
            for n in [file.name.len() as u16, 0, 0, 0, 0] {
                word(&mut self.bytes, n)?;
            }
            long(&mut self.bytes, 0)?;
            long(&mut self.bytes, file.offset)?;
            append(&mut self.bytes, file.name.as_bytes())?;
        }
        let size = self.bytes.len() as u32 - start;
        append(&mut self.bytes, b"PK\x05\x06")?;
        for n in [0, 0, self.files.len() as u16, self.files.len() as u16] {
            word(&mut self.bytes, n)?;
        }
        long(&mut self.bytes, size)?;
        long(&mut self.bytes, start)?;
        word(&mut self.bytes, 0)?;
        Ok(self.bytes)
    }
}
/// Content-Type voor previews en downloads, onafhankelijk van het hostplatform.
pub fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" | "csv" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zip_checks_crc_paths_and_truncation() {
        let mut writer = Writer::default();
        writer.add("index.html", b"hello").unwrap();
        writer.add("assets/a.css", b"body{}").unwrap();
        let mut zip = writer.finish().unwrap();
        let parsed = entries(&zip).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].read().unwrap(), b"hello");
        for end in 0..zip.len() {
            assert!(entries(&zip[..end]).is_err());
        }
        zip[40] ^= 1;
        assert!(entries(&zip).unwrap()[0].read().is_err());
        for path in ["../a", "/root", "a/../b", "a\\b", "a//b", "C:foo"] {
            assert!(Writer::default().add(path, b"bad").is_err());
        }
    }
}
