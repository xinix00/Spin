//! Echte SQL, overlappende capture/writes, fouten, herstarts en exacte restores.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error, Result,
    local::Name,
    maintenance::Schedule,
    object::{self, StoreError},
    owner::{Config, Replica},
    replication, restore,
    time::Time,
};
use replica_sqlite::{Engine, Value};
use std::ffi::CStr;
use support::{Bucket, Fs};

const NS: &str = "replica/test";
fn at(seconds: i64) -> Time {
    Time::unix(1_790_765_296 + seconds, 0).unwrap()
}
fn config() -> Config {
    let mut c = Config::new(NS, &"1".repeat(64), Name::new("db").unwrap(), 65536).unwrap();
    c.interval = 1;
    c.segment_bytes = 65536;
    c.generation = 600;
    c.retention = 1200;
    c.schedule = Schedule::parse("1m:2m,2m:4m").unwrap();
    c
}
fn sql(r: &mut Replica, fs: &mut Fs, heap: &mut [u64], text: &CStr) {
    let mut vfs = r.vfs(fs).unwrap();
    // SAFETY: Eén test bezit de SQLite-runtime; elke engine sluit vóór de volgende call.
    let mut engine = unsafe { Engine::initialize(heap, &mut vfs) }.unwrap();
    engine.open(c"db").unwrap().execute(text).unwrap();
}
fn integrity(fs: &mut Fs, name: &Name, heap: &mut [u64]) -> Result {
    // SAFETY: Alle eerdere engines zijn gesloten; dezelfde exclusieve test-eigenaar.
    let mut engine = unsafe { Engine::initialize(heap, fs) }?;
    let mut db = engine.open(name.cstr()?)?;
    let mut q = db.prepare(c"PRAGMA integrity_check")?;
    assert!(q.step()?);
    assert_eq!(q.column(0)?, Value::Text("ok"));
    Ok(())
}
fn prepare(fs: &mut Fs, store: &mut Bucket, heap: &mut [u64], time: i64) -> Replica {
    Replica::prepare(fs, store, config(), at(time), |b, name| {
        integrity(b, name, heap)
    })
    .unwrap()
}
fn restore_exact(fs: &Fs, store: &mut Bucket, r: &Replica, heap: &mut [u64]) {
    let prefix = replication::generation_prefix(NS, &r.marker().generation).unwrap();
    let layout = object::layout(store, &prefix).unwrap();
    let mut target = Fs::default();
    restore::stage(
        &mut target,
        store,
        &layout,
        None,
        Name::new("copy").unwrap(),
        65536,
    )
    .unwrap()
    .verify(&mut target, |b, name| integrity(b, name, heap))
    .unwrap()
    .publish(&mut target, &r.marker().generation)
    .unwrap();
    assert_eq!(
        target.cold().data("copy").unwrap(),
        fs.cold().data("db").unwrap()
    );
}
fn catch_up(r: &mut Replica, fs: &mut Fs, store: &mut Bucket, time: i64) {
    let result = r.sync(fs, store, at(time));
    if matches!(
        result,
        Err(Error::State | Error::ForeignWrite | Error::Gap | Error::Corrupt)
    ) {
        r.sync(fs, store, at(time + 1)).unwrap_or_else(|e| {
            panic!(
                "retry time={time}, first={result:?}, error={e:?}, marker={:?}, status={:?}",
                r.marker(),
                r.status()
            )
        });
    } else {
        result.unwrap();
    }
}

