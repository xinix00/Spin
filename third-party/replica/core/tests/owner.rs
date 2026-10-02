//! De applicatie hoeft geen losse protocolstappen te gokken: één eigenaar stuurt de keten.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error,
    archive::{self, Fetch},
    local::Name,
    object::StoreError,
    owner::{Config, Replica},
    prepare::Reason,
    replication,
    time::Time,
};
use replica_sqlite::{Engine, OpenFlags, Storage, Value};
use support::{Bucket, Fs};
fn at(n: i64) -> Time {
    Time::unix(1_790_765_296 + n, 0).unwrap()
}
fn config() -> Config {
    Config::new(
        "replica/test",
        &"1".repeat(64),
        Name::new("db").unwrap(),
        65536,
    )
    .unwrap()
}
#[test]
fn full_owner_lifecycle_with_sql_unknown_puts_external_write_and_archive_repair() {
    let mut fs = Fs::default();
    let mut bucket = Bucket::default();
    let mut heap = vec![0u64; 512 * 1024];
    let mut r = Replica::prepare(&mut fs, &mut bucket, config(), at(0), |_, _| {
        panic!("empty bootstrap needs no restore")
    })
    .unwrap();
    let sql = |r: &mut Replica, fs: &mut Fs, heap: &mut [u64], sql: &std::ffi::CStr| {
        let mut vfs = r.vfs(fs).unwrap();
        // SAFETY: Eén test met C-SQLite in deze testbinary; iedere engine sluit vóór de volgende.
        let mut engine = unsafe { Engine::initialize(heap, &mut vfs) }.unwrap();
        engine.open(c"db").unwrap().execute(sql).unwrap();
    };
    sql(&mut r,&mut fs,&mut heap,c"CREATE TABLE item(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO item(value) VALUES('first')");
    assert!(
        r.tick(&mut fs, &mut bucket, at(1))
            .unwrap()
            .unwrap()
            .published
    );
    assert!(r.marker().complete && r.marker().clean);
    let ops = fs.ops;
    assert!(r.tick(&mut fs, &mut bucket, at(2)).unwrap().is_none());
    assert_eq!(fs.ops, ops);
    sql(
        &mut r,
        &mut fs,
        &mut heap,
        c"INSERT INTO item(value) VALUES('second')",
    );
    bucket.fail_put_match = Some("/L0/".into());
    bucket.fail_before_put = true;
    assert!(matches!(
        r.tick(&mut fs, &mut bucket, at(16)),
        Err(Error::Object(StoreError::Transport))
    ));
    let generation = r.marker().generation.clone();
    assert_eq!(r.marker().uncertain, 2);
    assert_eq!(r.status().attempted, Some(at(16)));
    assert!(r.tick(&mut fs, &mut bucket, at(17)).unwrap().is_none());
    assert_eq!(r.marker().generation, generation);
    bucket.fail_put_match = None;
    bucket.fail_before_put = false;
    assert!(
        r.tick(&mut fs, &mut bucket, at(31))
            .unwrap()
            .unwrap()
            .published
    );
    assert_eq!(r.marker().sequence, 2);
    assert_eq!(r.marker().uncertain, 0);
    // Een buitenstaander verandert de SQLite change-counter: geen stille lege sync.
    let id = fs.open(c"db", OpenFlags(2)).unwrap();
    fs.write(id, 24, &99u32.to_be_bytes()).unwrap();
    fs.sync(id, 3).unwrap();
    fs.close(id).unwrap();
    assert!(matches!(
        r.tick(&mut fs, &mut bucket, at(46)),
        Err(Error::ForeignWrite)
    ));
    assert_eq!(r.status().reason, Reason::NewSnapshot);
    assert!(!r.marker().complete);
    assert!(
        r.tick(&mut fs, &mut bucket, at(61))
            .unwrap()
            .unwrap()
            .published
    );
    assert_ne!(r.marker().generation, generation);
    // Bewezen remote schade wordt voor de volgende beurt gepland, niet als netwerkretry.
    let damaged = r.marker().generation.clone();
    let prefix = replication::generation_prefix("replica/test", &damaged).unwrap();
    bucket
        .data
        .insert(format!("{prefix}snapshot"), b"broken".to_vec());
    assert!(matches!(
        r.tick(&mut fs, &mut bucket, at(121)),
        Err(Error::Corrupt)
    ));
    assert_eq!(r.status().reason, Reason::Repair);
    assert_eq!(r.marker().repair_from, damaged);
    assert!(
        r.tick(&mut fs, &mut bucket, at(136))
            .unwrap()
            .unwrap()
            .published
    );
    assert!(r.marker().repair_from.is_empty());
    let generation = r.marker().generation.clone();
    r.close();
    assert!(r.vfs(&mut fs).is_err());
    assert!(r.tick(&mut fs, &mut bucket, at(151)).is_err());
    archive::fetch(
        &mut fs,
        &mut bucket,
        Fetch {
            namespace: "replica/test",
            generation: &generation,
            at: None,
            live: Name::new("db").unwrap(),
            destination: Name::new("restored").unwrap(),
            page_limit: 65536,
        },
        |b, name| {
            // SAFETY: Alle engines hierboven zijn gedropt; dezelfde boot-eigenaar.
            let mut engine = unsafe { Engine::initialize(&mut heap, b) }?;
            let mut db = engine.open(name.cstr()?)?;
            {
                let mut q = db.prepare(c"PRAGMA integrity_check")?;
                assert!(q.step()?);
                assert_eq!(q.column(0)?, Value::Text("ok"));
            }
            let mut q = db.prepare(c"SELECT count(*) FROM item")?;
            assert!(q.step()?);
            assert_eq!(q.column(0)?, Value::Integer(2));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        fs.cold().data("restored").unwrap(),
        fs.cold().data("db").unwrap()
    );
}
#[test]
fn clock_configuration_and_closed_state_fail_before_io() {
    let mut fs = Fs::default();
    let mut bucket = Bucket::default();
    let mut bad = config();
    bad.segment_bytes = 10;
    assert!(Replica::prepare(&mut fs, &mut bucket, bad, at(0), |_, _| Ok(())).is_err());
    assert!(Replica::prepare(&mut fs, &mut bucket, config(), Time::ZERO, |_, _| Ok(())).is_err());
    assert!(fs.files.is_empty() && bucket.data.is_empty());
}
#[test]
fn uncertain_repair_marker_stops_the_owner_before_new_sql() {
    let mut fs = Fs::default();
    let mut bucket = Bucket::default();
    let mut r = Replica::prepare(&mut fs, &mut bucket, config(), at(0), |_, _| Ok(())).unwrap();
    let mut header = [0u8; 512];
    header[..16].copy_from_slice(b"SQLite format 3\0");
    header[16..18].copy_from_slice(&512u16.to_be_bytes());
    {
        let mut v = r.vfs(&mut fs).unwrap();
        let id = v.open(c"db", OpenFlags(0x106)).unwrap();
        v.write(id, 0, &header).unwrap();
        v.sync(id, 3).unwrap();
        v.close(id).unwrap();
    }
    r.sync(&mut fs, &mut bucket, at(1)).unwrap();
    let id = fs.open(c"db", OpenFlags(2)).unwrap();
    fs.write(id, 24, &42u32.to_be_bytes()).unwrap();
    fs.close(id).unwrap();
    fs.fail = Some((fs.ops + 1, true));
    assert!(matches!(
        r.sync(&mut fs, &mut bucket, at(16)),
        Err(Error::Storage(_))
    ));
    fs.fail = None;
    assert!(r.vfs(&mut fs).is_err());
    assert!(r.tick(&mut fs, &mut bucket, at(31)).is_err());
}
