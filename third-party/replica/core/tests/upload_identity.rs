//! Een uploadresultaat mag alleen zijn eigen capture bevestigen.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error,
    local::Name,
    owner::{Config, Replica},
    time::Time,
};
use replica_sqlite::Engine;
use support::{Bucket, Fs};

fn at(seconds: i64) -> Time {
    Time::unix(1_790_765_296 + seconds, 0).unwrap()
}

#[test]
fn stale_uploads_are_rejected_without_acknowledging_new_writes() {
    let mut fs = Fs::default();
    let mut store = Bucket::default();
    let config = Config::new(
        "replica/test",
        &"1".repeat(64),
        Name::new("db").unwrap(),
        65536,
    )
    .unwrap();
    let mut replica = Replica::prepare(&mut fs, &mut store, config, at(0), |_, _| {
        panic!("empty bootstrap needs no restore")
    })
    .unwrap();
    let mut heap = vec![0u64; 512 * 1024];
    let sql = |replica: &mut Replica, fs: &mut Fs, heap: &mut [u64], text: &std::ffi::CStr| {
        let mut vfs = replica.vfs(fs).unwrap();
        // SAFETY: De enige SQLite-test in deze binary; elke engine sluit vóór de volgende.
        let mut engine = unsafe { Engine::initialize(heap, &mut vfs) }.unwrap();
        engine.open(c"db").unwrap().execute(text).unwrap();
    };
    sql(
        &mut replica,
        &mut fs,
        &mut heap,
        c"CREATE TABLE item(value TEXT); INSERT INTO item VALUES('first')",
    );
    let first = replica.begin(&mut fs, &mut store, at(1)).unwrap().unwrap();
    let stale = first.upload(&mut fs, &mut store).unwrap();
    let uploaded = first.upload(&mut fs, &mut store);
    replica
        .finish(&mut fs, &mut store, first, uploaded, at(2))
        .unwrap();

    sql(
        &mut replica,
        &mut fs,
        &mut heap,
        c"INSERT INTO item VALUES('second')",
    );
    let second = replica.begin(&mut fs, &mut store, at(20)).unwrap().unwrap();
    let abandoned = second.upload(&mut fs, &mut store).unwrap();
    let marker = replica.marker().encode().unwrap();
    let objects = store.data.clone();
    let files = fs.stable.clone();
    assert!(matches!(
        replica.finish(&mut fs, &mut store, second, Ok(stale), at(21)),
        Err(Error::State)
    ));
    assert_eq!(replica.marker().encode().unwrap(), marker);
    assert_eq!(store.data, objects);
    assert_eq!(fs.stable, files);
    assert!(!replica.marker().clean);

    // Ook een nieuwe capture met dezelfde generatie en voorganger mag een
    // resultaat van een afgebroken capture niet overnemen.
    let retry = replica.begin(&mut fs, &mut store, at(40)).unwrap().unwrap();
    assert!(matches!(
        replica.finish(&mut fs, &mut store, retry, Ok(abandoned), at(41)),
        Err(Error::State)
    ));
    assert_eq!(replica.marker().encode().unwrap(), marker);
    assert_eq!(store.data, objects);
    assert_eq!(fs.stable, files);

    let retry = replica.begin(&mut fs, &mut store, at(60)).unwrap().unwrap();
    let uploaded = retry.upload(&mut fs, &mut store);
    assert!(
        replica
            .finish(&mut fs, &mut store, retry, uploaded, at(61))
            .unwrap()
            .published
    );
    assert_eq!(replica.marker().sequence, 2);
    assert!(replica.marker().clean);
    assert!(
        replica
            .begin(&mut fs, &mut store, at(80))
            .unwrap()
            .is_none()
    );
}
