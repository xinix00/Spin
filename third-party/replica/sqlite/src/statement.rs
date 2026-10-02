//! Een statement leent zijn verbinding; een rij leent weer zijn statement.
use crate::{Error, Result, check, ffi};
use core::{
    ffi::{CStr, c_void},
    marker::PhantomData,
    ptr, slice, str,
};

/// Een SQLite-waarde; bytes blijven geleend tot de volgende muterende stap.
#[derive(Debug, PartialEq)]
pub enum Value<'a> {
    /// SQL NULL.
    Null,
    /// Signed 64-bit integer.
    Integer(i64),
    /// IEEE-754 double.
    Real(f64),
    /// UTF-8 tekst met expliciete lengte.
    Text(&'a str),
    /// Willekeurige bytes, inclusief een lege blob.
    Blob(&'a [u8]),
}
/// Een voorbereid statement en de exclusieve lening van zijn verbinding.
pub struct Statement<'a> {
    raw: *mut c_void,
    row: bool,
    _connection: PhantomData<&'a mut c_void>,
}
impl Statement<'_> {
    pub(crate) fn prepare(raw: *mut c_void, sql: &CStr) -> Result<Self> {
        let mut stmt = ptr::null_mut();
        let mut tail = ptr::null();
        // SAFETY: De eigenaar leent de verbinding; SQLite kopieert de SQL-parserstaat.
        check(unsafe { ffi::sqlite3_prepare_v2(raw, sql.as_ptr(), -1, &mut stmt, &mut tail) })?;
        if stmt.is_null() {
            return Err(Error::MISUSE);
        }
        let value = Self {
            raw: stmt,
            row: false,
            _connection: PhantomData,
        };
        // SAFETY: tail wijst binnen de nul-afgesloten sql naar de ongebruikte suffix.
        if !unsafe { CStr::from_ptr(tail) }
            .to_bytes()
            .iter()
            .all(u8::is_ascii_whitespace)
        {
            return Err(Error::MISUSE);
        }
        Ok(value)
    }
    /// Bindt een waarde aan een één-gebaseerd parameternummer; bytes worden gekopieerd.
    pub fn bind(&mut self, index: u32, value: Value<'_>) -> Result {
        let index = i32::try_from(index).map_err(|_| Error::RANGE)?;
        let len = |n: usize| i32::try_from(n).map_err(|_| Error::FULL);
        // SAFETY: self leent het statement exclusief en TRANSIENT kopieert alle bytes.
        let rc = unsafe {
            match value {
                Value::Null => ffi::sqlite3_bind_null(self.raw, index),
                Value::Integer(n) => ffi::sqlite3_bind_int64(self.raw, index, n),
                Value::Real(n) => ffi::sqlite3_bind_double(self.raw, index, n),
                Value::Text(s) => {
                    ffi::replica_bind_text(self.raw, index, s.as_ptr(), len(s.len())?)
                }
                Value::Blob(b) => {
                    ffi::replica_bind_blob(self.raw, index, b.as_ptr(), len(b.len())?)
                }
            }
        };
        check(rc)
    }
    /// Stapt naar een rij (`true`) of het einde (`false`).
    pub fn step(&mut self) -> Result<bool> {
        self.row = false;
        // SAFETY: Het statement is exclusief geleend en alle oudere rijen zijn weg.
        match unsafe { ffi::sqlite3_step(self.raw) } {
            100 => {
                self.row = true;
                Ok(true)
            }
            101 => Ok(false),
            code => Err(Error { code }),
        }
    }
    /// Zet een statement terug voor hergebruik; bindingen blijven behouden.
    pub fn reset(&mut self) -> Result {
        self.row = false;
        // SAFETY: self is de enige eigenaar en er leeft geen geleende rij naast &mut.
        check(unsafe { ffi::sqlite3_reset(self.raw) })
    }
    /// Leest een nul-gebaseerde kolom van de huidige rij.
    pub fn column(&mut self, index: u32) -> Result<Value<'_>> {
        let index = i32::try_from(index).map_err(|_| Error::RANGE)?;
        // SAFETY: De verbinding/het statement leven en deze call wijzigt geen rij.
        if !self.row || index >= unsafe { ffi::sqlite3_column_count(self.raw) } {
            return Err(Error::RANGE);
        }
        // SAFETY: index is hierboven getoetst. De lening eindigt vóór step/reset/drop.
        unsafe {
            match ffi::sqlite3_column_type(self.raw, index) {
                1 => Ok(Value::Integer(ffi::sqlite3_column_int64(self.raw, index))),
                2 => Ok(Value::Real(ffi::sqlite3_column_double(self.raw, index))),
                3 => {
                    let b = self.bytes(index, true)?;
                    Ok(Value::Text(str::from_utf8(b).map_err(|_| Error::TEXT)?))
                }
                4 => Ok(Value::Blob(self.bytes(index, false)?)),
                5 => Ok(Value::Null),
                _ => Err(Error::MISUSE),
            }
        }
    }
    /// Vraagt bij TEXT eerst UTF-8, zodat column_bytes de pointer niet meer omzet.
    fn bytes(&self, index: i32, text: bool) -> Result<&[u8]> {
        // SAFETY: Deze private helper krijgt alleen een getoetste TEXT/BLOB-kolom.
        let (p, n) = unsafe {
            (
                if text {
                    ffi::sqlite3_column_text(self.raw, index)
                } else {
                    ffi::sqlite3_column_blob(self.raw, index)
                },
                ffi::sqlite3_column_bytes(self.raw, index),
            )
        };
        if text && p.is_null() {
            return Err(Error::MEMORY);
        }
        if n == 0 {
            return Ok(&[]);
        }
        if p.is_null() || n < 0 {
            return Err(Error::MEMORY);
        }
        // SAFETY: SQLite garandeert n leesbare bytes tot de volgende muterende call.
        Ok(unsafe { slice::from_raw_parts(p, n as usize) })
    }
}
impl Drop for Statement<'_> {
    fn drop(&mut self) {
        // SAFETY: De statement-eigenaar finalizeert precies eenmaal, vóór zijn verbinding.
        unsafe { ffi::sqlite3_finalize(self.raw) };
    }
}
