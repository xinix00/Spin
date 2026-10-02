//! Eén eigenaar-thread toetst de echte C-engine, inclusief fouten uit de VFS.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use replica_sqlite::memory::{Memory, Slot};
use replica_sqlite::{Connection, Engine, Error, FileId, OpenFlags, Result, Storage, Value};
use std::{cell::Cell, ffi::CStr};

fn integer<B: Storage>(db: &mut Connection<'_, '_, B>, sql: &CStr) -> i64 {
    let mut s = db.prepare(sql).unwrap();
    assert!(s.step().unwrap());
    let Value::Integer(n) = s.column(0).unwrap() else {
        panic!("integer expected")
    };
    n
}

fn integrity<B: Storage>(db: &mut Connection<'_, '_, B>) {
    let mut s = db.prepare(c"PRAGMA integrity_check").unwrap();
    assert!(s.step().unwrap());
    assert_eq!(s.column(0).unwrap(), Value::Text("ok"));
    assert!(!s.step().unwrap());
}

/// Alle initialisaties zitten bewust in één test: SQLite heeft globale C-staat.
#[test]
fn native_sqlite_contract() {
    assert_eq!(replica_sqlite::version_number(), 3_053_004);
    let mut heap = vec![0_u64; 512 * 1024];
    let mut data = vec![0_u8; 1024 * 1024];
    let mut journal = vec![0_u8; 1024 * 1024];
    let mut memory = Memory::new(
        [Slot::new(&mut data), Slot::new(&mut journal)],
        1_790_683_200_000,
        7,
    );
    {
        // SAFETY: Dit is de enige test en runtime; alle calls blijven op deze thread.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut memory) }.unwrap();
        let mut db = engine.open(c"test.db").unwrap();
        assert_eq!(integer(&mut db, c"PRAGMA synchronous"), 3);
        db.execute(c"CREATE TABLE item(id INTEGER PRIMARY KEY, n REAL, text TEXT, bytes BLOB, missing); BEGIN").unwrap();
        {
            let mut insert = db
                .prepare(c"INSERT INTO item VALUES(?1,?2,?3,?4,?5)")
                .unwrap();
            insert.bind(1, Value::Integer(i64::MAX)).unwrap();
            insert.bind(2, Value::Real(1.25)).unwrap();
            insert.bind(3, Value::Text("hé\0世界")).unwrap();
            insert.bind(4, Value::Blob(&[0, 255, 3])).unwrap();
            insert.bind(5, Value::Null).unwrap();
            assert!(!insert.step().unwrap());
            insert.reset().unwrap();
            insert.bind(1, Value::Integer(2)).unwrap();
            insert.bind(3, Value::Text("")).unwrap();
            insert.bind(4, Value::Blob(&[])).unwrap();
            assert!(!insert.step().unwrap());
        }
        db.execute(c"COMMIT").unwrap();
        assert_eq!(db.changes(), 1);
        {
            let mut s = db.prepare(c"SELECT * FROM item ORDER BY id DESC").unwrap();
            assert_eq!(s.column(0).unwrap_err(), Error::RANGE);
            assert!(s.step().unwrap());
            assert_eq!(s.column(0).unwrap(), Value::Integer(i64::MAX));
            assert_eq!(s.column(1).unwrap(), Value::Real(1.25));
            assert_eq!(s.column(2).unwrap(), Value::Text("hé\0世界"));
            assert_eq!(s.column(3).unwrap(), Value::Blob(&[0, 255, 3]));
            assert_eq!(s.column(4).unwrap(), Value::Null);
            assert_eq!(s.column(5).unwrap_err(), Error::RANGE);
            assert!(s.step().unwrap());
            assert_eq!(s.column(2).unwrap(), Value::Text(""));
            assert_eq!(s.column(3).unwrap(), Value::Blob(&[]));
            assert!(!s.step().unwrap());
        }
        db.execute(c"BEGIN; DELETE FROM item; ROLLBACK").unwrap();
        assert_eq!(integer(&mut db, c"SELECT count(*) FROM item"), 2);
        assert!(db.prepare(c"SELECT 1; SELECT 2").is_err());
        assert!(db.prepare(c"  ").is_err());
        assert!(db.execute(c"broken syntax").is_err());
        integrity(&mut db);
        // Ook vergeten statements moeten verdwijnen voordat de heap vrijkomt.
        std::mem::forget(db.prepare(c"SELECT * FROM item").unwrap());
        std::mem::forget(db);
    }
    assert!(memory.stats().reads > 0);
    assert!(memory.stats().writes > 0);
    assert!(memory.stats().syncs > 0);
    assert!(memory.stats().deletes > 0);
    assert_eq!(
        &memory.contents(c"test.db").unwrap()[..16],
        b"SQLite format 3\0"
    );
    {
        // SAFETY: De vorige runtime is volledig gedropt, op dezelfde testthread.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut memory) }.unwrap();
        let mut db = engine.open(c"test.db").unwrap();
        assert_eq!(integer(&mut db, c"SELECT count(*) FROM item"), 2);
        integrity(&mut db);
    }

    // Een journalbarrière die faalt mag geen succesvolle commit opleveren.
    let fail_at = Cell::new(0);
    let mut faulty = Fault {
        inner: memory,
        fail_at: &fail_at,
    };
    for barrier in 1..=3 {
        {
            // SAFETY: Iedere voorgaande runtime is dicht; één testthread.
            let mut engine = unsafe { Engine::initialize(&mut heap, &mut faulty) }.unwrap();
            let mut db = engine.open(c"test.db").unwrap();
            db.execute(c"BEGIN; DELETE FROM item").unwrap();
            fail_at.set(barrier);
            let err = db.execute(c"COMMIT").unwrap_err();
            assert_eq!(err.code & 255, Error::IO.code);
            fail_at.set(0);
        }
        {
            // SAFETY: Vorige verbinding en runtime zijn weg; dezelfde thread.
            let mut engine = unsafe { Engine::initialize(&mut heap, &mut faulty) }.unwrap();
            let mut db = engine.open(c"test.db").unwrap();
            integrity(&mut db);
            assert_eq!(integer(&mut db, c"SELECT count(*) FROM item"), 2);
        }
    }
    // De vaste SQLite-heap en opslag geven FULL/NOMEM, geen onbeperkte allocatie.
    {
        // SAFETY: De enige runtime op deze thread.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut faulty) }.unwrap();
        let mut db = engine.open(c"test.db").unwrap();
        let err = db
            .execute(c"INSERT INTO item(id,bytes) VALUES(3,zeroblob(1500000))")
            .unwrap_err();
        assert_eq!(err.code & 255, Error::FULL.code);
        integrity(&mut db);
        assert_eq!(integer(&mut db, c"SELECT count(*) FROM item"), 2);
        let err = db.execute(c"SELECT zeroblob(100000000)").unwrap_err();
        assert_eq!(err.code & 255, Error::MEMORY.code);
        integrity(&mut db);
    }

    // Ook een query zonder bestands-I/O moet onderbreekbaar blijven.
    let interrupt = Cell::new(true);
    let mut progress = Progress {
        inner: &mut faulty,
        interrupt: &interrupt,
    };
    {
        // SAFETY: De voorgaande runtime is dicht; deze testthread blijft de eigenaar.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut progress) }.unwrap();
        let mut db = engine.open(c"test.db").unwrap();
        let error = db.execute(c"WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000000) SELECT sum(x) FROM n").unwrap_err();
        assert_eq!(error.code & 255, 9);
        interrupt.set(false);
        assert_eq!(integer(&mut db, c"SELECT count(*) FROM item"), 2);
        integrity(&mut db);
    }

    // Een UTF-16-database moet via de Rust-API geldige UTF-8 leveren. Eerst
    // column_blob en daarna column_bytes zou de pointer tijdens conversie verliezen.
    let mut data16 = vec![0_u8; 65536];
    let mut journal16 = vec![0_u8; 65536];
    let mut utf16 = Memory::new([Slot::new(&mut data16), Slot::new(&mut journal16)], 0, 1);
    {
        // SAFETY: Alle eerdere runtimes zijn dicht; dezelfde unieke testthread.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut utf16) }.unwrap();
        let mut db = engine.open(c"utf16.db").unwrap();
        db.execute(c"PRAGMA encoding='UTF-16le'; CREATE TABLE words(text); INSERT INTO words VALUES('hé 世界')").unwrap();
        let mut s = db.prepare(c"SELECT text FROM words").unwrap();
        assert!(s.step().unwrap());
        assert_eq!(s.column(0).unwrap(), Value::Text("hé 世界"));
    }
}

