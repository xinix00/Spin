//! Volledige native SQLite -> tracking -> capture -> objecten -> koude restore.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error,
    local::Name,
    marker::{self, LocalMarker, Marker},
    object::{self, StoreError},
    replication::{self, Batch},
    restore,
    time::Time,
    tracking::Tracking,
};
use replica_sqlite::{Engine, OpenFlags, Storage, Value};
use support::{Bucket, Fs};
const NS: &str = "replica/test";
fn now(n: i64) -> Time {
    Time::unix(1_790_765_296 + n, 0).unwrap()
}
fn marker() -> LocalMarker {
    LocalMarker::new(
        Name::new("db").unwrap(),
        Marker::new(
            &marker::destination("endpoint", "bucket", "replica", "test").unwrap(),
            "",
            Time::ZERO,
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn resolving_an_unknown_snapshot_requires_nonempty_ownership_proof() {
    for current in [b"".as_slice(), b"20260930T123456Z-foreign".as_slice()] {
        for proof in ["", "20260930T123456Z-ours"] {
            let mut fs = Fs::default();
            let mut store = Bucket::default();
            let mut local = marker();
            replication::renew(&mut fs, &mut local, now(0)).unwrap();
            local.value.uncertain = 1;
            local.value.repair_from = proof.into();
            local.save(&mut fs).unwrap();
            let before = local.value.duplicate().unwrap();
            let files = fs.stable.clone();
            store.data.insert(format!("{NS}/current"), current.to_vec());
            assert!(matches!(
                replication::resolve(&mut fs, &mut store, NS, &mut local),
                Err(Error::State)
            ));
            assert_eq!(local.value, before);
            assert_eq!(fs.stable, files);
        }
    }
}
#[test]
fn native_sqlite_replication_pipeline_with_concurrent_changes_and_unknown_commits() {
    let mut fs = Fs::default();
    let mut store = Bucket::default();
    let mut tracking = Tracking::new(Name::new("db").unwrap(), 65536).unwrap();
    let mut marker = marker();
    let mut heap = vec![0u64; 512 * 1024];
    let sql = |fs: &mut Fs,
               tracking: &mut Tracking,
               marker: &mut LocalMarker,
               heap: &mut [u64],
               text: &std::ffi::CStr| {
        let mut vfs = tracking.wrap(fs, marker);
        // SAFETY: De enige SQLite-test in deze binary; iedere vorige engine is gesloten.
        let mut engine = unsafe { Engine::initialize(heap, &mut vfs) }.unwrap();
        let mut db = engine.open(c"db").unwrap();
        db.execute(text).unwrap();
    };
    sql(&mut fs,&mut tracking,&mut marker,&mut heap,c"CREATE TABLE item(id INTEGER PRIMARY KEY,label TEXT); INSERT INTO item(label) VALUES('first');");
    replication::renew(&mut fs, &mut marker, now(0)).unwrap();
    Batch::capture(&mut fs, Name::new("db").unwrap(), &tracking, &marker, 65536)
        .unwrap()
        .unwrap()
        .publish(&mut fs, &mut store, NS, &mut marker, &mut tracking, now(1))
        .unwrap();
    assert!(marker.value.complete && marker.value.clean);
    assert_eq!(marker.value.sequence, 1);
    assert!(
        Batch::capture(&mut fs, Name::new("db").unwrap(), &tracking, &marker, 65536)
            .unwrap()
            .is_none()
    );
    sql(
        &mut fs,
        &mut tracking,
        &mut marker,
        &mut heap,
        c"INSERT INTO item(label) VALUES('second')",
    );
    let batch = Batch::capture(&mut fs, Name::new("db").unwrap(), &tracking, &marker, 65536)
        .unwrap()
        .unwrap();
    // SQL na capture, vóór de upload: page 2 moet opnieuw dirty blijven.
    sql(
        &mut fs,
        &mut tracking,
        &mut marker,
        &mut heap,
        c"INSERT INTO item(label) VALUES('third')",
    );
    batch
        .publish(&mut fs, &mut store, NS, &mut marker, &mut tracking, now(2))
        .unwrap();
    assert!(!marker.value.clean);
    assert!(!tracking.pending().unwrap().is_empty());
    Batch::capture(&mut fs, Name::new("db").unwrap(), &tracking, &marker, 65536)
        .unwrap()
        .unwrap()
        .publish(&mut fs, &mut store, NS, &mut marker, &mut tracking, now(3))
        .unwrap();
    assert!(marker.value.clean);
    for before in [false, true] {
        sql(
            &mut fs,
            &mut tracking,
            &mut marker,
            &mut heap,
            c"INSERT INTO item(label) VALUES('uncertain')",
        );
        let seq = marker.value.sequence;
        store.fail_put_match = Some("/L0/".into());
        store.fail_before_put = before;
        store.get_error = Some(StoreError::Transport);
        let batch = Batch::capture(&mut fs, Name::new("db").unwrap(), &tracking, &marker, 65536)
            .unwrap()
            .unwrap();
        assert!(
            batch
                .publish(
                    &mut fs,
                    &mut store,
                    NS,
                    &mut marker,
                    &mut tracking,
                    now(4 + seq as i64)
                )
                .is_err()
        );
        assert_eq!(marker.value.uncertain, seq + 1);
        assert!(
            Batch::capture(&mut fs, Name::new("db").unwrap(), &tracking, &marker, 65536).is_err()
        );
        store.fail_put_match = None;
        store.get_error = None;
        store.fail_before_put = false;
        assert_eq!(
            replication::resolve(&mut fs, &mut store, NS, &mut marker).unwrap(),
            !before
        );
        assert_eq!(marker.value.sequence, seq + u64::from(!before));
        Batch::capture(&mut fs, Name::new("db").unwrap(), &tracking, &marker, 65536)
            .unwrap()
            .unwrap()
            .publish(
                &mut fs,
                &mut store,
                NS,
                &mut marker,
                &mut tracking,
                now(20 + seq as i64),
            )
            .unwrap();
    }
    let prefix = replication::generation_prefix(NS, &marker.value.generation).unwrap();
    let layout = object::layout(&mut store, &prefix).unwrap();
    let mut restored = Fs::default();
    let staged = restore::stage(
        &mut restored,
        &mut store,
        &layout,
        None,
        Name::new("restored").unwrap(),
        65536,
    )
    .unwrap();
    let verified = staged
        .verify(&mut restored, |b, name| {
            // SAFETY: Alle schrijver-engines zijn gesloten; dezelfde unieke C-runtime.
            let mut engine = unsafe { Engine::initialize(&mut heap, b) }?;
            let mut db = engine.open(name.cstr()?)?;
            let mut q = db.prepare(c"PRAGMA integrity_check")?;
            assert!(q.step()?);
            assert_eq!(q.column(0)?, Value::Text("ok"));
            Ok(())
        })
        .unwrap();
    verified
        .publish(&mut restored, &marker.value.generation)
        .unwrap();
    let mut cold = restored.cold();
    {
        // SAFETY: Ook de verificatie-engine is gesloten; één native SQLite-eigenaar.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut cold) }.unwrap();
        let mut db = engine.open(c"restored").unwrap();
        let mut q = db.prepare(c"SELECT count(*) FROM item").unwrap();
        assert!(q.step().unwrap());
        assert_eq!(q.column(0).unwrap(), Value::Integer(5));
    }
    // Onzichtbare externe write aan de laatste gecommitteerde pagina wordt geweigerd.
    let id = fs.open(c"db", OpenFlags(2)).unwrap();
    fs.write(id, 4096 + 100, &[0xee]).unwrap();
    fs.close(id).unwrap();
    assert!(matches!(
        Batch::capture(&mut fs, Name::new("db").unwrap(), &tracking, &marker, 65536),
        Err(Error::ForeignWrite)
    ));
}
#[test]
fn go_marker_and_renewal_bytes_and_generation_identity() {
    for bytes in [
        include_bytes!("fixtures/marker.json").as_slice(),
        include_bytes!("fixtures/renewal.json").as_slice(),
    ] {
        let marker = Marker::decode(bytes).unwrap();
        assert_eq!(marker.encode().unwrap(), bytes);
        assert_eq!(marker.duplicate().unwrap(), marker);
    }
    let mut fs = Fs::default();
    let generation = marker::new_generation(&mut fs, now(0)).unwrap();
    assert_eq!(marker::generation_time(&generation).unwrap(), now(0));
    assert_eq!(generation.len(), 49);
    for bad in [
        "20260930T123456Z",
        "20260230T123456Z-x",
        "20260930T123456Z-../x",
        "20260930T123456Z-\\x",
    ] {
        assert!(marker::generation_time(bad).is_err());
    }
    let prefix = replication::generation_prefix(NS, &generation).unwrap();
    let key = replication::raw_key(&prefix, 12, now(0)).unwrap();
    assert!(key.ends_with("L0/000000000012-01790765296000000000.json"));
}
#[test]
fn bounded_snapshot_segments_and_counter_failure_never_partially_advance_marker() {
    let mut fs = Fs::default();
    let mut store = Bucket::default();
    let mut tracking = Tracking::new(Name::new("db").unwrap(), 512).unwrap();
    let mut marker = marker();
    let mut source = vec![0u8; 257 * 512];
    source[..16].copy_from_slice(b"SQLite format 3\0");
    source[16..18].copy_from_slice(&512u16.to_be_bytes());
    for i in 1..257 {
        source[i * 512..(i + 1) * 512].fill(i as u8);
    }
    {
        let mut v = tracking.wrap(&mut fs, &mut marker);
        let id = v.open(c"db", OpenFlags(0x106)).unwrap();
        v.write(id, 0, &source).unwrap();
        v.sync(id, 3).unwrap();
        v.close(id).unwrap();
    }
    replication::renew(&mut fs, &mut marker, now(0)).unwrap();
    marker.value.bytes = i64::MAX as u64;
    assert!(matches!(
        Batch::capture(&mut fs, Name::new("db").unwrap(), &tracking, &marker, 65536)
            .unwrap()
            .unwrap()
            .publish(&mut fs, &mut store, NS, &mut marker, &mut tracking, now(1)),
        Err(Error::Limit)
    ));
    assert_eq!(marker.value.sequence, 0);
    assert_eq!(marker.value.uncertain, 0);
    assert!(
        !store
            .data
            .keys()
            .any(|k| k.ends_with("snapshot") || k.ends_with("current"))
    );
    marker.value.bytes = 0;
    store.fail_put_match = Some("/current".into());
    store.get_error = Some(StoreError::Transport);
    assert!(
        Batch::capture(&mut fs, Name::new("db").unwrap(), &tracking, &marker, 65536)
            .unwrap()
            .unwrap()
            .publish(&mut fs, &mut store, NS, &mut marker, &mut tracking, now(2))
            .is_err()
    );
    marker.value.bytes = i64::MAX as u64;
    let before = marker.value.duplicate().unwrap();
    store.fail_put_match = None;
    store.get_error = None;
    assert!(matches!(
        replication::resolve(&mut fs, &mut store, NS, &mut marker),
        Err(Error::Limit)
    ));
    assert_eq!(marker.value, before);
    marker.value.bytes = 0;
    replication::resolve(&mut fs, &mut store, NS, &mut marker).unwrap();
    let prefix = replication::generation_prefix(NS, &marker.value.generation).unwrap();
    let layout = object::layout(&mut store, &prefix).unwrap();
    assert_eq!(layout.plan(None).unwrap()[0].parts.len(), 3);
    let mut target = Fs::default();
    restore::stage(
        &mut target,
        &mut store,
        &layout,
        None,
        Name::new("copy").unwrap(),
        512,
    )
    .unwrap()
    .verify(&mut target, |_, _| Ok(()))
    .unwrap()
    .publish(&mut target, &marker.value.generation)
    .unwrap();
    assert_eq!(target.cold().data("copy").unwrap(), source);
}
