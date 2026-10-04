//! Een geldige maar teruggevallen remote tip mag nooit als actuele backup gelden.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error,
    local::Name,
    maintenance::Schedule,
    object,
    owner::{Config, Replica},
    prepare::Reason,
    replication, restore,
    time::Time,
};
use replica_sqlite::{OpenFlags, Storage};
use support::{Bucket, Fs};

const NS: &str = "replica/test";
fn at(seconds: i64) -> Time {
    Time::unix(1_790_765_296 + seconds, 0).unwrap()
}
fn config() -> Config {
    let mut c = Config::new(NS, &"1".repeat(64), Name::new("db").unwrap(), 100).unwrap();
    c.interval = 1;
    c.schedule = Schedule::parse("1m:1m,2m:2m").unwrap();
    c
}
fn write(r: &mut Replica, fs: &mut Fs, offset: u64, bytes: &[u8]) {
    let mut v = r.vfs(fs).unwrap();
    let id = v.open(c"db", OpenFlags(0x106)).unwrap();
    v.write(id, offset, bytes).unwrap();
    v.sync(id, 3).unwrap();
    v.close(id).unwrap();
}
fn repair_missing_metadata(compacted: bool, current: bool) {
    for restart in [false, true] {
        let mut fs = Fs::default();
        let mut store = Bucket::default();
        let mut r = Replica::prepare(&mut fs, &mut store, config(), at(0), |_, _| Ok(())).unwrap();
        let mut header = vec![0; 1024];
        header[..16].copy_from_slice(b"SQLite format 3\0");
        header[16..18].copy_from_slice(&512u16.to_be_bytes());
        write(&mut r, &mut fs, 0, &header);
        r.sync(&mut fs, &mut store, at(1)).unwrap();
        write(&mut r, &mut fs, 512, &[9; 512]);
        r.sync(&mut fs, &mut store, at(2)).unwrap();
        if compacted {
            r.sync(&mut fs, &mut store, at(600)).unwrap();
            assert!(!store.data.keys().any(|key| key.contains("/L0/")));
        }
        let old = r.marker().generation.clone();
        let prefix = replication::generation_prefix(NS, &old).unwrap();
        let key = if current {
            format!("{NS}/current")
        } else {
            store
                .data
                .keys()
                .find(|key| {
                    key.starts_with(&prefix)
                        && if compacted {
                            key.contains("/L2/") && key.ends_with("/complete")
                        } else {
                            key.contains("/L0/")
                        }
                })
                .unwrap()
                .clone()
        };
        store.data.remove(&key).unwrap();
        if restart {
            fs = fs.cold();
            r = Replica::prepare(&mut fs, &mut store, config(), at(720), |_, _| {
                panic!("the proven local database must stay authoritative")
            })
            .unwrap();
            assert_eq!(r.status().reason, Reason::Repair);
            assert!(!r.marker().complete);
        } else {
            assert!(matches!(
                r.sync(&mut fs, &mut store, at(720)),
                Err(Error::Gap | Error::Corrupt)
            ));
            assert_eq!(r.status().reason, Reason::Repair);
            assert!(!r.marker().complete);
        }
        r.sync(&mut fs, &mut store, at(721)).unwrap();
        assert_ne!(r.marker().generation, old);
        assert!(r.marker().complete && r.marker().clean);
        assert_eq!(
            store.data[&format!("{NS}/current")],
            r.marker().generation.as_bytes()
        );
        let prefix = replication::generation_prefix(NS, &r.marker().generation).unwrap();
        let layout = object::layout(&mut store, &prefix).unwrap();
        let mut restored = Fs::default();
        restore::stage(
            &mut restored,
            &mut store,
            &layout,
            None,
            Name::new("copy").unwrap(),
            100,
        )
        .unwrap()
        .verify(&mut restored, |_, _| Ok(()))
        .unwrap()
        .publish(&mut restored, &r.marker().generation)
        .unwrap();
        assert_eq!(
            restored.cold().data("copy").unwrap(),
            fs.cold().data("db").unwrap()
        );
        assert_eq!(&restored.cold().data("copy").unwrap()[512..], &[9; 512]);
    }
}
#[test]
fn missing_last_raw_manifest_triggers_a_new_snapshot() {
    repair_missing_metadata(false, false);
}
#[test]
fn missing_last_compacted_manifest_triggers_a_new_snapshot() {
    repair_missing_metadata(true, false);
}
#[test]
fn missing_current_is_repaired_from_the_proven_local_database() {
    repair_missing_metadata(false, true);
}

#[test]
fn an_unknown_repair_snapshot_commit_can_retry_while_current_still_points_to_ours() {
    for damaged in [false, true] {
        for accepted in [false, true] {
            let mut fs = Fs::default();
            let mut store = Bucket::default();
            let mut r =
                Replica::prepare(&mut fs, &mut store, config(), at(0), |_, _| Ok(())).unwrap();
            let mut header = vec![0; 1024];
            header[..16].copy_from_slice(b"SQLite format 3\0");
            header[16..18].copy_from_slice(&512u16.to_be_bytes());
            write(&mut r, &mut fs, 0, &header);
            r.sync(&mut fs, &mut store, at(1)).unwrap();
            let old = r.marker().generation.clone();
            if damaged {
                let prefix = replication::generation_prefix(NS, &old).unwrap();
                store
                    .data
                    .insert(format!("{prefix}snapshot"), b"broken".to_vec());
                assert!(matches!(
                    r.sync(&mut fs, &mut store, at(121)),
                    Err(Error::Corrupt)
                ));
            } else {
                // Een ongetrackte counter vraagt een nieuwe snapshot, maar de
                // gezonde remote generatie mag geen fallback voor deze writes zijn.
                let id = fs.open(c"db", OpenFlags(2)).unwrap();
                fs.write(id, 24, &99u32.to_be_bytes()).unwrap();
                fs.sync(id, 3).unwrap();
                fs.close(id).unwrap();
                assert!(matches!(
                    r.sync(&mut fs, &mut store, at(121)),
                    Err(Error::ForeignWrite)
                ));
            }
            store.fail_put_match = Some("snapshot".into());
            store.fail_before_put = !accepted;
            store.get_error = Some(replica_core::object::StoreError::Transport);
            assert!(r.sync(&mut fs, &mut store, at(122)).is_err());
            assert_eq!(r.marker().uncertain, 1);
            assert!(r.marker().previous.is_empty());
            assert_eq!(store.data[&format!("{NS}/current")], old.as_bytes());
            store.fail_put_match = None;
            store.fail_before_put = false;
            store.get_error = None;
            r.sync(&mut fs, &mut store, at(123)).unwrap();
            assert!(r.marker().complete && r.marker().clean);
            assert_eq!(r.marker().uncertain, 0);
            assert_ne!(r.marker().generation, old);
        }
    }
}
