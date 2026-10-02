//! Zelfde shrink/grow-uitvoer als Go; iedere onderbroken prune blijft herstelbaar.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    compact::{self, Window},
    local::Name,
    manifest::{Manifest, Part},
    marker::{self, LocalMarker, Marker},
    object, replication, restore, segment,
    time::Time,
};
use support::{Bucket, Fs};
const NS: &str = "replica/test";
const GENERATION: &str = "20260930T123456Z-abcdef";
fn at(n: i64) -> Time {
    Time::unix(1_790_765_296 + n, 0).unwrap()
}
fn part(store: &mut Bucket, prefix: &str, seq: u64, size: u64, pages: &[(u32, &[u8])]) -> Manifest {
    let bytes = segment::encode(512, size, pages).unwrap();
    let key = format!("{prefix}data/raw{seq}");
    let p = Part {
        key: key.clone(),
        size: bytes.len() as u64,
        hash: hop_auth::sha256(&bytes),
    };
    store.data.insert(key, bytes);
    Manifest {
        min_size: size,
        first: seq,
        sequence: seq,
        at: at(seq as i64 - 1),
        level: 0,
        start: Time::ZERO,
        end: Time::ZERO,
        parts: vec![p],
    }
}
fn fixture() -> (Fs, Bucket, LocalMarker, Manifest, Manifest, String, String) {
    let fs = Fs::default();
    let mut store = Bucket::default();
    let prefix = replication::generation_prefix(NS, GENERATION).unwrap();
    let snapshot = part(
        &mut store,
        &prefix,
        1,
        1536,
        &[(1, &[1; 512]), (2, &[2; 512]), (3, &[3; 512])],
    );
    let m2 = part(&mut store, &prefix, 2, 512, &[(1, &[9; 512])]);
    let m3 = part(&mut store, &prefix, 3, 1536, &[(3, &[7; 512])]);
    let key2 = replication::raw_key(&prefix, 2, m2.at).unwrap();
    let key3 = replication::raw_key(&prefix, 3, m3.at).unwrap();
    store.data.insert(
        format!("{prefix}snapshot"),
        snapshot.encode(&prefix).unwrap(),
    );
    store.data.insert(key2.clone(), m2.encode(&prefix).unwrap());
    store.data.insert(key3.clone(), m3.encode(&prefix).unwrap());
    let mut marker = Marker::new(
        &marker::destination("e", "b", "replica", "test").unwrap(),
        GENERATION,
        at(0),
    )
    .unwrap();
    marker.complete = true;
    marker.sequence = 3;
    marker.page_size = 512;
    marker.size = 1536;
    marker.at = at(2);
    let local = LocalMarker::new(Name::new("db").unwrap(), marker).unwrap();
    (fs, store, local, m2, m3, key2, key3)
}
fn window() -> Window {
    Window {
        level: 1,
        start: at(0),
        end: at(10),
        page_limit: 100,
        segment_bytes: 65536,
    }
}
#[test]
fn merged_segment_matches_go_and_retains_truncation_history() {
    let (mut fs, mut store, mut marker, m2, m3, _, _) = fixture();
    let merged =
        compact::merge(&mut fs, &mut store, NS, &mut marker, window(), &[&m2, &m3]).unwrap();
    assert_eq!(merged.min_size, 512);
    assert_eq!(merged.sequence, 3);
    assert_eq!(merged.parts.len(), 1);
    assert_eq!(
        store.data[&merged.parts[0].key],
        include_bytes!("fixtures/compact.bin")
    );
    assert_eq!(
        LocalMarker::load(&mut fs.cold(), Name::new("db").unwrap())
            .unwrap()
            .value
            .sealed_at,
        at(10)
    );
}
#[test]
fn gap_or_unpublished_replacement_never_removes_raw_history() {
    let (mut fs, mut store, mut marker, m2, mut m3, key2, key3) = fixture();
    m3.first = 4;
    m3.sequence = 4;
    assert!(compact::merge(&mut fs, &mut store, NS, &mut marker, window(), &[&m2, &m3]).is_err());
    assert!(store.data.contains_key(&key2) && store.data.contains_key(&key3));
    m3.first = 3;
    m3.sequence = 3;
    store.fail_put_match = Some("/complete".into());
    store.fail_before_put = true;
    assert!(compact::merge(&mut fs, &mut store, NS, &mut marker, window(), &[&m2, &m3]).is_err());
    assert!(store.data.contains_key(&key2) && store.data.contains_key(&key3));
    assert_eq!(marker.value.sealed_at, at(10));
}
#[test]
fn failed_delete_always_restores_through_confirmed_window() {
    for fail in 1..=4 {
        let (mut fs, mut store, mut marker, m2, m3, key2, key3) = fixture();
        let merged =
            compact::merge(&mut fs, &mut store, NS, &mut marker, window(), &[&m2, &m3]).unwrap();
        store.fail_delete = Some(fail);
        let _ = compact::prune(&mut store, &key2, &m2, &merged, &[&merged, &m3]);
        let _ = compact::prune(&mut store, &key3, &m3, &merged, &[&merged]);
        let prefix = replication::generation_prefix(NS, GENERATION).unwrap();
        let layout = object::layout(&mut store, &prefix).unwrap();
        let mut target = Fs::default();
        let verified = restore::stage(
            &mut target,
            &mut store,
            &layout,
            None,
            Name::new("restored").unwrap(),
            100,
        )
        .unwrap()
        .verify(&mut target, |_, _| Ok(()))
        .unwrap();
        verified.publish(&mut target, GENERATION).unwrap();
        assert_eq!(
            target.cold().data("restored").unwrap(),
            [vec![9; 512], vec![0; 512], vec![7; 512]].concat()
        );
    }
}
