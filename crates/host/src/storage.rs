//! Duurzame Spin-bestanden met één eigenaar en een begrensde handvattenpool.
use replica_sqlite::{Error, FileId, OpenFlags, Result, Storage};
use std::{
    ffi::CStr,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Hoogstens deze hoeveelheid tegelijk geopende SQLite-bestanden.
pub const MAX_FILES: usize = 16;
struct OpenFile {
    file: File,
    id: FileId,
}
/// De directory is exclusief voor deze Spin-instantie; andere databaseprogramma's
/// mogen haar niet openen. Het lockbestand voorkomt een tweede Spin-eigenaar.
pub struct Files {
    root: PathBuf,
    directory: File,
    _lock: File,
    files: [Option<OpenFile>; MAX_FILES],
    sequence: u32,
    entropy: Random,
}
/// Cryptografische hostentropie, uitsluitend aan de Unix-hostgrens.
pub struct Random(File);
impl Random {
    /// Opent de kernelbron eenmaal tijdens boot.
    pub fn open() -> std::io::Result<Self> {
        Ok(Self(File::open("/dev/urandom")?))
    }
}
impl spin_security::Entropy for Random {
    fn fill(&mut self, dst: &mut [u8]) -> spin_security::Result {
        self.0
            .read_exact(dst)
            .map_err(|e| spin_security::Error::Entropy(e.raw_os_error().unwrap_or(-1)))
    }
}
impl spin_store::IdSource for Random {
    fn next(&mut self, prefix: &str) -> spin_store::Result<String> {
        let mut bytes = [0; 16];
        spin_security::Entropy::fill(self, &mut bytes)?;
        let mut id = spin_domain::try_string(prefix)?;
        spin_domain::try_push_str(&mut id, "_")?;
        // Go gebruikt eveneens 16 willekeurige bytes in lowercase hex.
        for byte in bytes {
            use std::fmt::Write;
            id.try_reserve(2)
                .map_err(|_| spin_domain::Error::OutOfMemory)?;
            write!(&mut id, "{byte:02x}").map_err(|_| spin_domain::Error::OutOfMemory)?;
        }
        Ok(id)
    }
}
impl Files {
    /// Opent een bestaande of nieuwe directory en houdt haar lock tot Drop.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".spin.lock"))?;
        lock.try_lock().map_err(std::io::Error::other)?;
        let directory = File::open(&root)?;
        Ok(Self {
            root,
            directory,
            _lock: lock,
            files: std::array::from_fn(|_| None),
            sequence: 0,
            entropy: Random::open()?,
        })
    }
    fn path(&self, name: &CStr) -> Result<PathBuf> {
        let name = name.to_str().map_err(|_| Error::CANNOT_OPEN)?;
        // Restore uses SQLite's super-journal for its attached database transaction.
        // Only SQLite's fixed-width generated suffix is accepted, never a caller's path.
        let super_journal = name.strip_prefix("spin.sqlite-mj").is_some_and(|suffix| {
            suffix.len() == 9
                && suffix.as_bytes()[6] == b'9'
                && suffix.bytes().all(|b| b.is_ascii_hexdigit())
        });
        // SQLite's tijdelijke bestanden (sorteren, indexbouw): etilqs_ plus
        // zestien hexcijfers van de VFS, verwijderd bij het sluiten.
        let temp = name
            .strip_prefix("etilqs_")
            .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
        if !super_journal
            && !temp
            && !matches!(
                name,
                "spin.sqlite"
                    | "spin.sqlite-journal"
                    | "spin-restore.sqlite"
                    | "spin-restore.sqlite-journal"
                    // Replica-sidecars: marker, dirty logs, capture-spool en restore-scratch.
                    | "spin.sqlite.replica"
                    | "spin.sqlite.replica-dirty-a"
                    | "spin.sqlite.replica-dirty-b"
                    | "spin.sqlite.replica-capture"
                    | "spin.sqlite.replica-pending"
                    | "spin.sqlite.replica-shadow"
                    | "spin.sqlite.replica-restore-data"
                    | "spin.sqlite.replica-restore-data-journal"
                    | "spin.sqlite.replica-restoring"
            )
        {
            return Err(Error::CANNOT_OPEN);
        }
        let path = self.root.join(name);
        match path.symlink_metadata() {
            Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => Err(Error::CANNOT_OPEN),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Error::IO),
            _ => Ok(path),
        }
    }
    fn file(&mut self, id: FileId) -> Result<&mut File> {
        self.files
            .iter_mut()
            .flatten()
            .find(|f| f.id == id)
            .map(|f| &mut f.file)
            .ok_or(Error::MISUSE)
    }
}
impl Storage for Files {
    fn open(&mut self, name: &CStr, flags: OpenFlags) -> Result<FileId> {
        let path = self.path(name)?;
        let slot = self
            .files
            .iter_mut()
            .find(|f| f.is_none())
            .ok_or(Error::FULL)?;
        let id = FileId(self.sequence.checked_add(1).ok_or(Error::FULL)?);
        let file = OpenOptions::new()
            .read(true)
            .write(flags.is_writable())
            .create(flags.is_create())
            .truncate(false)
            .open(path)
            .map_err(|_| Error::CANNOT_OPEN)?;
        self.sequence = id.0;
        *slot = Some(OpenFile { file, id });
        Ok(id)
    }
    fn close(&mut self, id: FileId) -> Result {
        let slot = self
            .files
            .iter_mut()
            .find(|s| s.as_ref().is_some_and(|f| f.id == id))
            .ok_or(Error::MISUSE)?;
        *slot = None;
        Ok(())
    }
    fn read(&mut self, id: FileId, offset: u64, dst: &mut [u8]) -> Result<usize> {
        let file = self.file(id)?;
        file.seek(SeekFrom::Start(offset)).map_err(|_| Error::IO)?;
        let mut done = 0;
        while let Some(dst) = dst.get_mut(done..).filter(|s| !s.is_empty()) {
            match file.read(dst) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(Error::IO),
            }
        }
        Ok(done)
    }
    fn write(&mut self, id: FileId, offset: u64, src: &[u8]) -> Result {
        let file = self.file(id)?;
        file.seek(SeekFrom::Start(offset)).map_err(|_| Error::IO)?;
        file.write_all(src).map_err(|_| Error::IO)
    }
    fn truncate(&mut self, id: FileId, size: u64) -> Result {
        self.file(id)?.set_len(size).map_err(|_| Error::IO)
    }
    fn sync(&mut self, id: FileId, _: i32) -> Result {
        self.file(id)?.sync_all().map_err(|_| Error::IO)?;
        // Een nieuw journal moet ook via zijn directory-entry duurzaam zijn
        // vóór SQLite databasepagina's overschrijft.
        self.directory.sync_all().map_err(|_| Error::IO)
    }
    fn size(&mut self, id: FileId) -> Result<u64> {
        Ok(self.file(id)?.metadata().map_err(|_| Error::IO)?.len())
    }
    fn remove(&mut self, name: &CStr, sync_directory: bool) -> Result {
        let path = self.path(name)?;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error::IO),
        }
        if sync_directory {
            self.directory.sync_all().map_err(|_| Error::IO)?;
        }
        Ok(())
    }
    fn exists(&mut self, name: &CStr) -> Result<bool> {
        self.path(name)?.try_exists().map_err(|_| Error::IO)
    }
    fn random(&mut self, dst: &mut [u8]) -> Result {
        spin_security::Entropy::fill(&mut self.entropy, dst).map_err(|_| Error::IO)
    }
    fn unix_millis(&mut self) -> Result<i64> {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| Error::IO)?
                .as_millis(),
        )
        .map_err(|_| Error::IO)
    }
}
