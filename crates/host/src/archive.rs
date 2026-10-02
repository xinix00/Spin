//! Tijdelijke archives horen bij één operatie en verdwijnen bij iedere return/drop.
use crate::{executor, process, storage::Random};
use spin_core::{
    archive::{BLOCK, Extensions, Header, METADATA_LIMIT},
    process::Command,
};
use spin_store::IdSource;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};
pub(crate) const FILE_LIMIT: u64 = 64 << 30;
pub(crate) struct Temporary {
    pub(crate) file: File,
    path: PathBuf,
}
impl Temporary {
    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }
    pub(crate) fn new() -> std::io::Result<Self> {
        let mut random = Random::open()?;
        for _ in 0..8 {
            let path = std::env::temp_dir().join(
                random
                    .next("spin-rust-archive")
                    .map_err(std::io::Error::other)?,
            );
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => return Ok(Self { file, path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::other("temporary archive name collision"))
    }
    pub(crate) fn rewind(&mut self) -> std::io::Result<()> {
        self.file.rewind()
    }
    pub(crate) async fn capture(mut command: Command) -> std::io::Result<Self> {
        command.merge_stderr = false;
        let mut spool = Self::new()?;
        check(process::transfer(command, None, Some(&mut spool.file), FILE_LIMIT).await?)?;
        spool.rewind()?;
        Ok(spool)
    }
    /// gzip wordt alleen aan de Unix-hostgrens gebruikt; invoer blijft via een fd gaan.
    pub(crate) async fn gzip(&mut self, decompress: bool) -> std::io::Result<Self> {
        let mut command = Command::new("gzip").map_err(std::io::Error::other)?;
        command
            .arg(if decompress { "-dc" } else { "-1c" })
            .map_err(std::io::Error::other)?;
        command.timeout_ms = 15 * 60 * 1000;
        let mut result = Self::new()?;
        self.rewind()?;
        check(
            process::transfer(
                command,
                Some(&mut self.file),
                Some(&mut result.file),
                FILE_LIMIT,
            )
            .await?,
        )?;
        result.rewind()?;
        Ok(result)
    }
    pub(crate) fn compressed(&mut self) -> std::io::Result<bool> {
        self.rewind()?;
        let mut bytes = [0; 2];
        let n = self.file.read(&mut bytes)?;
        self.rewind()?;
        Ok(n == 2 && bytes == [0x1f, 0x8b])
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
pub(crate) fn check(output: process::Output) -> std::io::Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let value = spin_core::validation::text(format_args!(
        "archive command exited {}: {}",
        output.status.code().unwrap_or(-1),
        spin_core::docker::utf8(&output.stderr, true).map_err(std::io::Error::other)?
    ))
    .map_err(std::io::Error::other)?;
    Err(std::io::Error::other(value))
}
pub(crate) struct Entry {
    pub(crate) header: Header,
    pub(crate) start: u64,
    pub(crate) data: u64,
    pub(crate) end: u64,
}
pub(crate) struct Reader<'a> {
    pub(crate) file: &'a mut File,
    next: u64,
    length: u64,
    entries: usize,
}
impl<'a> Reader<'a> {
    pub(crate) fn new(file: &'a mut File) -> std::io::Result<Self> {
        let length = file.metadata()?.len();
        if length > FILE_LIMIT {
            return Err(std::io::Error::other("archive exceeds disk budget"));
        }
        Ok(Self {
            file,
            next: 0,
            length,
            entries: 0,
        })
    }
    pub(crate) async fn next(&mut self) -> std::io::Result<Option<Entry>> {
        executor::progress();
        executor::next_round().await;
        let start = self.next;
        let mut extensions = Extensions::default();
        let mut metadata = 0;
        loop {
            if self.next == self.length {
                if self.next != start {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                return Ok(None);
            }
            self.file.seek(SeekFrom::Start(self.next))?;
            let mut block = [0; BLOCK];
            self.file.read_exact(&mut block)?;
            let Some(mut header) = Header::decode(&block).map_err(std::io::Error::other)? else {
                if self.next != start {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                return Ok(None);
            };
            self.next += u64::try_from(BLOCK).map_err(std::io::Error::other)?;
            if matches!(header.kind, b'x' | b'g' | b'L' | b'K') {
                let data_start = self.next;
                let size = usize::try_from(header.padded_size().map_err(std::io::Error::other)?)
                    .map_err(std::io::Error::other)?;
                if size.saturating_add(BLOCK) > METADATA_LIMIT.saturating_sub(metadata) {
                    return Err(std::io::Error::other("tar metadata budget exceeded"));
                }
                metadata += size + BLOCK;
                let mut data = Vec::new();
                data.try_reserve_exact(size)
                    .map_err(std::io::Error::other)?;
                data.resize(size, 0);
                self.file.read_exact(&mut data)?;
                extensions
                    .read(
                        header.kind,
                        &data[..usize::try_from(header.size).map_err(std::io::Error::other)?],
                    )
                    .map_err(std::io::Error::other)?;
                self.next += u64::try_from(size).map_err(std::io::Error::other)?;
                if header.kind == b'g' {
                    // archive/tar exposeert een globale pax-kop zelf, met nul
                    // mode/size/link; de ruwe records reizen ongewijzigd mee.
                    extensions.apply(&mut header);
                    header.mode = 0;
                    header.size = 0;
                    header.link.clear();
                    self.entries += 1;
                    if self.entries > 1_000_000 {
                        return Err(std::io::Error::other("archive has too many entries"));
                    }
                    return Ok(Some(Entry {
                        header,
                        start,
                        data: data_start,
                        end: self.next,
                    }));
                }
                continue;
            }
            extensions.apply(&mut header);
            if block[156] == 0 && header.name.ends_with('/') {
                header.kind = b'5';
            }
            let data = self.next;
            self.next = self
                .next
                .checked_add(header.padded_size().map_err(std::io::Error::other)?)
                .ok_or_else(|| std::io::Error::other("tar entry overflows"))?;
            if self.next > self.length {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            self.entries += 1;
            if self.entries > 1_000_000 {
                return Err(std::io::Error::other("archive has too many entries"));
            }
            return Ok(Some(Entry {
                header,
                start,
                data,
                end: self.next,
            }));
        }
    }
}
/// De metadata en bytes blijven origineel; ook xattrs, hardlinks en symlinks reizen mee.
pub(crate) async fn copy_range(
    input: &mut File,
    output: &mut File,
    start: u64,
    size: u64,
) -> std::io::Result<()> {
    input.seek(SeekFrom::Start(start))?;
    let mut remaining = size;
    let mut bytes = [0; 8192];
    while remaining > 0 {
        let n =
            usize::try_from(remaining.min(bytes.len() as u64)).map_err(std::io::Error::other)?;
        input.read_exact(&mut bytes[..n])?;
        output.write_all(&bytes[..n])?;
        remaining -= u64::try_from(n).map_err(std::io::Error::other)?;
        executor::progress();
        executor::next_round().await;
    }
    Ok(())
}
pub(crate) async fn hash_range(
    input: &mut File,
    start: u64,
    size: u64,
) -> std::io::Result<[u8; 32]> {
    input.seek(SeekFrom::Start(start))?;
    let mut remaining = size;
    let mut bytes = [0; 8192];
    let mut hash = spin_security::Sha256::new();
    while remaining > 0 {
        let n =
            usize::try_from(remaining.min(bytes.len() as u64)).map_err(std::io::Error::other)?;
        input.read_exact(&mut bytes[..n])?;
        hash.update(&bytes[..n]);
        remaining -= u64::try_from(n).map_err(std::io::Error::other)?;
        executor::progress();
        executor::next_round().await;
    }
    Ok(hash.finish())
}
pub(crate) fn finish(file: &mut File) -> std::io::Result<()> {
    file.write_all(&[0; BLOCK * 2])
}
