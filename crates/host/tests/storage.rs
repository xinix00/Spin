//! De echte hostbestanden overleven heropenen en een afgebroken SQLite-transactie.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use replica_sqlite::{Engine, OpenFlags, Storage};
use spin_host::storage::{Files, Random};
use spin_store::IdSource;
use std::{ffi::OsString, path::PathBuf, process::Command};

#[test]
fn durable_reopen_exclusion_and_hot_journal_recovery() {
    if let Some(root) = std::env::var_os("SPIN_VFS_CRASH_DIR") {
        let mut files = Files::open(&PathBuf::from(root)).unwrap();
        let mut heap = vec![0_u64; (64 << 20) / 8];
        // SAFETY: Dit subprocess heeft één SQLite-runtime, op deze testthread;
        // heap en files blijven geleend tot het proces zonder Drop stopt.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut files) }.unwrap();
        let mut connection = engine.open(c"spin.sqlite").unwrap();
        connection.execute(c"PRAGMA cache_size=1; BEGIN IMMEDIATE; UPDATE spin_kv SET value=zeroblob(1048576) WHERE key='proof';").unwrap();
        // Een crash voert geen destructors uit: het journal moet herstel dragen.
        std::process::exit(0);
    }
    let id = Random::open().unwrap().next("spin-vfs").unwrap();
    let root = std::env::temp_dir().join(id);
    {
        let mut files = Files::open(&root).unwrap();
        assert!(Files::open(&root).is_err());
        assert!(files.open(c"../escape", OpenFlags(6)).is_err());
        let old = files.open(c"spin.sqlite", OpenFlags(6)).unwrap();
        files.close(old).unwrap();
        let new = files.open(c"spin.sqlite", OpenFlags(6)).unwrap();
        assert_ne!(old, new);
        assert!(files.write(old, 0, b"stale").is_err());
        files.close(new).unwrap();
        let mut heap = vec![0_u64; (64 << 20) / 8];
        // SAFETY: Dit testproces heeft precies één runtime tegelijk, alle calls
        // blijven op deze thread en de opslagcallback herintreedt nooit in SQLite.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut files) }.unwrap();
        let mut db =
            spin_persistence::Database::open(engine.open(c"spin.sqlite").unwrap()).unwrap();
        db.write_file("proof", b"committed before crash").unwrap();
        db.quick_check().unwrap();
    }
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "durable_reopen_exclusion_and_hot_journal_recovery",
        ])
        .env("SPIN_VFS_CRASH_DIR", OsString::from(&root))
        .status()
        .unwrap();
    assert!(status.success());
    assert!(root.join("spin.sqlite-journal").exists());
    {
        let mut files = Files::open(&root).unwrap();
        let mut heap = vec![0_u64; (64 << 20) / 8];
        // SAFETY: De vorige runtime en het subprocess zijn beëindigd. Deze
        // nieuwe runtime blijft exclusief op de enige eigenaar-thread.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut files) }.unwrap();
        let mut db =
            spin_persistence::Database::open(engine.open(c"spin.sqlite").unwrap()).unwrap();
        assert_eq!(
            db.read_file("proof", 100).unwrap(),
            b"committed before crash"
        );
        db.quick_check().unwrap();
    }
    transactional_restore(&root);
    std::fs::remove_dir_all(root).unwrap();
}
fn transactional_restore(root: &std::path::Path) {
    let mut files = Files::open(root).unwrap();
    let mut heap = vec![0_u64; (64 << 20) / 8];
    // SAFETY: This runs after all preceding engines were dropped, on the same test thread.
    let mut engine = unsafe { Engine::initialize(&mut heap, &mut files) }.unwrap();
    {
        let mut incoming =
            spin_persistence::Database::open(engine.open(c"spin-restore.sqlite").unwrap()).unwrap();
        incoming
            .write_file("state", b"source-key-ciphertext")
            .unwrap();
        incoming
            .write_file("backup/master_key", b"source-key")
            .unwrap();
        let mut data = b"restored attachment".as_slice();
        incoming
            .put_blob("attachment:new", "job-attachment", |out| {
                let n = data.len();
                out[..n].copy_from_slice(data);
                data = &[];
                Ok(n)
            })
            .unwrap();
    }
    {
        let mut connection = engine.open(c"spin.sqlite").unwrap();
        connection.execute(c"CREATE TRIGGER reject_restore BEFORE INSERT ON spin_rows BEGIN SELECT RAISE(ABORT,'injected state rejection'); END;").unwrap();
        let mut live = spin_persistence::Database::attach(connection).unwrap();
        assert!(live.install_restore(&restored()).is_err());
        assert_eq!(
            live.read_file("proof", 100).unwrap(),
            b"committed before crash"
        );
        assert!(
            live.blob_info("attachment:new").is_err(),
            "blob replacement must roll back with the rejected state"
        );
    }
    {
        let mut connection = engine.open(c"spin.sqlite").unwrap();
        connection.execute(c"DROP TRIGGER reject_restore").unwrap();
        let mut live = spin_persistence::Database::attach(connection).unwrap();
        live.install_restore(&restored()).unwrap();
        assert_eq!(
            live.read_state(100).unwrap().unwrap(),
            (
                br#"{"worker_token":"destination-key-ciphertext"}"#.to_vec(),
                false
            )
        );
        assert!(live.read_file("proof", 100).is_err());
        assert!(live.read_file("backup/master_key", 100).is_err());
        assert_eq!(
            live.read_blob("attachment:new", 100).unwrap().0,
            b"restored attachment"
        );
        live.quick_check().unwrap();
    }
}
/// De state van de bestemming, al met haar eigen sleutel versleuteld.
fn restored() -> Vec<spin_persistence::Row> {
    vec![spin_persistence::Row {
        collection: "worker_token",
        id: String::new(),
        value: Some(r#""destination-key-ciphertext""#.into()),
    }]
}
