//! De kleine, handgeschreven ABI naar de gevendorde SQLite-engine.
use core::ffi::{c_char, c_int, c_void};

#[repr(C)]
pub(crate) struct Callbacks {
    pub context: *mut c_void,
    pub open: unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut u32) -> c_int,
    pub close: unsafe extern "C" fn(*mut c_void, u32) -> c_int,
    pub read: unsafe extern "C" fn(*mut c_void, u32, u64, *mut u8, c_int) -> c_int,
    pub write: unsafe extern "C" fn(*mut c_void, u32, u64, *const u8, c_int) -> c_int,
    pub truncate: unsafe extern "C" fn(*mut c_void, u32, u64) -> c_int,
    pub sync: unsafe extern "C" fn(*mut c_void, u32, c_int) -> c_int,
    pub size: unsafe extern "C" fn(*mut c_void, u32, *mut u64) -> c_int,
    pub remove: unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_int,
    pub exists: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_int) -> c_int,
    pub random: unsafe extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int,
    pub time: unsafe extern "C" fn(*mut c_void, *mut i64) -> c_int,
    pub cooperate: unsafe extern "C" fn(*mut c_void) -> c_int,
}
unsafe extern "C" {
    pub(crate) fn replica_sqlite_init(
        heap: *mut c_void,
        bytes: c_int,
        storage: *const Callbacks,
    ) -> c_int;
    pub(crate) fn replica_sqlite_end();
    pub(crate) fn replica_sqlite_open(path: *const c_char, db: *mut *mut c_void) -> c_int;
    pub(crate) fn replica_sqlite_close(db: *mut c_void);
    pub(crate) fn sqlite3_exec(
        db: *mut c_void,
        sql: *const c_char,
        callback: *const c_void,
        context: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
    pub(crate) fn sqlite3_prepare_v2(
        db: *mut c_void,
        sql: *const c_char,
        len: c_int,
        stmt: *mut *mut c_void,
        tail: *mut *const c_char,
    ) -> c_int;
    pub(crate) fn sqlite3_finalize(stmt: *mut c_void) -> c_int;
    pub(crate) fn sqlite3_step(stmt: *mut c_void) -> c_int;
    pub(crate) fn sqlite3_reset(stmt: *mut c_void) -> c_int;
    pub(crate) fn sqlite3_bind_int64(stmt: *mut c_void, index: c_int, value: i64) -> c_int;
    pub(crate) fn sqlite3_bind_double(stmt: *mut c_void, index: c_int, value: f64) -> c_int;
    pub(crate) fn sqlite3_bind_null(stmt: *mut c_void, index: c_int) -> c_int;
    pub(crate) fn replica_bind_text(
        stmt: *mut c_void,
        index: c_int,
        value: *const u8,
        len: c_int,
    ) -> c_int;
    pub(crate) fn replica_bind_blob(
        stmt: *mut c_void,
        index: c_int,
        value: *const u8,
        len: c_int,
    ) -> c_int;
    pub(crate) fn sqlite3_column_count(stmt: *mut c_void) -> c_int;
    pub(crate) fn sqlite3_column_type(stmt: *mut c_void, index: c_int) -> c_int;
    pub(crate) fn sqlite3_column_int64(stmt: *mut c_void, index: c_int) -> i64;
    pub(crate) fn sqlite3_column_double(stmt: *mut c_void, index: c_int) -> f64;
    pub(crate) fn sqlite3_column_blob(stmt: *mut c_void, index: c_int) -> *const u8;
    pub(crate) fn sqlite3_column_text(stmt: *mut c_void, index: c_int) -> *const u8;
    pub(crate) fn sqlite3_column_bytes(stmt: *mut c_void, index: c_int) -> c_int;
    pub(crate) fn sqlite3_changes64(db: *mut c_void) -> i64;
    pub(crate) fn sqlite3_libversion_number() -> c_int;
}