struct Fault<'a, B> {
    inner: B,
    fail_at: &'a Cell<u32>,
}
impl<B: Storage> Storage for Fault<'_, B> {
    fn open(&mut self, n: &CStr, f: OpenFlags) -> Result<FileId> {
        self.inner.open(n, f)
    }
    fn close(&mut self, f: FileId) -> Result {
        self.inner.close(f)
    }
    fn read(&mut self, f: FileId, o: u64, b: &mut [u8]) -> Result<usize> {
        self.inner.read(f, o, b)
    }
    fn write(&mut self, f: FileId, o: u64, b: &[u8]) -> Result {
        self.inner.write(f, o, b)
    }
    fn truncate(&mut self, f: FileId, n: u64) -> Result {
        self.inner.truncate(f, n)
    }
    fn sync(&mut self, f: FileId, flags: i32) -> Result {
        let n = self.fail_at.get();
        self.fail_at.set(n.saturating_sub(1));
        if n == 1 {
            Err(Error { code: 1034 })
        } else {
            self.inner.sync(f, flags)
        }
    }
    fn size(&mut self, f: FileId) -> Result<u64> {
        self.inner.size(f)
    }
    fn remove(&mut self, n: &CStr, s: bool) -> Result {
        self.inner.remove(n, s)
    }
    fn exists(&mut self, n: &CStr) -> Result<bool> {
        self.inner.exists(n)
    }
    fn random(&mut self, b: &mut [u8]) -> Result {
        self.inner.random(b)
    }
    fn unix_millis(&mut self) -> Result<i64> {
        self.inner.unix_millis()
    }
}

