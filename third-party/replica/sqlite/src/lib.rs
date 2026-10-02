//! Native SQLite met een exclusieve eigenaar en een door de aanroeper geleende VFS.
//!
//! De C-engine krijgt een vaste heap, geen libc-bestandsfuncties en geen threads.
//! De veilige verbinding- en statement-API leent die boot-runtime; RAII sluit
//! ook bij vergeten handvatten de resterende C-objecten voordat de heap vrijkomt.
#![no_std]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod asynchronous;
mod callbacks;
mod ffi;
pub mod memory;
mod statement;
mod storage;
use core::{
    ffi::{CStr, c_void},
    fmt,
    marker::PhantomData,
    ptr,
};
pub use statement::{Statement, Value};
pub use storage::{FileId, OpenFlags, Storage};

/// Resultaat met SQLite's primaire of uitgebreide foutcode.
pub type Result<T = ()> = core::result::Result<T, Error>;
/// Een SQLite-fout, zonder allocatie of verloren uitgebreide foutcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    /// De door SQLite of de VFS geretourneerde code.
    pub code: i32,
}
impl Error {
    /// Onvoldoende geheugen in de vaste SQLite-heap.
    pub const MEMORY: Self = Self { code: 7 };
    /// De opslag kon de opdracht niet uitvoeren.
    pub const IO: Self = Self { code: 10 };
    /// De opslaglimiet is bereikt.
    pub const FULL: Self = Self { code: 13 };
    /// Het bestand kan niet worden geopend.
    pub const CANNOT_OPEN: Self = Self { code: 14 };
    /// De aanroep past niet bij de levensloop of het opslagcontract.
    pub const MISUSE: Self = Self { code: 21 };
    /// Parameter- of kolomnummer buiten bereik.
    pub const RANGE: Self = Self { code: 25 };
    /// Externe tekst is geen geldige UTF-8.
    pub const TEXT: Self = Self { code: 20 };
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sqlite: code={}", self.code)
    }
}
impl core::error::Error for Error {}
pub(crate) fn check(code: i32) -> Result {
    if code == 0 {
        Ok(())
    } else {
        Err(Error { code })
    }
}

/// De unieke procesruntime; de heap en opslag blijven exclusief geleend.
///
/// # Invariants
/// SQLite is single-threaded; dit type is niet `Send` of `Sync`. C bewaart geen
/// verwijzing naar dit verplaatsbare handvat, alleen naar de geleende buffers.
pub struct Engine<'a, B: Storage> {
    _owner: PhantomData<(&'a mut B, &'a mut [u64], *mut ())>,
}
impl<'a, B: Storage> Engine<'a, B> {
    /// Initialiseert de unieke SQLite-runtime tijdens de bootfase.
    ///
    /// # Safety
    /// Er mag in dit proces geen andere SQLite-runtime actief zijn of tegelijk
    /// worden gestart. Alle SQLite-calls blijven op deze eigenaar-thread; ook
    /// opslagcallbacks mogen niet in SQLite herintreden. Deze voorwaarde hoort
    /// bij de boot-schil, omdat SQLite zelf procesglobale C-staat bevat.
    pub unsafe fn initialize(heap: &'a mut [u64], backend: &'a mut B) -> Result<Self> {
        let bytes = heap
            .len()
            .checked_mul(8)
            .and_then(|n| i32::try_from(n).ok())
            .ok_or(Error::MEMORY)?;
        let callbacks = callbacks::callbacks(backend);
        // SAFETY: De aanroeper bewijst de unieke boot-runtime; heap en B zijn geleend
        // tot Drop. De C-shim kopieert callbacks voordat deze stackwaarde verdwijnt.
        check(unsafe { ffi::replica_sqlite_init(heap.as_mut_ptr().cast(), bytes, &callbacks) })?;
        Ok(Self {
            _owner: PhantomData,
        })
    }
    /// Opent één verbinding en leent daarvoor de hele runtime.
    pub fn open<'e>(&'e mut self, path: &CStr) -> Result<Connection<'e, 'a, B>> {
        let mut raw = ptr::null_mut();
        // SAFETY: self is de exclusieve runtime en path blijft geldig gedurende open.
        check(unsafe { ffi::replica_sqlite_open(path.as_ptr(), &mut raw) })?;
        if raw.is_null() {
            return Err(Error::MEMORY);
        }
        Ok(Connection {
            raw,
            _engine: PhantomData,
        })
    }
}
impl<B: Storage> Drop for Engine<'_, B> {
    fn drop(&mut self) {
        // SAFETY: De levenslopen sluiten verdere veilige toegang uit. De C-shim ruimt
        // ook met mem::forget gelekte verbindingen/statements op vóór heap/B vrij zijn.
        unsafe { ffi::replica_sqlite_end() };
    }
}
/// Een verbinding die de runtime exclusief leent; de eigenaar serialiseert SQL.
pub struct Connection<'e, 'a, B: Storage> {
    pub(crate) raw: *mut c_void,
    _engine: PhantomData<&'e mut Engine<'a, B>>,
}
impl<B: Storage> Connection<'_, '_, B> {
    /// Voert SQL uit; commitfouten van de VFS komen ongewijzigd terug.
    pub fn execute(&mut self, sql: &CStr) -> Result {
        // SAFETY: De verbinding is exclusief; exec behoudt sql noch callbacks.
        check(unsafe {
            ffi::sqlite3_exec(
                self.raw,
                sql.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        })
    }
    /// Bereidt exact één statement voor; volgende statements worden geweigerd.
    pub fn prepare<'s>(&'s mut self, sql: &CStr) -> Result<Statement<'s>> {
        Statement::prepare(self.raw, sql)
    }
    /// Het aantal gewijzigde rijen van het laatst voltooide statement.
    pub fn changes(&self) -> i64 {
        // SAFETY: Deze lening kan niet naast een actieve statement-mutatie bestaan.
        unsafe { ffi::sqlite3_changes64(self.raw) }
    }
}
impl<B: Storage> Drop for Connection<'_, '_, B> {
    fn drop(&mut self) {
        // SAFETY: Geen statement kan deze eigenaar veilig overleven; de C-shim
        // sluit ook vergeten statements, zodat close geen verbinding laat lekken.
        unsafe { ffi::replica_sqlite_close(self.raw) };
    }
}
/// Het gepinde SQLite-versienummer, bijvoorbeeld 3053004 voor 3.53.4.
pub fn version_number() -> i32 {
    // SAFETY: Dit leest alleen SQLite's constante versienummer, zonder runtime.
    unsafe { ffi::sqlite3_libversion_number() }
}
