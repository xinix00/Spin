//! Bounded ZIP/ZIP64 reader for both Go and Rust portable backups.
use super::{CHUNK, DB, KEY, MAX_DATABASE, crc, invalid};
use alloc::vec::Vec;
use miniz_oxide::{
    DataFormat, MZFlush, MZStatus,
    inflate::stream::{InflateState, inflate},
};
use spin_domain::{Error, Fallible};

/// Random access to a staged, immutable upload. Short reads are invalid.
pub trait Source {
    /// Total uploaded bytes.
    fn size(&self) -> u64;
    /// Exact, bounded read. The reader requests at most one MiB.
    fn read(&mut self, offset: u64, length: usize) -> Fallible<Vec<u8>>;
}
fn read(source: &mut impl Source, offset: u64, length: usize) -> Fallible<Vec<u8>> {
    if offset
        .checked_add(length as u64)
        .is_none_or(|end| end > source.size())
    {
        return Err(invalid());
    }
    let bytes = source.read(offset, length)?;
    if bytes.len() != length {
        return Err(invalid());
    }
    Ok(bytes)
}
fn word(bytes: &[u8], at: usize, width: usize) -> Fallible<u64> {
    let field = bytes
        .get(at..at.checked_add(width).ok_or_else(invalid)?)
        .ok_or_else(invalid)?;
    Ok(field
        .iter()
        .enumerate()
        .fold(0, |n, (i, b)| n | (u64::from(*b) << (8 * i))))
}
/// A validated fixed-name archive member. Fields cannot be forged by callers.
#[derive(Clone, Copy)]
pub struct Entry {
    start: u64,
    compressed: u64,
    size: u64,
    checksum: u32,
    method: u16,
}
impl Entry {
    /// Expected extracted size, bounded before extraction starts.
    pub fn size(&self) -> u64 {
        self.size
    }
}
/// Exactly the two portable backup members; paths are never used as filesystem names.
pub struct Archive {
    /// SQLite database member.
    pub database: Entry,
    /// Source encryption key member.
    pub key: Entry,
}
impl Archive {
    /// Validate directory and local headers without loading member bodies.
    pub fn open(source: &mut impl Source) -> Fallible<Self> {
        let size = source.size();
        if !(22..=MAX_DATABASE + (1 << 20)).contains(&size) {
            return Err(invalid());
        }
        let length = size.min(65557) as usize;
        let tail_at = size - length as u64;
        let tail = read(source, tail_at, length)?;
        let end = (0..=length - 22)
            .rev()
            .find(|&i| {
                tail.get(i..i + 4) == Some(b"PK\x05\x06")
                    && word(&tail, i + 20, 2).is_ok_and(|n| i + 22 + n as usize == length)
            })
            .ok_or_else(invalid)?;
        if word(&tail, end + 4, 2)? != 0 || word(&tail, end + 6, 2)? != 0 {
            return Err(invalid());
        }
        let mut count = word(&tail, end + 10, 2)?;
        if word(&tail, end + 8, 2)? != count {
            return Err(invalid());
        }
        let mut directory_size = word(&tail, end + 12, 4)?;
        let mut directory = word(&tail, end + 16, 4)?;
        let end_at = tail_at + end as u64;
        let mut directory_end = end_at;
        if end_at >= 20 {
            let locator = read(source, end_at - 20, 20)?;
            if locator.starts_with(b"PK\x06\x07") {
                if word(&locator, 4, 4)? != 0 || word(&locator, 16, 4)? != 1 {
                    return Err(invalid());
                }
                let at = word(&locator, 8, 8)?;
                let z64 = read(source, at, 56)?;
                if !z64.starts_with(b"PK\x06\x06")
                    || word(&z64, 4, 8)? < 44
                    || at
                        .checked_add(12)
                        .and_then(|n| n.checked_add(word(&z64, 4, 8).ok()?))
                        != Some(end_at - 20)
                    || word(&z64, 16, 4)? != 0
                    || word(&z64, 20, 4)? != 0
                {
                    return Err(invalid());
                }
                count = word(&z64, 32, 8)?;
                if word(&z64, 24, 8)? != count {
                    return Err(invalid());
                }
                directory_size = word(&z64, 40, 8)?;
                directory = word(&z64, 48, 8)?;
                directory_end = at;
            }
        }
        if count != 2
            || directory_size > 1 << 20
            || directory.checked_add(directory_size) != Some(directory_end)
        {
            return Err(invalid());
        }
        let bytes = read(source, directory, directory_size as usize)?;
        let mut cursor = 0;
        let mut database = None;
        let mut key = None;
        let mut ranges = [(0, 0); 2];
        for range in &mut ranges {
            let header = bytes.get(cursor..).ok_or_else(invalid)?;
            if !header.starts_with(b"PK\x01\x02") {
                return Err(invalid());
            }
            let flags = word(header, 8, 2)?;
            let method = word(header, 10, 2)?;
            // Deflate option bits, data descriptor, and UTF-8 names are harmless.
            if flags & !0x80e != 0 || !matches!(method, 0 | 8) || word(header, 34, 2)? != 0 {
                return Err(invalid());
            }
            let mode = word(header, 38, 4)? >> 16;
            if mode & 0xf000 != 0 && mode & 0xf000 != 0x8000 {
                return Err(invalid());
            }
            let name_len = word(header, 28, 2)? as usize;
            let extra_len = word(header, 30, 2)? as usize;
            let comment_len = word(header, 32, 2)? as usize;
            let name = header.get(46..46 + name_len).ok_or_else(invalid)?;
            let extra = header
                .get(46 + name_len..46 + name_len + extra_len)
                .ok_or_else(invalid)?;
            let mut compressed = word(header, 20, 4)?;
            let mut size = word(header, 24, 4)?;
            let mut start = word(header, 42, 4)?;
            let mut found = false;
            let mut e = 0;
            while e < extra.len() {
                let tag = word(extra, e, 2)?;
                let len = word(extra, e + 2, 2)? as usize;
                let data = extra.get(e + 4..e + 4 + len).ok_or_else(invalid)?;
                if tag == 1 {
                    if found {
                        return Err(invalid());
                    }
                    found = true;
                    let mut pos = 0;
                    for value in [&mut size, &mut compressed, &mut start] {
                        if *value == u64::from(u32::MAX) {
                            *value = word(data, pos, 8)?;
                            pos += 8;
                        }
                    }
                }
                e += 4 + len;
            }
            if size == 0
                || size > MAX_DATABASE
                || compressed > MAX_DATABASE
                || (method == 0 && compressed != size)
                || (!found && [size, compressed, start].contains(&(u64::from(u32::MAX))))
            {
                return Err(invalid());
            }
            let target = if name == DB {
                &mut database
            } else if name == KEY && size <= 4096 {
                &mut key
            } else {
                return Err(invalid());
            };
            if target.is_some() {
                return Err(invalid());
            }
            let local = read(source, start, 30)?;
            if !local.starts_with(b"PK\x03\x04")
                || word(&local, 6, 2)? != flags
                || word(&local, 8, 2)? != method
                || word(&local, 26, 2)? != name_len as u64
            {
                return Err(invalid());
            }
            if read(source, start + 30, name_len)? != name {
                return Err(invalid());
            }
            let data = start
                .checked_add(30 + name_len as u64 + word(&local, 28, 2)?)
                .ok_or_else(invalid)?;
            let last = data.checked_add(compressed).ok_or_else(invalid)?;
            if last > directory {
                return Err(invalid());
            }
            *range = (start, last);
            *target = Some(Entry {
                start: data,
                compressed,
                size,
                checksum: word(header, 16, 4)? as u32,
                method: method as u16,
            });
            cursor += 46 + name_len + extra_len + comment_len;
        }
        if cursor != bytes.len() || (ranges[0].0 < ranges[1].1 && ranges[1].0 < ranges[0].1) {
            return Err(invalid());
        }
        Ok(Self {
            database: database.ok_or_else(invalid)?,
            key: key.ok_or_else(invalid)?,
        })
    }
}
/// One bounded extraction step per owner turn; no archive-sized allocations.
pub struct Decoder {
    entry: Entry,
    input: u64,
    output: u64,
    checksum: u32,
    inflater: InflateState,
    done: bool,
    failed: bool,
}
impl Decoder {
    /// Start an independently checksummed member.
    pub fn new(entry: Entry) -> Self {
        Self {
            entry,
            input: 0,
            output: 0,
            checksum: u32::MAX,
            inflater: InflateState::new(DataFormat::Raw),
            done: false,
            failed: false,
        }
    }
    /// None is returned only after size, stream end, and CRC have all matched.
    /// After a decoding error this object must be discarded.
    pub fn next(&mut self, source: &mut impl Source) -> Fallible<Option<Vec<u8>>> {
        if self.failed {
            return Err(invalid());
        }
        if self.done {
            return Ok(None);
        }
        let bytes = read(
            source,
            self.entry.start + self.input,
            (self.entry.compressed - self.input).min(CHUNK as u64) as usize,
        )?;
        let mut out = Vec::new();
        out.try_reserve_exact(CHUNK)
            .map_err(|_| Error::OutOfMemory)?;
        // Reserve before advancing the inflater, so allocation failure is retryable.
        self.failed = true;
        let end = if self.entry.method == 0 {
            out.extend_from_slice(&bytes);
            self.input += bytes.len() as u64;
            self.input == self.entry.compressed
        } else {
            out.resize(CHUNK, 0);
            let result = inflate(&mut self.inflater, &bytes, &mut out, MZFlush::None);
            self.input += result.bytes_consumed as u64;
            out.truncate(result.bytes_written);
            match result.status {
                Ok(MZStatus::StreamEnd) => true,
                Ok(MZStatus::Ok) if result.bytes_consumed != 0 || result.bytes_written != 0 => {
                    false
                }
                _ => return Err(invalid()),
            }
        };
        self.output += out.len() as u64;
        self.checksum = crc(self.checksum, &out);
        if self.output > self.entry.size
            || (end
                && (self.input != self.entry.compressed
                    || self.output != self.entry.size
                    || !self.checksum != self.entry.checksum))
        {
            return Err(invalid());
        }
        self.done = end;
        self.failed = false;
        Ok(Some(out))
    }
}
