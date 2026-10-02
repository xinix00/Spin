//! Opslagcontract van de VFS; de aanroeper bezit bestanden, klok en entropie.
use crate::Result;
use core::ffi::CStr;

/// Een door de opslag uitgegeven bestandshandvat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId(pub u32);

/// SQLite's open-vlaggen, zonder platformbestandssysteem.
#[derive(Clone, Copy, Debug)]
pub struct OpenFlags(pub i32);
impl OpenFlags {
    /// SQLite vraagt toestemming een ontbrekend bestand te maken.
    pub fn is_create(self) -> bool {
        self.0 & 4 != 0
    }
    /// SQLite vraagt een schrijfbaar bestand.
    pub fn is_writable(self) -> bool {
        self.0 & 2 != 0
    }
    /// Het primaire databasebestand, in plaats van zijn journal.
    pub fn is_database(self) -> bool {
        self.0 & 0x100 != 0
    }
    /// Het rollback-journal.
    pub fn is_journal(self) -> bool {
        self.0 & 0x800 != 0
    }
}

/// Synchrone opslag van één eigenaar; callbacks mogen niet herintreden in SQLite.
///
/// `sync` moet alle voorafgaande writes duurzaam maken als deze backend duurzame
/// opslag belooft. `remove(..., true)` maakt ook het verdwijnen van de naam
/// duurzaam. Een RAM-backend belooft uitsluitend behoud tijdens zijn levensduur.
/// Een async HopOS-client hoort niet via een geneste executor in dit contract.
/// De eigenaar heeft exclusieve toegang tot het hele opslagdomein: er is geen
/// OS-locking voor schrijvers buiten deze runtime. Padnamen zijn canonieke namen
/// van de backend; aliassen naar hetzelfde bestand moeten worden geweigerd.
pub trait Storage {
    /// Geef de eigenaar periodiek ruimte; een fout onderbreekt de SQL-uitvoering.
    fn cooperate(&mut self) -> Result {
        Ok(())
    }
    /// Opent een bestand; het handvat blijft geldig tot `close`.
    fn open(&mut self, name: &CStr, flags: OpenFlags) -> Result<FileId>;
    /// Geeft een handvat vrij, ook na een mislukte SQLite-operatie.
    fn close(&mut self, file: FileId) -> Result;
    /// Leest bytes; een korte read betekent EOF en wordt door de VFS aangevuld.
    fn read(&mut self, file: FileId, offset: u64, dst: &mut [u8]) -> Result<usize>;
    /// Schrijft de volledige buffer of geeft een fout.
    fn write(&mut self, file: FileId, offset: u64, src: &[u8]) -> Result;
    /// Wijzigt de grootte; nieuw zichtbare bytes zijn nul.
    fn truncate(&mut self, file: FileId, size: u64) -> Result;
    /// Voltooit de opslagbarrière; een fout wordt SQLite's commitfout.
    fn sync(&mut self, file: FileId, flags: i32) -> Result;
    /// Geeft de logische bestandsgrootte.
    fn size(&mut self, file: FileId) -> Result<u64>;
    /// Verwijdert de naam; `sync_directory` vraagt een duurzame naamwijziging.
    fn remove(&mut self, name: &CStr, sync_directory: bool) -> Result;
    /// Onderzoekt uitsluitend het eigen opslagdomein.
    fn exists(&mut self, name: &CStr) -> Result<bool>;
    /// Vult alle bytes met entropie; een fout mag geen ongevulde bytes publiceren.
    fn random(&mut self, dst: &mut [u8]) -> Result;
    /// Geeft de UTC-klok in milliseconden sinds de Unix-epoch.
    fn unix_millis(&mut self) -> Result<i64>;
}
