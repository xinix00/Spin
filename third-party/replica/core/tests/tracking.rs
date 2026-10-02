//! Echte SQLite-writes en fouten vóór/na iedere logpublicatiefase.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Result, dirty,
    local::{self, Name},
    tracking::{CleanMarker, Tracking},
};
use replica_sqlite::{Engine, OpenFlags, Storage, Value};
use support::Fs;
struct Marker;
impl CleanMarker<Fs> for Marker {
    fn invalidate(&mut self, b: &mut Fs) -> Result {
        local::write(b, &Name::new("db.replica").unwrap(), b"unclean")
    }
}
fn header() -> [u8; 512] {
    let mut p = [0; 512];
    p[..16].copy_from_slice(b"SQLite format 3\0");
    p[16..18].copy_from_slice(&512u16.to_be_bytes());
    p
}
fn write(t: &mut Tracking, b: &mut Fs, offset: u64, bytes: &[u8]) {
    let mut marker = Marker;
    let mut v = t.wrap(b, &mut marker);
    let id = v.open(c"db", OpenFlags(0x106)).unwrap();
    v.write(id, offset, bytes).unwrap();
    v.sync(id, 3).unwrap();
    v.close(id).unwrap();
}
#[test]
fn dirty_durable_before_database_and_marker_before_write() {
    let mut b = Fs::default();
    let mut t = Tracking::new(Name::new("db").unwrap(), 100).unwrap();
    write(&mut t, &mut b, 0, &header());
    let marker = b
        .events
        .iter()
        .position(|e| e.0 == "db.replica" && e.1 == "sync")
        .unwrap();
    let main = b
        .events
        .iter()
        .position(|e| e.0 == "db" && e.1 == "write")
        .unwrap();
    let log = b
        .events
        .iter()
        .rposition(|e| e.0 == "db.replica-dirty-a" && e.1 == "sync")
        .unwrap();
    let main_sync = b
        .events
        .iter()
        .position(|e| e.0 == "db" && e.1 == "sync")
        .unwrap();
    assert!(marker < main && log < main_sync);
    assert_eq!(t.take().unwrap(), [1]);
    t.committed(&mut b, "generation", 1, t.revision(), true)
        .unwrap();
    write(&mut t, &mut b, 512, &[7; 512]);
    let mut cold = b.cold();
    let mut restart = Tracking::new(Name::new("db").unwrap(), 100).unwrap();
    restart.recover(&mut cold, "generation", 1, 512).unwrap();
    assert_eq!(restart.take().unwrap(), [2]);
}
#[test]
fn uncertain_header_never_sends_later_appends_to_old_log() {
    for after in [false, true] {
        for fail in 1..=5 {
            let mut b = Fs::default();
            let mut t = Tracking::new(Name::new("db").unwrap(), 100).unwrap();
            write(&mut t, &mut b, 0, &header());
            t.take().unwrap();
            t.committed(&mut b, "generation", 1, t.revision(), true)
                .unwrap();
            write(&mut t, &mut b, 512, &[2; 512]);
            t.take().unwrap();
            let revision = t.revision();
            // Schrijven ná capture houdt pagina 3 over voor de logrewrite met records.
            write(&mut t, &mut b, 1024, &[3; 512]);
            b.fail = Some((b.ops + fail, after));
            let result = t.committed(&mut b, "generation", 2, revision, false);
            assert!(result.is_err());
            b.fail = None;
            write(&mut t, &mut b, 1536, &[4; 512]);
            let cold = b.cold();
            let a = cold.data("db.replica-dirty-a");
            let c = cold.data("db.replica-dirty-b");
            // Corruptie vraagt een snapshot; een geldig log mag de latere write nooit missen.
            if let Ok(log) = dirty::select(a, c, "generation", 2) {
                let pages: Vec<_> = log.pages().collect();
                assert!(
                    pages.contains(&4),
                    "phase={fail} after={after} pages={pages:?}"
                );
            }
        }
    }
}
#[test]
fn failed_marker_or_log_barrier_stops_database_publication() {
    for fail in 1..=7 {
        let mut b = Fs::default();
        let mut t = Tracking::new(Name::new("db").unwrap(), 100).unwrap();
        let mut marker = Marker;
        b.fail = Some((fail, false));
        let mut v = t.wrap(&mut b, &mut marker);
        let id = v.open(c"db", OpenFlags(0x106)).unwrap();
        let result = v.write(id, 0, &header()).and_then(|()| v.sync(id, 3));
        v.close(id).unwrap();
        assert!(result.is_err());
        assert!(!b.stable.contains_key("db"));
    }
}
#[test]
fn actual_sqlite_changes_are_recoverable_and_rollback_is_a_safe_superset() {
    let mut b = Fs::default();
    let mut t = Tracking::new(Name::new("db").unwrap(), 65536).unwrap();
    let mut marker = Marker;
    let mut heap = vec![0u64; 512 * 1024];
    {
        let mut v = t.wrap(&mut b, &mut marker);
        // SAFETY: De enige native SQLite-test in deze testbinary; exclusieve eigenaar.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut v) }.unwrap();
        let mut db = engine.open(c"db").unwrap();
        db.execute(c"CREATE TABLE item(id INTEGER PRIMARY KEY,value TEXT); BEGIN; INSERT INTO item(value) VALUES('eerste'); COMMIT; BEGIN; INSERT INTO item(value) VALUES('rollback'); ROLLBACK").unwrap();
    }
    let first = t.take().unwrap();
    assert_eq!(t.page_size(), 4096);
    assert!(first.contains(&1) && first.contains(&2));
    t.committed(&mut b, "sql", 1, t.revision(), true).unwrap();
    {
        let mut v = t.wrap(&mut b, &mut marker);
        // SAFETY: De vorige engine is gedropt, dezelfde eigenaar en heap.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut v) }.unwrap();
        let mut db = engine.open(c"db").unwrap();
        db.execute(c"INSERT INTO item(value) VALUES('tweede')")
            .unwrap();
    }
    let mut cold = b.cold();
    let mut recovered = Tracking::new(Name::new("db").unwrap(), 65536).unwrap();
    recovered.recover(&mut cold, "sql", 1, 4096).unwrap();
    let pages = recovered.take().unwrap();
    assert!(pages.contains(&1) && pages.contains(&2));
    {
        let mut v = recovered.wrap(&mut cold, &mut marker);
        // SAFETY: De vorige engine is gedropt; één native SQLite-eigenaar.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut v) }.unwrap();
        let mut db = engine.open(c"db").unwrap();
        let mut s = db.prepare(c"SELECT count(*) FROM item").unwrap();
        assert!(s.step().unwrap());
        assert_eq!(s.column(0).unwrap(), Value::Integer(2));
    }
}
#[test]
fn deleting_the_main_database_requires_stopping_its_owner() {
    let mut b = Fs::default();
    let mut t = Tracking::new(Name::new("db").unwrap(), 100).unwrap();
    write(&mut t, &mut b, 0, &header());
    let before = b.cold().data("db").unwrap().to_vec();
    let mut marker = Marker;
    let mut v = t.wrap(&mut b, &mut marker);
    assert!(v.remove(c"db", true).is_err());
    assert_eq!(b.cold().data("db").unwrap(), before);
}
