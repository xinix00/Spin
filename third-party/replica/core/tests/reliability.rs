//! Fouten vóór/na iedere publicatiestap, gevolgd door retry en koude restore.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error, Result,
    local::Name,
    object::{self, Object, Store, StoreError},
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
    c.segment_bytes = 65536;
    c.interval = 1;
    c
}
fn sql(r: &mut Replica, fs: &mut Fs, heap: &mut [u64], text: &CStr) {
    let mut vfs = r.vfs(fs).unwrap();
    // SAFETY: Alle SQLite-calls in deze binary lopen in één test, engines sluiten per call.
    let mut engine = unsafe { Engine::initialize(heap, &mut vfs) }.unwrap();
    engine.open(c"db").unwrap().execute(text).unwrap();
}
fn verify(fs: &mut Fs, name: &Name, heap: &mut [u64]) -> Result {
    // SAFETY: De vorige engine is gesloten; één test bezit de globale SQLite-runtime.
    let mut engine = unsafe { Engine::initialize(heap, fs) }?;
    let mut db = engine.open(name.cstr()?)?;
    let mut q = db.prepare(c"PRAGMA integrity_check")?;
    assert!(q.step()?);
    assert_eq!(q.column(0)?, Value::Text("ok"));
    Ok(())
}
fn prepare(fs: &mut Fs, store: &mut FaultStore, heap: &mut [u64], time: i64) -> Replica {
    Replica::prepare(fs, store, config(), at(time), |b, name| {
        verify(b, name, heap)
    })
    .unwrap()
}
fn assert_restore(fs: &Fs, store: &mut FaultStore, r: &Replica, heap: &mut [u64]) {
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
    .verify(&mut target, |b, name| verify(b, name, heap))
    .unwrap()
    .publish(&mut target, &r.marker().generation)
    .unwrap();
    assert_eq!(
        target.cold().data("copy").unwrap(),
        fs.cold().data("db").unwrap()
    );
}

#[derive(Clone, Default)]
struct FaultStore {
    bucket: Bucket,
    calls: Vec<(&'static str, String)>,
    fault: Option<(usize, bool)>,
}
impl FaultStore {
    fn call<T>(
        &mut self,
        op: &'static str,
        key: &str,
        f: impl FnOnce(&mut Bucket) -> core::result::Result<T, StoreError>,
    ) -> core::result::Result<T, StoreError> {
        self.calls.push((op, key.into()));
        let fault = self.fault.filter(|(n, _)| *n == self.calls.len());
        if fault == Some((self.calls.len(), false)) {
            return Err(StoreError::Transport);
        }
        let result = f(&mut self.bucket);
        if fault.is_some() {
            Err(StoreError::Transport)
        } else {
            result
        }
    }
}
impl Store for FaultStore {
    fn put(&mut self, key: &str, bytes: &[u8]) -> core::result::Result<(), StoreError> {
        self.call("put", key, |b| b.put(key, bytes))
    }
    fn get(&mut self, key: &str, limit: usize) -> core::result::Result<Vec<u8>, StoreError> {
        self.call("get", key, |b| b.get(key, limit))
    }
    fn list(
        &mut self,
        prefix: &str,
        limit: usize,
    ) -> core::result::Result<Vec<Object>, StoreError> {
        self.call("list", prefix, |b| b.list(prefix, limit))
    }
    fn delete(&mut self, key: &str) -> core::result::Result<(), StoreError> {
        self.call("delete", key, |b| b.delete(key))
    }
}

#[test]
fn every_local_and_remote_publication_fault_preserves_committed_sql() {
    let mut heap = vec![0u64; 512 * 1024];
    let mut source = Fs::default();
    let mut bucket = FaultStore::default();
    let mut owner = prepare(&mut source, &mut bucket, &mut heap, 0);
    sql(&mut owner, &mut source, &mut heap,
        c"CREATE TABLE item(id INTEGER PRIMARY KEY, value BLOB); WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<24) INSERT INTO item SELECT x, zeroblob(2048) FROM n");
    // Snapshot, increment en generatievernieuwing krijgen dezelfde foutmatrix.
    for phase in 0..3 {
        if phase != 0 {
            owner
                .sync(&mut source, &mut bucket, at(phase * 10))
                .unwrap();
            sql(
                &mut owner,
                &mut source,
                &mut heap,
                c"UPDATE item SET value=randomblob(2048); DELETE FROM item WHERE id%7=0",
            );
        }
        let clock = if phase == 2 { 8 * 86400 } else { 30 + phase };
        let mut baseline_fs = source.cold();
        let mut baseline_store = bucket.clone();
        let mut baseline = prepare(&mut baseline_fs, &mut baseline_store, &mut heap, clock);
        let local_start = baseline_fs.ops;
        let remote_start = baseline_store.calls.len();
        baseline
            .sync(&mut baseline_fs, &mut baseline_store, at(clock + 1))
            .unwrap();
        let local_steps = baseline_fs.ops - local_start;
        let remote_steps = baseline_store.calls.len() - remote_start;
        assert!(
            baseline_store.calls[remote_start..]
                .iter()
                .filter(|(op, key)| *op == "put" && key.ends_with(".seg"))
                .count()
                >= 2
        );
        assert_restore(&baseline_fs, &mut baseline_store, &baseline, &mut heap);

        for remote in [false, true] {
            let steps = if remote { remote_steps } else { local_steps };
            for step in 1..=steps {
                for after in [false, true] {
                    for restart in [false, true] {
                        let mut fs = source.cold();
                        let mut store = bucket.clone();
                        let mut r = prepare(&mut fs, &mut store, &mut heap, clock);
                        if remote {
                            store.fault = Some((store.calls.len() + step, after));
                        } else {
                            fs.fail = Some((fs.ops + step, after));
                        }
                        let _ = r.sync(&mut fs, &mut store, at(clock + 1));
                        fs.fail = None;
                        store.fault = None;
                        if restart {
                            fs = fs.cold();
                            r = prepare(&mut fs, &mut store, &mut heap, clock + 2);
                        }
                        // Een fout kan een nieuwe snapshot plannen. Hoogstens
                        // één volgende beurt voor de expliciete herstelbeslissing.
                        let retry = r.sync(&mut fs, &mut store, at(clock + 3));
                        if matches!(
                            retry,
                            Err(Error::State | Error::ForeignWrite | Error::Gap | Error::Corrupt)
                        ) {
                            r.sync(&mut fs, &mut store, at(clock + 4)).unwrap();
                        } else {
                            retry.unwrap_or_else(|e| panic!("phase={phase} remote={remote} step={step} after={after} restart={restart}: {e:?}"));
                        }
                        assert!(r.marker().complete && r.marker().uncertain == 0);
                        assert_eq!(
                            fs.cold().data("db").unwrap(),
                            source.cold().data("db").unwrap(),
                            "source changed: phase={phase} remote={remote} step={step} after={after} restart={restart}"
                        );
                        assert_restore(&fs, &mut store, &r, &mut heap);
                    }
                }
            }
        }
    }
}