struct Progress<'a, B> {
    inner: &'a mut B,
    interrupt: &'a Cell<bool>,
}
impl<B: Storage> Storage for Progress<'_, B> {
    fn cooperate(&mut self) -> Result {
        if self.interrupt.get() {
            Err(Error::IO)
        } else {
            Ok(())
        }
    }
    fn open(&mut self, n: &CStr, f: OpenFlags) -> Result<FileId> {
        self.inner.open(n, f)
    }
    fn close(&mut self, f: FileId) -> Result {
        self.inner.close(f)
    }
    fn read(&mut self, f: FileId, o: u64, b: &mut [u8]) -> Result<usize> {
        self.inner.read(f, o, b)
    }
    fn write(&mut self, f: FileId, o: u64, b: &[u8]) -> Result {
        self.inner.write(f, o, b)
    }
    fn truncate(&mut self, f: FileId, n: u64) -> Result {
        self.inner.truncate(f, n)
    }
    fn sync(&mut self, f: FileId, flags: i32) -> Result {
        self.inner.sync(f, flags)
    }
    fn size(&mut self, f: FileId) -> Result<u64> {
        self.inner.size(f)
    }
    fn remove(&mut self, n: &CStr, s: bool) -> Result {
        self.inner.remove(n, s)
    }
    fn exists(&mut self, n: &CStr) -> Result<bool> {
        self.inner.exists(n)
    }
    fn random(&mut self, b: &mut [u8]) -> Result {
        self.inner.random(b)
    }
    fn unix_millis(&mut self) -> Result<i64> {
        self.inner.unix_millis()
    }
}
