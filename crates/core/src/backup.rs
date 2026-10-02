//! Een streaming ZIP64-backup: vaste namen, stored bytes en hoogstens 64 KiB per beurt.
use alloc::{string::String, vec::Vec};
use spin_domain::{Error, Fallible, try_string};
mod reader;
pub use reader::{Archive, Decoder, Entry, Source};
const DB: &[u8] = b"spin.db";
const KEY: &[u8] = b"master-key.txt";
/// Bestaande uploadgrens van Spin; ZIP64 voorkomt afkappen boven vier GiB.
pub const MAX_DATABASE: u64 = 64 << 30;
/// Eén uitvoerblok houdt de server-eigenaar maar kort bezig.
pub const CHUNK: usize = 64 << 10;
fn invalid() -> Error {
    crate::validation::invalid("backup", "invalid backup stream")
}
fn crc(mut value: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        value ^= u32::from(byte);
        for _ in 0..8 {
            value = (value >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(value & 1));
        }
    }
    value
}
fn u16(out: &mut Vec<u8>, n: u16) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn local(out: &mut Vec<u8>, name: &[u8], size: u64, checksum: u32, descriptor: bool) {
    u32(out, 0x04034b50);
    u16(out, 45);
    u16(out, if descriptor { 8 } else { 0 });
    u16(out, 0);
    u16(out, 0);
    u16(out, 0x21);
    u32(out, checksum);
    u32(out, u32::MAX);
    u32(out, u32::MAX);
    u16(out, name.len() as u16);
    u16(out, 20);
    out.extend_from_slice(name);
    u16(out, 1);
    u16(out, 16);
    u64(out, size);
    u64(out, size);
}
fn central(
    out: &mut Vec<u8>,
    name: &[u8],
    size: u64,
    checksum: u32,
    offset: u64,
    descriptor: bool,
) {
    u32(out, 0x02014b50);
    u16(out, 45);
    u16(out, 45);
    u16(out, if descriptor { 8 } else { 0 });
    u16(out, 0);
    u16(out, 0);
    u16(out, 0x21);
    u32(out, checksum);
    u32(out, u32::MAX);
    u32(out, u32::MAX);
    u16(out, name.len() as u16);
    u16(out, 28);
    u16(out, 0);
    u16(out, 0);
    u16(out, 0);
    u32(out, 0);
    u32(out, u32::MAX);
    out.extend_from_slice(name);
    u16(out, 1);
    u16(out, 24);
    u64(out, size);
    u64(out, size);
    u64(out, offset);
}
/// De eigenaar levert opeenvolgende databaseblokken uit een bevroren, gesynchroniseerd bestand.
pub struct Zip {
    size: u64,
    key: String,
    offset: u64,
    checksum: u32,
    phase: u8,
}
impl Zip {
    /// Valideert de grenzen voordat writes worden gepauzeerd.
    pub fn new(size: u64, key: &str) -> Fallible<Self> {
        if size == 0 || size > MAX_DATABASE || key.is_empty() || key.len() > 4096 {
            return Err(invalid());
        }
        Ok(Self {
            size,
            key: try_string(key)?,
            offset: 0,
            checksum: u32::MAX,
            phase: 0,
        })
    }
    fn key_offset(&self) -> u64 {
        30 + DB.len() as u64 + 20 + self.size + 24
    }
    fn central_offset(&self) -> u64 {
        self.key_offset() + 30 + KEY.len() as u64 + 20 + self.key.len() as u64
    }
    fn central_size(&self) -> u64 {
        (2 * (46 + 28) + DB.len() + KEY.len()) as u64
    }
    /// Exacte Content-Length, ook voor een database groter dan vier GiB.
    pub fn size(&self) -> u64 {
        self.central_offset() + self.central_size() + 56 + 20 + 22
    }
    /// Het volgende databereik, wanneer de volgende beurt een bestandsblok nodig heeft.
    pub fn read_range(&self) -> Option<(u64, usize)> {
        (self.phase == 1 && self.offset < self.size).then(|| {
            (
                self.offset,
                (self.size - self.offset).min(CHUNK as u64) as usize,
            )
        })
    }
    /// All database bytes and the directory are now owned by the transport.
    pub fn finished(&self) -> bool {
        self.phase == 3
    }
    /// Geeft één ZIP-fragment; fouten wijzigen de positie niet.
    pub fn next(&mut self, bytes: Option<Vec<u8>>) -> Fallible<Option<Vec<u8>>> {
        if let Some((_, count)) = self.read_range() {
            let out = bytes.ok_or_else(invalid)?;
            if out.len() != count {
                return Err(invalid());
            }
            self.checksum = crc(self.checksum, &out);
            self.offset += count as u64;
            return Ok(Some(out));
        }
        if bytes.is_some() {
            return Err(invalid());
        }
        if self.phase == 3 {
            return Ok(None);
        }
        let mut out = Vec::new();
        out.try_reserve_exact(1024 + self.key.len())
            .map_err(|_| Error::OutOfMemory)?;
        if self.phase == 0 {
            local(&mut out, DB, self.size, 0, true);
            self.phase = 1;
        } else {
            let checksum = !self.checksum;
            let key_crc = !crc(u32::MAX, self.key.as_bytes());
            u32(&mut out, 0x08074b50);
            u32(&mut out, checksum);
            u64(&mut out, self.size);
            u64(&mut out, self.size);
            local(&mut out, KEY, self.key.len() as u64, key_crc, false);
            out.extend_from_slice(self.key.as_bytes());
            central(&mut out, DB, self.size, checksum, 0, true);
            central(
                &mut out,
                KEY,
                self.key.len() as u64,
                key_crc,
                self.key_offset(),
                false,
            );
            let directory = self.central_offset();
            let directory_size = self.central_size();
            u32(&mut out, 0x06064b50);
            u64(&mut out, 44);
            u16(&mut out, 45);
            u16(&mut out, 45);
            u32(&mut out, 0);
            u32(&mut out, 0);
            u64(&mut out, 2);
            u64(&mut out, 2);
            u64(&mut out, directory_size);
            u64(&mut out, directory);
            u32(&mut out, 0x07064b50);
            u32(&mut out, 0);
            u64(&mut out, directory + directory_size);
            u32(&mut out, 1);
            u32(&mut out, 0x06054b50);
            u16(&mut out, 0);
            u16(&mut out, 0);
            u16(&mut out, 2);
            u16(&mut out, 2);
            u32(&mut out, directory_size as u32);
            u32(&mut out, u32::try_from(directory).unwrap_or(u32::MAX));
            u16(&mut out, 0);
            self.phase = 3;
        }
        Ok(Some(out))
    }
}
