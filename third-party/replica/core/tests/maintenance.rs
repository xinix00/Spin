//! Retentie behoudt een herstelpad, ook bij afgebroken opruimrondes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error,
    local::Name,
    maintenance::{self, Budget, Level, Schedule},
    manifest::{Layout, Manifest, Part},
    marker::{self, LocalMarker, Marker},
    object, replication, restore, segment,
    time::Time,
};
use support::{Bucket, Fs};
const NS: &str = "replica/test";
const GEN: &str = "20260930T120000Z-abcdef";
fn at(n: i64) -> Time {
    Time::unix(
        Time::parse("2026-09-30T12:00:00Z").unwrap().seconds() + n,
        0,
    )
    .unwrap()
}
fn manifest(store: &mut Bucket, seq: u64, t: Time, size: u64, pages: &[(u32, &[u8])]) -> Manifest {
    let prefix = replication::generation_prefix(NS, GEN).unwrap();
    let bytes = segment::encode(512, size, pages).unwrap();
    let key = format!("{prefix}data/raw{seq}");
    let part = Part {
        key: key.clone(),
        size: bytes.len() as u64,
        hash: hop_auth::sha256(&bytes),
    };
    store.data.insert(key, bytes);
    let m = Manifest {
        min_size: size,
        first: seq,
        sequence: seq,
        at: t,
        level: 0,
        start: Time::ZERO,
        end: Time::ZERO,
        parts: vec![part],
    };
    let key = if seq == 1 {
        format!("{prefix}snapshot")
    } else {
        replication::raw_key(&prefix, seq, t).unwrap()
    };
    store.data.insert(key, m.encode(&prefix).unwrap());
    m
}
fn schedule() -> Schedule {
    Schedule::new(&[
        Level {
            window: 60,
            keep: 60,
        },
        Level {
            window: 120,
            keep: 120,
        },
    ])
    .unwrap()
}
#[test]
fn schedule_validation_age_and_closed_boundaries() {
    assert_eq!(
        Schedule::parse("").unwrap().levels(),
        Schedule::defaults().unwrap().levels()
    );
    assert_eq!(
        Schedule::parse(" 1m:1m, 2m:2m ").unwrap().levels(),
        schedule().levels()
    );
    for bad in [
        "1s:1h",
        "1m:10s",
        "1m:1h,90s:1h",
        "2m:2h,1m:1h",
        "1m:1h,1m:1h",
        "60.5s:1h",
        "-1m:1h",
    ] {
        assert!(Schedule::parse(bad).is_err(), "{bad}");
    }
    let mut bucket = Bucket::default();
    let snapshot = manifest(&mut bucket, 1, at(0), 512, &[(1, &[1; 512])]);
    manifest(&mut bucket, 2, at(60), 512, &[(1, &[2; 512])]);
    let prefix = replication::generation_prefix(NS, GEN).unwrap();
    let layout = object::layout(&mut bucket, &prefix).unwrap();
    assert!(schedule().elapsed(&layout, 1, at(59)).unwrap().is_empty());
    assert_eq!(
        schedule().elapsed(&layout, 1, at(60)).unwrap(),
        [(at(0), at(60))]
    );
    assert!(schedule().elapsed(&layout, 0, at(60)).is_err());
    // Go truncate gebruikt maandag als weekanker, niet de Unix-epoch op donderdag.
    let mut m = manifest(
        &mut bucket,
        3,
        Time::parse("2026-09-30T12:00:00Z").unwrap(),
        512,
        &[(1, &[3; 512])],
    );
    m.first = 2;
    m.sequence = 2;
    let layout = Layout::new(snapshot, vec![m], &prefix).unwrap();
    let week = Schedule::new(&[Level {
        window: 604800,
        keep: 604800,
    }])
    .unwrap();
    assert_eq!(
        week.elapsed(&layout, 1, Time::parse("2026-10-05T00:00:00Z").unwrap())
            .unwrap(),
        [(
            Time::parse("2026-09-28T00:00:00Z").unwrap(),
            Time::parse("2026-10-05T00:00:00Z").unwrap()
        )]
    );
    let mut marker = Marker::new(
        &marker::destination("e", "b", "p", "d").unwrap(),
        GEN,
        at(0),
    )
    .unwrap();
    marker.complete = true;
    marker.bytes = i64::MAX as u64;
    assert!(!maintenance::renewal_due(&marker, at(120), 120).unwrap());
    assert!(maintenance::renewal_due(&marker, at(121), 120).unwrap());
    assert!(maintenance::renewal_due(&marker, Time::ZERO, 120).is_err());
}
fn fixture() -> (Fs, Bucket, LocalMarker) {
    let mut bucket = Bucket::default();
    manifest(
        &mut bucket,
        1,
        at(0),
        1536,
        &[(1, &[1; 512]), (2, &[2; 512]), (3, &[3; 512])],
    );
    manifest(&mut bucket, 2, at(1), 512, &[(1, &[9; 512])]);
    manifest(&mut bucket, 3, at(60), 1536, &[(3, &[7; 512])]);
    manifest(&mut bucket, 4, at(61), 1536, &[(2, &[6; 512])]);
    manifest(&mut bucket, 5, at(120), 1536, &[(1, &[8; 512])]);
    let mut marker = Marker::new(
        &marker::destination("e", "b", "p", "d").unwrap(),
        GEN,
        at(0),
    )
    .unwrap();
    marker.complete = true;
    marker.sequence = 5;
    marker.page_size = 512;
    marker.size = 1536;
    marker.at = at(120);
    (
        Fs::default(),
        bucket,
        LocalMarker::new(Name::new("db").unwrap(), marker).unwrap(),
    )
}
fn assert_restore(bucket: &mut Bucket) {
    let prefix = replication::generation_prefix(NS, GEN).unwrap();
    let layout = object::layout(bucket, &prefix).unwrap();
    let mut fs = Fs::default();
    restore::stage(
        &mut fs,
        bucket,
        &layout,
        None,
        Name::new("restored").unwrap(),
        100,
    )
    .unwrap()
    .verify(&mut fs, |_, _| Ok(()))
    .unwrap()
    .publish(&mut fs, GEN)
    .unwrap();
    assert_eq!(
        fs.cold().data("restored").unwrap(),
        [vec![8; 512], vec![6; 512], vec![7; 512]].concat()
    );
}
#[test]
fn multilevel_retention_and_every_failed_delete_preserve_restore() {
    for fail in 0..=12 {
        let (mut fs, mut bucket, mut marker) = fixture();
        bucket.fail_delete = if fail == 0 { None } else { Some(fail) };
        let report = maintenance::run(
            &mut fs,
            &mut bucket,
            NS,
            &mut marker,
            &schedule(),
            at(600),
            Budget {
                pages: 100,
                segment_bytes: 65536,
            },
        );
        if fail == 0 {
            assert_eq!(
                report.unwrap(),
                maintenance::Report {
                    merged: 3,
                    pruned: 6
                }
            );
        } else {
            assert!(report.is_err(), "delete {fail}");
        }
        assert_restore(&mut bucket);
        bucket.fail_delete = None;
        maintenance::run(
            &mut fs,
            &mut bucket,
            NS,
            &mut marker,
            &schedule(),
            at(600),
            Budget {
                pages: 100,
                segment_bytes: 65536,
            },
        )
        .unwrap();
        assert_restore(&mut bucket);
        let layout = object::layout(
            &mut bucket,
            &replication::generation_prefix(NS, GEN).unwrap(),
        )
        .unwrap();
        assert_eq!(layout.commits().len(), 1);
        assert_eq!(layout.commits()[0].level, 2);
    }
}
fn old_objects(bucket: &mut Bucket, id: &str, complete: bool) -> Vec<String> {
    let prefix = replication::generation_prefix(NS, id).unwrap();
    let mut keys = vec![format!("{prefix}L0/manifest"), format!("{prefix}data/part")];
    if complete {
        keys.insert(0, format!("{prefix}snapshot"));
    }
    for key in &keys {
        bucket.data.insert(key.clone(), vec![1]);
    }
    keys
}
#[test]
fn generation_gc_never_deletes_dependencies_after_failed_metadata_delete() {
    for fail in 1..=3 {
        let mut bucket = Bucket::default();
        bucket
            .data
            .insert(format!("{NS}/current"), GEN.as_bytes().to_vec());
        let keep = old_objects(&mut bucket, GEN, true);
        let old = old_objects(&mut bucket, "20260801T000000Z-old", true);
        bucket.fail_delete = Some(fail);
        assert!(maintenance::generations(&mut bucket, NS, GEN, at(0), 86400).is_err());
        assert!(keep.iter().all(|k| bucket.data.contains_key(k)));
        for key in old.iter().skip(fail - 1) {
            assert!(bucket.data.contains_key(key));
        }
        assert_eq!(bucket.deleted, old[..fail]);
        bucket.fail_delete = None;
        maintenance::generations(&mut bucket, NS, GEN, at(0), 86400).unwrap();
        assert!(old.iter().all(|k| !bucket.data.contains_key(k)));
    }
}
#[test]
fn gc_preserves_current_unknown_recent_and_complete_within_retention() {
    let mut bucket = Bucket::default();
    bucket
        .data
        .insert(format!("{NS}/current"), GEN.as_bytes().to_vec());
    let mut keep = old_objects(&mut bucket, GEN, true);
    keep.extend(old_objects(&mut bucket, "20260928T000000Z-complete", true));
    keep.extend(old_objects(
        &mut bucket,
        "20260930T110000Z-in-progress",
        false,
    ));
    let unknown = format!("{NS}/generations/unknown/data/x");
    bucket.data.insert(unknown.clone(), vec![1]);
    keep.push(unknown);
    let abandoned = old_objects(&mut bucket, "20260928T000000Z-abandoned", false);
    assert_eq!(
        maintenance::generations(&mut bucket, NS, GEN, at(0), 30 * 86400).unwrap(),
        2
    );
    assert!(abandoned.iter().all(|k| !bucket.data.contains_key(k)));
    assert!(keep.iter().all(|k| bucket.data.contains_key(k)));
    let count = bucket.deleted.len();
    assert!(matches!(
        maintenance::generations(&mut bucket, NS, "20260928T000000Z-complete", at(0), 1),
        Err(Error::Unproven)
    ));
    assert_eq!(bucket.deleted.len(), count);
}

