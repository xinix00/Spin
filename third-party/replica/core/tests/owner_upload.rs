//! SQL schrijft door terwijl de delen van een capture buiten de eigenaar uploaden.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error,
    local::Name,
    object::StoreError,
    owner::{Config, Replica},
    time::Time,
};
use replica_sqlite::Engine;
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
fn begin_and_finish_let_sql_write_while_the_parts_upload() {
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
    let pending = r
        .begin(&mut fs, &mut bucket, at(1))
        .unwrap()
        .expect("the first write is a capture");
    // Tijdens de upload: geen tweede beurt, wel SQL.
    assert!(matches!(
        r.begin(&mut fs, &mut bucket, at(20)),
        Err(Error::State)
    ));
    sql(
        &mut r,
        &mut fs,
        &mut heap,
        c"INSERT INTO item(value) VALUES('second')",
    );
    let uploaded = pending.upload(&mut fs, &mut bucket);
    assert!(uploaded.is_ok());
    let synced = r
        .finish(&mut fs, &mut bucket, pending, uploaded, at(21))
        .unwrap();
    assert!(synced.published);
    assert!(
        r.marker().complete && !r.marker().clean,
        "the write during the upload stays dirty"
    );
    // De volgende beurt neemt precies die write mee.
    let pending = r
        .begin(&mut fs, &mut bucket, at(40))
        .unwrap()
        .expect("the write during the upload is the next capture");
    let uploaded = pending.upload(&mut fs, &mut bucket);
    assert!(
        r.finish(&mut fs, &mut bucket, pending, uploaded, at(41))
            .unwrap()
            .published
    );
    assert!(r.marker().clean);
    assert!(r.begin(&mut fs, &mut bucket, at(60)).unwrap().is_none());
    // Een mislukte upload is de fout van die beurt, geen blokkade: de volgende
    // capture neemt dezelfde pagina's mee.
    sql(
        &mut r,
        &mut fs,
        &mut heap,
        c"INSERT INTO item(value) VALUES('third')",
    );
    let pending = r
        .begin(&mut fs, &mut bucket, at(80))
        .unwrap()
        .expect("third capture");
    assert!(matches!(
        r.finish(
            &mut fs,
            &mut bucket,
            pending,
            Err(StoreError::Transport.into()),
            at(81)
        ),
        Err(Error::Object(StoreError::Transport))
    ));
    assert_eq!(r.status().error, Some(Error::Object(StoreError::Transport)));
    let pending = r
        .begin(&mut fs, &mut bucket, at(100))
        .unwrap()
        .expect("the retry captures the same pages");
    let uploaded = pending.upload(&mut fs, &mut bucket);
    assert!(
        r.finish(&mut fs, &mut bucket, pending, uploaded, at(101))
            .unwrap()
            .published
    );
    assert!(r.marker().clean && r.status().error.is_none());
}
