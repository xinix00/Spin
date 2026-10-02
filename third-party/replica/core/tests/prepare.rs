//! Boot mag geen bestaande lokale of remote gegevens als lege bootstrap behandelen.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error,
    local::{self, Name},
    manifest::{Manifest, Part},
    marker::{self, LocalMarker, Marker},
    object::StoreError,
    prepare::{self, Options, Reason},
    replication, segment,
    time::Time,
    tracking::Tracking,
};
use replica_sqlite::{OpenFlags, Storage};
use support::{Bucket, Fs};
const NS: &str = "replica/test";
const OLD: &str = "20260930T123456Z-old";
const NEW: &str = "20260930T143456Z-new";
fn at(n: i64) -> Time {
    Time::unix(1_790_765_296 + n, 0).unwrap()
}
fn destination() -> String {
    marker::destination("e", "b", "replica", "test").unwrap()
}
fn options(destination: &str, adopt_local: bool) -> Options<'_> {
    Options {
        namespace: NS,
        destination,
        path: Name::new("db").unwrap(),
        page_limit: 100,
        now: at(20),
        adopt_local,
    }
}
fn header() -> [u8; 512] {
    let mut h = [1; 512];
    h[..16].copy_from_slice(b"SQLite format 3\0");
    h[16..18].copy_from_slice(&512u16.to_be_bytes());
    h
}
fn remote(store: &mut Bucket, generation: &str, seq: u64, value: u8) {
    let prefix = replication::generation_prefix(NS, generation).unwrap();
    let h = header();
    let page = [value; 512];
    let pages = if seq == 1 {
        vec![(1, h.as_slice()), (2, page.as_slice())]
    } else {
        vec![(2, page.as_slice())]
    };
    let bytes = segment::encode(512, 1024, &pages).unwrap();
    let key = format!("{prefix}data/{seq}");
    let part = Part {
        key: key.clone(),
        size: bytes.len() as u64,
        hash: hop_auth::sha256(&bytes),
    };
    store.data.insert(key, bytes);
    let manifest = Manifest {
        min_size: 1024,
        first: seq,
        sequence: seq,
        at: at(seq as i64),
        level: 0,
        start: Time::ZERO,
        end: Time::ZERO,
        parts: vec![part],
    };
    let key = if seq == 1 {
        format!("{prefix}snapshot")
    } else {
        replication::raw_key(&prefix, seq, manifest.at).unwrap()
    };
    store.data.insert(key, manifest.encode(&prefix).unwrap());
    store
        .data
        .insert(format!("{NS}/current"), generation.as_bytes().to_vec());
}
fn local(fs: &mut Fs, clean: bool) -> (LocalMarker, Tracking) {
    local::write(
        fs,
        &Name::new("db").unwrap(),
        &[header().to_vec(), vec![2; 512]].concat(),
    )
    .unwrap();
    let mut marker = Marker::new(&destination(), OLD, at(0)).unwrap();
    marker.page_size = 512;
    marker.sequence = 1;
    marker.size = 1024;
    marker.at = at(1);
    marker.complete = true;
    marker.clean = clean;
    let mut marker = LocalMarker::new(Name::new("db").unwrap(), marker).unwrap();
    marker.save(fs).unwrap();
    let mut tracking = Tracking::new(Name::new("db").unwrap(), 100).unwrap();
    tracking.reset_confirmed(fs, OLD, 1, 512).unwrap();
    (marker, tracking)
}
fn write(fs: &mut Fs, marker: &mut LocalMarker, tracking: &mut Tracking, value: u8) {
    let mut v = tracking.wrap(fs, marker);
    let id = v.open(c"db", OpenFlags(2)).unwrap();
    v.write(id, 512, &[value; 512]).unwrap();
    v.sync(id, 3).unwrap();
    v.close(id).unwrap();
}
#[test]
fn empty_bootstrap_is_distinct_from_missing_current_with_archive() {
    let mut fs = Fs::default();
    let mut bucket = Bucket::default();
    let result = prepare::run(
        &mut fs,
        &mut bucket,
        options(&destination(), false),
        |_, _| panic!("bootstrap cannot restore"),
    )
    .unwrap();
    assert_eq!(result.reason, Reason::Bootstrap);
    assert!(!result.marker.value.complete);
    assert!(fs.data("db").is_none());
    let mut fs = Fs::default();
    bucket
        .data
        .insert(format!("{NS}/generations/old/data/part"), vec![1]);
    assert!(matches!(
        prepare::run(
            &mut fs,
            &mut bucket,
            options(&destination(), false),
            |_, _| Ok(())
        ),
        Err(Error::Unproven)
    ));
    assert!(fs.data("db").is_none());
}
#[test]
fn unproven_local_database_requires_explicit_adoption() {
    let mut fs = Fs::default();
    local::write(&mut fs, &Name::new("db").unwrap(), b"local truth").unwrap();
    let mut bucket = Bucket::default();
    remote(&mut bucket, OLD, 1, 2);
    assert!(matches!(
        prepare::run(
            &mut fs,
            &mut bucket,
            options(&destination(), false),
            |_, _| Ok(())
        ),
        Err(Error::Unproven)
    ));
    assert_eq!(fs.data("db").unwrap(), b"local truth");
    let prepared = prepare::run(
        &mut fs,
        &mut bucket,
        options(&destination(), true),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(prepared.reason, Reason::Bootstrap);
    assert_eq!(fs.data("db").unwrap(), b"local truth");
}
#[test]
fn remote_generation_replaces_only_a_clean_older_database() {
    for clean in [false, true] {
        let mut fs = Fs::default();
        let _ = local(&mut fs, clean);
        let mut bucket = Bucket::default();
        remote(&mut bucket, NEW, 1, 9);
        let mut verified = false;
        let result = prepare::run(
            &mut fs,
            &mut bucket,
            options(&destination(), false),
            |b, name| {
                let bytes = local::read(b, name, 1024)?;
                assert_eq!(&bytes[512..], &[9; 512]);
                verified = true;
                Ok(())
            },
        );
        if clean {
            let prepared = result.unwrap();
            assert_eq!(prepared.reason, Reason::Restored);
            assert_eq!(prepared.marker.value.generation, NEW);
            assert!(verified);
            assert_eq!(&fs.cold().data("db").unwrap()[512..], &[9; 512]);
        } else {
            assert!(matches!(result, Err(Error::Unproven)));
            assert!(!verified);
            assert_eq!(&fs.data("db").unwrap()[512..], &[2; 512]);
        }
    }
}
#[test]
fn restart_adopts_accepted_commit_and_keeps_dirty_superset() {
    let mut fs = Fs::default();
    let (mut marker, mut tracking) = local(&mut fs, true);
    write(&mut fs, &mut marker, &mut tracking, 9);
    let mut bucket = Bucket::default();
    remote(&mut bucket, OLD, 1, 2);
    remote(&mut bucket, OLD, 2, 9);
    let mut cold = fs.cold();
    let mut prepared = prepare::run(
        &mut cold,
        &mut bucket,
        options(&destination(), false),
        |_, _| panic!("must continue local lineage"),
    )
    .unwrap();
    assert_eq!(prepared.marker.value.sequence, 2);
    assert_eq!(prepared.tracking.pending().unwrap(), [2]);
    assert!(!prepared.marker.value.clean);
    write(&mut cold, &mut prepared.marker, &mut prepared.tracking, 8);
    assert!(
        !LocalMarker::load(&mut cold, Name::new("db").unwrap())
            .unwrap()
            .value
            .clean
    );
}
#[test]
fn clean_resume_invalidates_marker_before_the_next_write() {
    let mut fs = Fs::default();
    let _ = local(&mut fs, true);
    let mut bucket = Bucket::default();
    remote(&mut bucket, OLD, 1, 2);
    let mut cold = fs.cold();
    let mut prepared = prepare::run(
        &mut cold,
        &mut bucket,
        options(&destination(), false),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(prepared.reason, Reason::Continued);
    assert!(prepared.marker.value.clean);
    write(&mut cold, &mut prepared.marker, &mut prepared.tracking, 8);
    assert!(
        !LocalMarker::load(&mut cold.cold(), Name::new("db").unwrap())
            .unwrap()
            .value
            .clean
    );
}
#[test]
fn incomplete_renewal_continues_previous_with_changes() {
    let mut fs = Fs::default();
    let (mut marker, mut tracking) = local(&mut fs, true);
    replication::renew(&mut fs, &mut marker, at(20)).unwrap();
    write(&mut fs, &mut marker, &mut tracking, 8);
    let mut bucket = Bucket::default();
    remote(&mut bucket, OLD, 1, 2);
    let prepared = prepare::run(
        &mut fs.cold(),
        &mut bucket,
        options(&destination(), false),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(prepared.reason, Reason::Continued);
    assert_eq!(prepared.marker.value.generation, OLD);
    assert_eq!(prepared.tracking.pending().unwrap(), [2]);
}
#[test]
fn proven_damage_repairs_own_generation_but_transport_never_becomes_bootstrap() {
    let mut fs = Fs::default();
    let _ = local(&mut fs, false);
    let mut bucket = Bucket::default();
    remote(&mut bucket, OLD, 1, 2);
    let prefix = replication::generation_prefix(NS, OLD).unwrap();
    bucket.data.remove(&format!("{prefix}data/1"));
    let prepared = prepare::run(
        &mut fs,
        &mut bucket,
        options(&destination(), false),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(prepared.reason, Reason::Repair);
    assert_eq!(prepared.marker.value.repair_from, OLD);
    assert!(prepared.marker.value.previous.is_empty());
    bucket.get_error = Some(StoreError::Denied);
    let mut empty = Fs::default();
    assert!(matches!(
        prepare::run(
            &mut empty,
            &mut bucket,
            options(&destination(), false),
            |_, _| Ok(())
        ),
        Err(Error::Object(StoreError::Denied))
    ));
    assert!(empty.files.is_empty());
}