#[test]
fn sql_and_upload_failures_never_lose_commits_across_restart_and_compaction() {
    let mut heap = vec![0u64; 512 * 1024];
    for offset in [0, 17, 59] {
        let mut fs = Fs::default();
        let mut store = Bucket::default();
        let mut r = prepare(&mut fs, &mut store, &mut heap, offset);
        sql(&mut r, &mut fs, &mut heap,
            c"CREATE TABLE item(id INTEGER PRIMARY KEY, value BLOB); WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<24) INSERT INTO item SELECT x, zeroblob(2048) FROM n");
        catch_up(&mut r, &mut fs, &mut store, offset + 1);
        for cycle in 0..30 {
            let now = offset + 20 + cycle * 65;
            match cycle % 5 {
                0 => sql(&mut r, &mut fs, &mut heap,
                    c"BEGIN; UPDATE item SET value=randomblob(2048); INSERT INTO item(value) VALUES(zeroblob(8192)); ROLLBACK; UPDATE item SET value=randomblob(2048) WHERE id%3=0"),
                1 => sql(&mut r, &mut fs, &mut heap,
                    c"DELETE FROM item WHERE id%2=0; VACUUM"),
                2 => sql(&mut r, &mut fs, &mut heap,
                    c"WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<16) INSERT INTO item(value) SELECT zeroblob(2048) FROM n"),
                3 => sql(&mut r, &mut fs, &mut heap, c"PRAGMA page_size=512; VACUUM; UPDATE item SET value=randomblob(512)"),
                _ => sql(&mut r, &mut fs, &mut heap, c"PRAGMA page_size=4096; VACUUM; UPDATE item SET value=randomblob(2048)"),
            }
            let started = r.begin(&mut fs, &mut store, at(now));
            let pending = match started {
                Ok(Some(pending)) => pending,
                Err(Error::State | Error::ForeignWrite | Error::Gap) => {
                    r.begin(&mut fs, &mut store, at(now + 1)).unwrap().unwrap()
                }
                other => panic!(
                    "cycle={cycle} begin: {}",
                    if other.is_ok() {
                        "no capture"
                    } else {
                        "unexpected error"
                    }
                ),
            };
            // Zowel writes na capture als krimp/groei vóór de volgende capture.
            sql(&mut r, &mut fs, &mut heap,
                c"INSERT INTO item(value) VALUES(zeroblob(1024)); UPDATE item SET value=randomblob(1024) WHERE id%5=0");
            match cycle % 4 {
                0 => {
                    fs.files
                        .iter_mut()
                        .find(|e| e.name == "db.replica-capture")
                        .unwrap()
                        .data[100] ^= 1;
                }
                1 => {
                    store.fail_put_match = Some("/data/".into());
                    store.fail_before_put = cycle % 8 == 1;
                }
                2 => {
                    store.fail_put_match = Some(
                        if r.marker().complete {
                            "/L0/"
                        } else {
                            "snapshot"
                        }
                        .into(),
                    );
                    store.get_error = Some(StoreError::Transport);
                }
                _ => {}
            }
            let uploaded = pending.upload(&mut fs, &mut store);
            let _ = r.finish(&mut fs, &mut store, pending, uploaded, at(now + 2));
            store.fail_put_match = None;
            store.fail_before_put = false;
            store.get_error = None;
            let committed = fs.cold().data("db").unwrap().to_vec();
            if cycle % 3 != 0 {
                fs = fs.cold();
                r = prepare(&mut fs, &mut store, &mut heap, now + 3);
                assert_eq!(fs.cold().data("db").unwrap(), committed);
            }
            catch_up(&mut r, &mut fs, &mut store, now + 4);
            assert!(r.marker().complete && r.marker().clean && r.marker().uncertain == 0);
            restore_exact(&fs, &mut store, &r, &mut heap);
        }
        // Restore -> opnieuw schrijven -> opnieuw repliceren, zoals Litestream's
        // RestoreAndReplicateAfterDataLoss-regressie, zonder de oude lokale logs.
        let expected = fs.cold().data("db").unwrap().to_vec();
        let mut empty = Fs::default();
        r = prepare(&mut empty, &mut store, &mut heap, offset + 2100);
        assert_eq!(empty.cold().data("db").unwrap(), expected);
        sql(
            &mut r,
            &mut empty,
            &mut heap,
            c"INSERT INTO item(value) VALUES('after restore')",
        );
        catch_up(&mut r, &mut empty, &mut store, offset + 2101);
        restore_exact(&empty, &mut store, &r, &mut heap);
    }
}