#[test]
fn generation_gc_batches_large_archives_without_listing_live_segments() {
    use object::{Object, Store, StoreError};
    struct Large(Bucket);
    impl Store for Large {
        fn put(&mut self, k: &str, b: &[u8]) -> Result<(), StoreError> {
            self.0.put(k, b)
        }
        fn get(&mut self, k: &str, n: usize) -> Result<Vec<u8>, StoreError> {
            self.0.get(k, n)
        }
        fn delete(&mut self, k: &str) -> Result<(), StoreError> {
            self.0.delete(k)
        }
        fn list(&mut self, _: &str, _: usize) -> Result<Vec<Object>, StoreError> {
            panic!("retention must not materialize the archive")
        }
        fn directories(&mut self, prefix: &str, _: usize) -> Result<Vec<String>, StoreError> {
            Ok(vec![
                format!("{prefix}{GEN}/"),
                format!("{prefix}20260801T000000Z-old/"),
            ])
        }
        fn list_batch(&mut self, prefix: &str, limit: usize) -> Result<Vec<Object>, StoreError> {
            assert!(limit <= 128);
            Ok(self
                .0
                .data
                .keys()
                .filter(|k| k.starts_with(prefix))
                .take(limit)
                .map(|key| Object {
                    key: key.clone(),
                    size: None,
                })
                .collect())
        }
    }
    let mut bucket = Large(Bucket::default());
    bucket
        .0
        .data
        .insert(format!("{NS}/current"), GEN.as_bytes().to_vec());
    let keep = old_objects(&mut bucket.0, GEN, true);
    old_objects(&mut bucket.0, "20260801T000000Z-old", true);
    let old = replication::generation_prefix(NS, "20260801T000000Z-old").unwrap();
    for n in 0..300 {
        bucket.0.data.insert(format!("{old}data/{n:04}"), vec![1]);
    }
    let mut removed = 0;
    loop {
        let count = maintenance::generations(&mut bucket, NS, GEN, at(0), 86400).unwrap();
        assert!(count <= 128);
        assert!(keep.iter().all(|k| bucket.0.data.contains_key(k)));
        if count == 0 {
            break;
        }
        removed += count;
    }
    assert_eq!(removed, 303);
    assert_eq!(bucket.0.deleted[0], format!("{old}snapshot"));
    assert_eq!(bucket.0.deleted[1], format!("{old}L0/manifest"));
}
