//! Restore mag de werkende database pas na volledige validatie aanraken.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error,
    local::{self, Name},
    manifest::{Layout, Manifest, Part},
    object::{self, StoreError},
    restore, segment,
    time::Time,
};
use replica_sqlite::{Engine, Value};
use support::{Bucket, Fs};
const PREFIX: &str = "replica/test/gen/";
fn manifest(
    bucket: &mut Bucket,
    seq: u64,
    size: u64,
    pages: &[(u32, &[u8])],
    page_size: u32,
) -> Manifest {
    let bytes = segment::encode(page_size, size, pages).unwrap();
    let key = format!("{PREFIX}data/{seq}");
    let part = Part {
        key: key.clone(),
        size: bytes.len() as u64,
        hash: hop_auth::sha256(&bytes),
    };
    bucket.data.insert(key, bytes);
    Manifest {
        min_size: size,
        first: seq,
        sequence: seq,
        at: Time::unix(1_790_765_296 + seq as i64, 0).unwrap(),
        level: 0,
        start: Time::ZERO,
        end: Time::ZERO,
        parts: vec![part],
    }
}
fn baseline() -> (Bucket, Layout) {
    let mut bucket = Bucket::default();
    let snapshot = manifest(
        &mut bucket,
        1,
        1536,
        &[(1, &[1; 512]), (2, &[2; 512]), (3, &[3; 512])],
        512,
    );
    (bucket, Layout::new(snapshot, vec![], PREFIX).unwrap())
}
#[test]
fn invalid_late_part_and_coverage_leave_destination_untouched() {
    let dest = Name::new("db").unwrap();
    let mut fs = Fs::default();
    local::write(&mut fs, &dest, b"old database").unwrap();
    let (mut bucket, layout) = baseline();
    bucket.data.get_mut(&format!("{PREFIX}data/1")).unwrap()[100] ^= 1;
    assert!(matches!(
        restore::stage(&mut fs, &mut bucket, &layout, None, dest, 100),
        Err(Error::Corrupt)
    ));
    assert_eq!(fs.data("db").unwrap(), b"old database");
    let mut bucket = Bucket::default();
    let snapshot = manifest(
        &mut bucket,
        1,
        2048,
        &[(1, &[1; 512]), (2, &[2; 512]), (3, &[3; 512])],
        512,
    );
    let layout = Layout::new(snapshot, vec![], PREFIX).unwrap();
    assert!(matches!(
        restore::stage(&mut fs, &mut bucket, &layout, None, dest, 100),
        Err(Error::Gap)
    ));
    assert_eq!(fs.data("db").unwrap(), b"old database");
    bucket.get_error = Some(StoreError::Denied);
    assert!(matches!(
        restore::stage(&mut fs, &mut bucket, &layout, None, dest, 100),
        Err(Error::Object(StoreError::Denied))
    ));
}
#[test]
fn compacted_shrink_then_growth_does_not_resurrect_old_page() {
    let mut bucket = Bucket::default();
    let snapshot = manifest(
        &mut bucket,
        1,
        1536,
        &[(1, &[1; 512]), (2, &[2; 512]), (3, &[3; 512])],
        512,
    );
    let mut window = manifest(&mut bucket, 2, 1536, &[(1, &[9; 512]), (3, &[7; 512])], 512);
    window.min_size = 512;
    window.sequence = 3;
    window.level = 1;
    window.start = snapshot.at;
    window.end = window.at;
    let layout = Layout::new(snapshot, vec![window], PREFIX).unwrap();
    let dest = Name::new("db").unwrap();
    let mut fs = Fs::default();
    let staged = restore::stage(&mut fs, &mut bucket, &layout, None, dest, 100).unwrap();
    let verified = staged
        .verify(&mut fs, |b, name| {
            let bytes = local::read(b, name, 1536)?;
            assert_eq!(&bytes[512..1024], &[0; 512]);
            Ok(())
        })
        .unwrap();
    let result = verified.publish(&mut fs, "generation").unwrap();
    assert_eq!(result.sequence, 3);
    let cold = fs.cold();
    assert_eq!(&cold.data("db").unwrap()[512..1024], &[0; 512]);
}
#[test]
fn every_publication_fault_has_intent_or_an_intact_database() {
    for after in [false, true] {
        for step in 1..=9 {
            let (mut bucket, layout) = baseline();
            let dest = Name::new("db").unwrap();
            let mut fs = Fs::default();
            local::write(&mut fs, &dest, b"old database").unwrap();
            local::write(&mut fs, &Name::new("db-journal").unwrap(), b"stale journal").unwrap();
            let verified = restore::stage(&mut fs, &mut bucket, &layout, None, dest, 100)
                .unwrap()
                .verify(&mut fs, |_, _| Ok(()))
                .unwrap();
            fs.fail = Some((fs.ops + step, after));
            let result = verified.publish(&mut fs, "generation");
            let mut cold = fs.cold();
            let target = [vec![1; 512], vec![2; 512], vec![3; 512]].concat();
            if cold.data("db.replica-restoring").is_some() {
                // Prepare moet dit pad volgen vóór SQLite openen; dezelfde input mag opnieuw.
                restore::stage(&mut cold, &mut bucket, &layout, None, dest, 100)
                    .unwrap()
                    .verify(&mut cold, |_, _| Ok(()))
                    .unwrap()
                    .publish(&mut cold, "generation")
                    .unwrap();
                assert_eq!(cold.cold().data("db").unwrap(), target);
            } else {
                let bytes = cold.data("db").unwrap();
                assert!(
                    bytes == b"old database" || bytes == target,
                    "step={step} after={after}"
                );
                if result.is_ok() {
                    assert_eq!(bytes, target);
                }
            }
        }
    }
}
#[test]
fn lost_manifest_reply_only_counts_after_exact_readback() {
    let (mut bucket, layout) = baseline();
    let m = layout.plan(None).unwrap()[0];
    bucket.put_reply_lost = true;
    object::publish(&mut bucket, &format!("{PREFIX}snapshot"), m, PREFIX).unwrap();
    bucket.get_error = Some(StoreError::Transport);
    assert_eq!(
        object::publish(&mut bucket, &format!("{PREFIX}snapshot"), m, PREFIX),
        Err(Error::Object(StoreError::Transport))
    );
    bucket.get_error = None;
    let loaded = object::layout(&mut bucket, PREFIX).unwrap();
    assert_eq!(loaded.plan(None).unwrap()[0].sequence, 1);
    // Niet-gecommitteerde onderdelen mogen de layout niet uitbreiden.
    bucket
        .data
        .insert(format!("{PREFIX}data/orphan"), vec![99; 1024]);
    assert_eq!(
        object::layout(&mut bucket, PREFIX)
            .unwrap()
            .plan(None)
            .unwrap()
            .len(),
        1
    );
}
#[test]
fn native_sqlite_snapshot_restores_and_opens_with_integrity_check() {
    let mut source = Fs::default();
    let mut heap = vec![0u64; 512 * 1024];
    {
        // SAFETY: De enige native SQLite-test in deze testbinary; exclusieve eigenaar.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut source) }.unwrap();
        let mut db = engine.open(c"original").unwrap();
        db.execute(c"CREATE TABLE t(a INTEGER,b TEXT);INSERT INTO t VALUES(123,'HopOS native SQLite'),(456,'replica restore');").unwrap();
    }
    let bytes = source.data("original").unwrap();
    let pages: Vec<_> = bytes
        .chunks_exact(4096)
        .enumerate()
        .map(|(i, p)| (i as u32 + 1, p))
        .collect();
    let mut bucket = Bucket::default();
    let snapshot = manifest(&mut bucket, 1, bytes.len() as u64, &pages, 4096);
    let layout = Layout::new(snapshot, vec![], PREFIX).unwrap();
    let mut target = Fs::default();
    let staged = restore::stage(
        &mut target,
        &mut bucket,
        &layout,
        None,
        Name::new("restored").unwrap(),
        65536,
    )
    .unwrap();
    let verified = staged
        .verify(&mut target, |b, name| {
            // SAFETY: De bronengine is gedropt, geen andere runtime gebruikt de C-engine.
            let mut engine = unsafe { Engine::initialize(&mut heap, b) }?;
            let mut db = engine.open(name.cstr()?)?;
            let mut s = db.prepare(c"PRAGMA integrity_check")?;
            assert!(s.step()?);
            assert_eq!(s.column(0)?, Value::Text("ok"));
            Ok(())
        })
        .unwrap();
    verified.publish(&mut target, "generation").unwrap();
    let mut cold = target.cold();
    {
        // SAFETY: Ook de verificatie-engine is gesloten; de heap is vrij voor hergebruik.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut cold) }.unwrap();
        let mut db = engine.open(c"restored").unwrap();
        let mut s = db.prepare(c"SELECT sum(a),count(*) FROM t").unwrap();
        assert!(s.step().unwrap());
        assert_eq!(s.column(0).unwrap(), Value::Integer(579));
        assert_eq!(s.column(1).unwrap(), Value::Integer(2));
    }
}
