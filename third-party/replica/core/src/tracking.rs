//! De tracking-VFS: dirty log vóór database-sync, marker vóór de eerste write.
use crate::{
    Error, Result,
    coverage::Pages,
    dirty,
    local::{self, File, Name},
    reserve,
    segment::valid_page_size,
};
use alloc::vec::Vec;
use core::ffi::CStr;
use replica_sqlite::{FileId, OpenFlags, Storage};
/// De eigenaar bewaart de volledige Replica-marker; deze callback zet Clean=false.
pub trait CleanMarker<B: Storage> {
    /// Moet duurzaam slagen vóór de eerste databasewrite/truncate na een clean marker.
    fn invalidate(&mut self, storage: &mut B) -> Result;
}
struct Journal {
    names: [Name; 2],
    active: Option<usize>,
    size: u64,
    noted: Pages,
    pending: Vec<u32>,
    broken: bool,
}
impl Journal {
    fn new(path: Name, limit: u32) -> Result<Self> {
        Ok(Self {
            names: [
                path.suffix(".replica-dirty-a")?,
                path.suffix(".replica-dirty-b")?,
            ],
            active: None,
            size: 0,
            noted: Pages::new(limit),
            pending: Vec::new(),
            broken: false,
        })
    }
    fn note(&mut self, page: u32) -> Result {
        if page == 0 {
            if self.broken {
                return Ok(());
            }
            crate::grow(&mut self.pending, 1, self.noted.limit() as usize + 1)?;
            self.pending.push(0);
            self.broken = true;
        } else if !self.noted.contains(page) {
            crate::grow(&mut self.pending, 1, self.noted.limit() as usize + 1)?;
            self.noted.add(page)?;
            self.pending.push(page);
        }
        Ok(())
    }
    fn records<B: Storage>(f: &mut File<'_, B>, pages: &[u32], mut offset: u64) -> Result {
        let mut bytes = [0u8; 8192];
        for part in pages.chunks(bytes.len() / 8) {
            for (dst, &page) in bytes.chunks_exact_mut(8).zip(part) {
                dst.copy_from_slice(&dirty::record(page));
            }
            let len = part.len() * 8;
            f.write(offset, &bytes[..len])?;
            offset = offset.checked_add(len as u64).ok_or(Error::Limit)?;
        }
        Ok(())
    }
    fn flush<B: Storage>(&mut self, b: &mut B) -> Result {
        if self.pending.is_empty() {
            return Ok(());
        }
        if self.active.is_none() {
            let pages = core::mem::take(&mut self.pending);
            let result = self.rewrite(b, "", 0, &pages);
            if result.is_err() {
                self.pending = pages;
                if self.active.is_some() {
                    self.broken = false;
                    self.note(0)?;
                }
            }
            return result;
        }
        let n = self.active.ok_or(Error::State)?;
        let size = self
            .size
            .checked_add(self.pending.len() as u64 * 8)
            .ok_or(Error::Limit)?;
        if size > dirty::MAX_BYTES as u64 {
            return Err(Error::Limit);
        }
        let mut f = File::open(b, &self.names[n], false)?;
        Self::records(&mut f, &self.pending, self.size)?;
        f.sync()?;
        f.close()?;
        self.size = size;
        self.pending.clear();
        Ok(())
    }
    fn rewrite<B: Storage>(
        &mut self,
        b: &mut B,
        generation: &str,
        sequence: u64,
        pages: &[u32],
    ) -> Result {
        let header = dirty::header(generation, sequence)?;
        let size = (header.len() as u64)
            .checked_add(pages.len() as u64 * 8)
            .ok_or(Error::Limit)?;
        if size > dirty::MAX_BYTES as u64 {
            return Err(Error::Limit);
        }
        // Alles wat kan alloceren gebeurt vóór een nieuwere header leesbaar wordt.
        let mut noted = Pages::new(self.noted.limit());
        for &p in pages {
            if p != 0 {
                noted.add(p)?;
            }
        }
        self.pending.try_reserve(1).map_err(|_| Error::Memory)?;
        let next = self.active.map_or(0, |n| 1 - n);
        let mut f = File::open(b, &self.names[next], true)?;
        f.truncate(0)?;
        Self::records(&mut f, pages, header.len() as u64)?;
        if !pages.is_empty() {
            f.sync()?;
        }
        // Na een headerpoging kan herstart dit nieuwere log kiezen, ook bij fout.
        // Dus alle volgende appends moeten hierheen, nooit terug naar het oude log.
        self.active = Some(next);
        self.size = size;
        self.noted = noted;
        self.pending.clear();
        self.broken = pages.contains(&0);
        let result = (|| {
            f.write(0, &header)?;
            f.sync()?;
            f.close()
        })();
        if result.is_err() {
            self.broken = true;
            self.pending.push(0);
        }
        result
    }
}
/// Eén eigenaar voor dirty state, twee logs, clean-overgang en de huidige paginamaten.
pub struct Tracking {
    path: Name,
    journal: Journal,
    dirty: Pages,
    page_size: u32,
    revision: u64,
    clean: bool,
    unknown: bool,
    pub(crate) source_counter: Option<u32>,
    pub(crate) witness: Option<(u32, [u8; 32])>,
}
impl Tracking {
    /// Alleen na een bewezen clean marker of volledige restore: alle lokale writes
    /// zijn al remote bevestigd. Het tweede oude log mag daarna duurzaam weg.
    pub fn reset_confirmed<B: Storage>(
        &mut self,
        b: &mut B,
        generation: &str,
        sequence: u64,
        page_size: u32,
    ) -> Result {
        if self.revision != 0 || self.journal.active.is_some() || !valid_page_size(page_size) {
            return Err(Error::State);
        }
        self.journal.rewrite(b, generation, sequence, &[])?;
        if b.exists(self.journal.names[1].cstr()?)? {
            b.remove(self.journal.names[1].cstr()?, true)?;
        }
        self.page_size = page_size;
        self.clean = true;
        Ok(())
    }
    /// Reserveert geen databasegrootte; het expliciete paginabudget geldt bij elke write.
    pub fn new(path: Name, page_limit: u32) -> Result<Self> {
        if page_limit == 0 {
            return Err(Error::Limit);
        }
        Ok(Self {
            path,
            journal: Journal::new(path, page_limit)?,
            dirty: Pages::new(page_limit),
            page_size: 0,
            revision: 0,
            clean: true,
            unknown: false,
            source_counter: None,
            witness: None,
        })
    }
    /// Laatst uit een SQLite-header geleerde paginamaten, nul zolang onbekend.
    pub const fn page_size(&self) -> u32 {
        self.page_size
    }
    /// Capture bewaart deze teller om een clean-marker niet over nieuwe writes te zetten.
    pub const fn revision(&self) -> u64 {
        self.revision
    }
    /// De huidige nummers kopiëren zonder ze vóór remote commit weg te nemen.
    pub fn pending(&self) -> Result<Vec<u32>> {
        self.dirty.numbers()
    }
    /// Is deze pagina sinds de vorige publicatie opnieuw geschreven?
    pub fn is_dirty(&self, page: u32) -> bool {
        self.dirty.contains(page)
    }
    /// Het expliciete maximumbudget van de database.
    pub const fn page_limit(&self) -> u32 {
        self.dirty.limit()
    }
    /// Bevestigt een capture die de dirty set niet vooraf afnam. Bij nieuwe writes
    /// blijft de superset staan: een pagina opnieuw verzenden is veilig.
    pub fn acknowledge<B: Storage>(
        &mut self,
        b: &mut B,
        generation: &str,
        sequence: u64,
        revision: u64,
        full: bool,
    ) -> Result<bool> {
        if revision > self.revision || (self.needs_snapshot() && !full) {
            return Err(Error::State);
        }
        if revision == self.revision {
            self.dirty.clear();
        }
        self.committed(b, generation, sequence, revision, full)
    }
    /// Niet toe te wijzen writes vereisen een volledig snapshot.
    pub const fn needs_snapshot(&self) -> bool {
        self.unknown || self.journal.broken
    }
    /// Dirty-nummers afnemen voor een capture; het log behoudt ze tot publicatie.
    pub fn take(&mut self) -> Result<Vec<u32>> {
        let pages = self.dirty.numbers()?;
        self.dirty.clear();
        Ok(pages)
    }
    /// Een mislukte capture/publicatie geeft de afgenomen nummers terug.
    pub fn put_back(&mut self, pages: &[u32]) -> Result {
        for &p in pages {
            self.dirty.add(p)?;
        }
        Ok(())
    }
    /// Start met logherstel na een bewezen lokale marker; corrupte logs vallen niet stil weg.
    pub fn recover<B: Storage>(
        &mut self,
        b: &mut B,
        generation: &str,
        sequence: u64,
        page_size: u32,
    ) -> Result {
        if self.revision != 0 || self.journal.active.is_some() {
            return Err(Error::State);
        }
        if !valid_page_size(page_size) {
            return Err(Error::Corrupt);
        }
        let a = if b.exists(self.journal.names[0].cstr()?)? {
            Some(local::read(b, &self.journal.names[0], dirty::MAX_BYTES)?)
        } else {
            None
        };
        let c = if b.exists(self.journal.names[1].cstr()?)? {
            Some(local::read(b, &self.journal.names[1], dirty::MAX_BYTES)?)
        } else {
            None
        };
        let log = dirty::select(a.as_deref(), c.as_deref(), generation, sequence)?;
        let mut pages = Vec::new();
        reserve(&mut pages, log.pages().len())?;
        pages.extend(log.pages());
        self.page_size = page_size;
        self.dirty.clear();
        self.put_back(&pages)?;
        // Bewaar de gekozen kant als de nieuwe rewrite onderweg faalt.
        self.journal.active = if a.as_deref().is_some_and(|bytes| {
            dirty::Log::decode(bytes)
                .is_ok_and(|l| l.generation == generation && l.sequence == log.sequence)
        }) {
            Some(0)
        } else {
            Some(1)
        };
        self.journal.rewrite(b, generation, sequence, &pages)?;
        // De marker kan bij boot Clean=true zijn. Altijd vóór de eerste nieuwe
        // write invalideren; bij een al vuile marker is dit alleen extra I/O.
        self.clean = true;
        Ok(())
    }
    /// Pas na bevestigde manifest- én lokale markerpublicatie het log herschrijven.
    /// `captured_revision` voorkomt een clean-marker over nieuwere databasewrites.
    pub fn committed<B: Storage>(
        &mut self,
        b: &mut B,
        generation: &str,
        sequence: u64,
        captured_revision: u64,
        full_snapshot: bool,
    ) -> Result<bool> {
        let mut pages = self.dirty.numbers()?;
        if captured_revision > self.revision || (self.needs_snapshot() && !full_snapshot) {
            return Err(Error::State);
        }
        if self.unknown && !(full_snapshot && self.revision == captured_revision) {
            reserve(&mut pages, 1)?;
            pages.push(0);
        }
        self.journal.rewrite(b, generation, sequence, &pages)?;
        if full_snapshot && self.revision == captured_revision {
            self.unknown = false;
        }
        self.clean =
            self.revision == captured_revision && pages.is_empty() && !self.needs_snapshot();
        Ok(self.clean)
    }
    /// Leent de tracking-VFS uitsluitend zolang één SQLite-runtime draait.
    pub fn wrap<'a, B: Storage, M: CleanMarker<B>>(
        &'a mut self,
        b: &'a mut B,
        marker: &'a mut M,
    ) -> Tracked<'a, B, M> {
        Tracked {
            tracking: self,
            storage: b,
            marker,
            main: None,
        }
    }
    fn learn(&mut self, bytes: &[u8]) -> Result {
        if bytes.len() < 18 || &bytes[..16] != b"SQLite format 3\0" {
            return Ok(());
        }
        let raw = u16::from_be_bytes([bytes[16], bytes[17]]);
        let size = if raw == 1 { 65536 } else { u32::from(raw) };
        if valid_page_size(size) {
            if self.page_size != 0 && self.page_size != size {
                self.unknown = true;
                self.journal.note(0)?;
            }
            self.page_size = size;
        }
        Ok(())
    }
    fn mark(&mut self, offset: u64, len: usize) -> Result {
        if self.page_size == 0 {
            self.unknown = true;
            return self.journal.note(0);
        }
        let end = offset.checked_add(len as u64).ok_or(Error::Limit)?;
        let first = offset / u64::from(self.page_size) + 1;
        let last = (end - 1) / u64::from(self.page_size) + 1;
        if last > u64::from(self.dirty.limit()) {
            return Err(Error::Limit);
        }
        for page in first..=last {
            self.dirty.add(page as u32)?;
            self.journal.note(page as u32)?;
        }
        Ok(())
    }
}
/// Geleende VFS-adapter. SQLite's journal gaat gewoon door de onderliggende VFS.
pub struct Tracked<'a, B: Storage, M: CleanMarker<B>> {
    tracking: &'a mut Tracking,
    storage: &'a mut B,
    marker: &'a mut M,
    main: Option<FileId>,
}
fn sqlite_error(e: Error) -> replica_sqlite::Error {
    match e {
        Error::Storage(e) => e,
        Error::Memory => replica_sqlite::Error::MEMORY,
        Error::Limit => replica_sqlite::Error::FULL,
        _ => replica_sqlite::Error::IO,
    }
}
impl<B: Storage, M: CleanMarker<B>> Tracked<'_, B, M> {
    fn unclean(&mut self) -> Result {
        if self.tracking.clean {
            self.marker.invalidate(self.storage)?;
        }
        self.tracking.clean = false;
        self.tracking.revision = self.tracking.revision.checked_add(1).ok_or(Error::Limit)?;
        Ok(())
    }
}
impl<B: Storage, M: CleanMarker<B>> Storage for Tracked<'_, B, M> {
    fn cooperate(&mut self) -> replica_sqlite::Result {
        self.storage.cooperate()
    }
    fn open(&mut self, name: &CStr, flags: OpenFlags) -> replica_sqlite::Result<FileId> {
        let main = name == self.tracking.path.cstr().map_err(sqlite_error)?;
        if main && self.main.is_some() {
            return Err(replica_sqlite::Error { code: 5 });
        }
        let id = self.storage.open(name, flags)?;
        if main {
            let mut header = [0; 18];
            let result = self
                .storage
                .read(id, 0, &mut header)
                .map_err(Error::from)
                .and_then(|n| {
                    if n > header.len() {
                        Err(Error::Corrupt)
                    } else {
                        self.tracking.learn(&header[..n])
                    }
                });
            if let Err(e) = result {
                let _ = self.storage.close(id);
                return Err(sqlite_error(e));
            }
            self.main = Some(id);
        }
        Ok(id)
    }
    fn close(&mut self, file: FileId) -> replica_sqlite::Result {
        if self.main == Some(file) {
            self.main = None;
        }
        self.storage.close(file)
    }
    fn read(&mut self, file: FileId, offset: u64, dst: &mut [u8]) -> replica_sqlite::Result<usize> {
        self.storage.read(file, offset, dst)
    }
    fn write(&mut self, file: FileId, offset: u64, src: &[u8]) -> replica_sqlite::Result {
        if self.main == Some(file) && !src.is_empty() {
            self.unclean().map_err(sqlite_error)?;
            if offset == 0 {
                self.tracking.learn(src).map_err(sqlite_error)?;
            }
            self.tracking
                .mark(offset, src.len())
                .map_err(sqlite_error)?;
        }
        self.storage.write(file, offset, src)
    }
    fn truncate(&mut self, file: FileId, size: u64) -> replica_sqlite::Result {
        if self.main == Some(file) {
            self.unclean().map_err(sqlite_error)?;
            self.tracking.dirty.add(1).map_err(sqlite_error)?;
            self.tracking.journal.note(1).map_err(sqlite_error)?;
        }
        self.storage.truncate(file, size)
    }
    fn sync(&mut self, file: FileId, flags: i32) -> replica_sqlite::Result {
        if self.main == Some(file) {
            self.tracking
                .journal
                .flush(self.storage)
                .map_err(sqlite_error)?;
        }
        self.storage.sync(file, flags)
    }
    fn size(&mut self, file: FileId) -> replica_sqlite::Result<u64> {
        self.storage.size(file)
    }
    fn remove(&mut self, name: &CStr, dir: bool) -> replica_sqlite::Result {
        // Een database verwijderen vereist eerst de volledige eigenaar stoppen.
        // SQLite verwijdert hier alleen journals en tijdelijke bestanden.
        if name == self.tracking.path.cstr().map_err(sqlite_error)? {
            return Err(replica_sqlite::Error::IO);
        }
        self.storage.remove(name, dir)
    }
    fn exists(&mut self, name: &CStr) -> replica_sqlite::Result<bool> {
        self.storage.exists(name)
    }
    fn random(&mut self, dst: &mut [u8]) -> replica_sqlite::Result {
        self.storage.random(dst)
    }
    fn unix_millis(&mut self) -> replica_sqlite::Result<i64> {
        self.storage.unix_millis()
    }
}
