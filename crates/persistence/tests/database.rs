//! Interoperabiliteit met Go en herstel na een onderbroken blobschrijfopdracht.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use replica_sqlite::{
    Engine, OpenFlags, Storage,
    memory::{Memory, Slot},
};
use spin_domain::{Timestamp, Wire};
use spin_persistence::{BLOB_CHUNK_SIZE, Database, Encrypted, Error};
use spin_security::{Cipher, Entropy};
use spin_store::{Context, Store};
struct Random(u8);
impl Entropy for Random {
    fn fill(&mut self, bytes: &mut [u8]) -> spin_security::Result {
        self.0 = self.0.wrapping_add(1);
        bytes.fill(self.0);
        Ok(())
    }
}
fn read_from<'a>(
    mut bytes: &'a [u8],
) -> impl FnMut(&mut [u8]) -> spin_persistence::Result<usize> + 'a {
    move |target| {
        let n = bytes.len().min(target.len()).min(8191);
        target[..n].copy_from_slice(&bytes[..n]);
        bytes = &bytes[n..];
        Ok(n)
    }
}
#[test]
fn go_database_rust_reopen_deduplication_chunking_and_failed_upload() {
    full_transaction_preserves_cause_and_committed_state();
    large_legacy_state_rewrite();
    // Eén test in dit proces bezit de enige SQLite-runtime; geen parallelle engines.
    let mut heap = vec![0_u64; 2 << 20];
    let mut db_bytes = vec![0; 16 << 20];
    let mut journal = vec![0; 16 << 20];
    let mut memory = Memory::new(
        [Slot::new(&mut db_bytes), Slot::new(&mut journal)],
        1_790_000_000_000,
        1,
    );
    let file = memory.open(c"spin.db", OpenFlags(6 | 0x100)).unwrap();
    memory.write(file, 0, include_bytes!("go.sqlite")).unwrap();
    memory.close(file).unwrap();
    {
        // SAFETY: Deze test is de enige SQLite-gebruiker in dit proces; de engine
        // blijft op deze thread en wordt gesloten vóór heap en VFS weer worden gebruikt.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut memory) }.unwrap();
        let mut db = Database::open(engine.open(c"spin.db").unwrap()).unwrap();
        db.quick_check().unwrap();
        let (bytes, info) = db.read_blob("fixture:a", 100).unwrap();
        assert_eq!(bytes, b"Go SQLite fixture\0\xff");
        assert_eq!(info.kind, "fixture");
        assert_eq!(db.usage().unwrap().objects, 1);
        db.delete_blob("fixture:a").unwrap();
        assert_eq!(db.read_blob("fixture:b", 100).unwrap().0, bytes);
        let mut encrypted = Encrypted::new(db, Cipher::new([7; 32]), Random(0));
        let state = encrypted
            .load(|| Err(spin_security::Error::Payload))
            .unwrap();
        assert_eq!(state.worker_token, "fixture-worker");
        assert!(
            state
                .users
                .iter()
                .any(|(_, u)| u.username == "derek" && u.password_hash == "fixture-hash")
        );
        let mut store = Store::new(state, encrypted);
        let now = Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap();
        store
            .create_recording(
                spin_domain::CreateRecordingRequest::from_json(
                    br#"{"actor":"derek","kind":"tool","name":"rust"}"#,
                )
                .unwrap(),
                Context {
                    now: &now,
                    id: "rec-rust",
                },
            )
            .unwrap();
    }
    assert!(memory.stats().syncs > 0);
    {
        // SAFETY: De eerdere engine is uit scope; exclusieve boot op dezelfde thread.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut memory) }.unwrap();
        let db = Database::open(engine.open(c"spin.db").unwrap()).unwrap();
        let mut encrypted = Encrypted::new(db, Cipher::new([7; 32]), Random(100));
        let state = encrypted
            .load(|| Err(spin_security::Error::Payload))
            .unwrap();
        assert_eq!(state.recordings.get("rec-rust").unwrap().name, "rust");
        let db = encrypted.database();
        let bytes = vec![0x5a; BLOB_CHUNK_SIZE + 31];
        let info = db
            .put_blob("large:a", "snapshot", read_from(&bytes))
            .unwrap();
        db.put_blob("large:b", "snapshot", read_from(&bytes))
            .unwrap();
        assert_eq!(db.usage().unwrap().objects, 2);
        assert_eq!(
            db.read_blob_chunk("large:a", BLOB_CHUNK_SIZE as i64)
                .unwrap()
                .0
                .len(),
            31
        );
        assert!(matches!(
            db.read_blob_chunk("large:a", 1),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            db.read_blob("large:a", 10),
            Err(Error::Invalid(_))
        ));
        assert_eq!(db.read_blob("large:a", bytes.len()).unwrap().0, bytes);
        let mut calls = 0;
        let error = db
            .put_blob("large:a", "replacement", |target| {
                calls += 1;
                if calls == 1 {
                    target.fill(42);
                    Ok(target.len())
                } else {
                    Err(Error::Invalid("injected source failure"))
                }
            })
            .unwrap_err();
        assert_eq!(error, Error::Invalid("injected source failure"));
        assert_eq!(db.blob_info("large:a").unwrap().digest, info.digest);
        db.delete_blob("large:a").unwrap();
        assert_eq!(db.read_blob("large:b", bytes.len()).unwrap().0, bytes);
        db.delete_blob("large:b").unwrap();
        assert_eq!(db.usage().unwrap().objects, 1);
        db.quick_check().unwrap();
    }
}

fn large_legacy_state_rewrite() {
    let mut heap = vec![0_u64; spin_persistence::SQLITE_HEAP_BYTES / 8];
    let mut bytes = vec![0_u8; 32 << 20];
    let mut journal = vec![0_u8; 32 << 20];
    let mut memory = Memory::new(
        [Slot::new(&mut bytes), Slot::new(&mut journal)],
        1_790_000_000_000,
        1,
    );
    let state = vec![42_u8; 10 << 20];
    {
        // SAFETY: Called sequentially before the other SQLite owners in this test.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut memory) }.unwrap();
        let mut connection = engine.open(c"legacy.db").unwrap();
        connection
            .execute(
                c"CREATE TABLE spin_kv(key TEXT PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;",
            )
            .unwrap();
        let mut db = Database::open(connection).unwrap();
        db.write_file("state", &state).unwrap();
    }
    {
        // SAFETY: The first engine has closed; this models reopening a large Go state.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut memory) }.unwrap();
        let mut db = Database::open(engine.open(c"legacy.db").unwrap()).unwrap();
        let mut loaded = db
            .read_file("state", spin_persistence::MAX_STATE_BYTES)
            .unwrap();
        assert_eq!(loaded, state);
        loaded[0] = 7;
        db.write_file("state", &loaded).unwrap();
        assert_eq!(
            db.read_file("state", spin_persistence::MAX_STATE_BYTES)
                .unwrap(),
            loaded
        );
        db.quick_check().unwrap();
    }
}

fn full_transaction_preserves_cause_and_committed_state() {
    let mut heap = vec![0_u64; (16 << 20) / 8];
    let mut bytes = vec![0_u8; 2 << 20];
    let mut journal = vec![0_u8; 2 << 20];
    let mut memory = Memory::new(
        [Slot::new(&mut bytes), Slot::new(&mut journal)],
        1_790_000_000_000,
        1,
    );
    {
        // SAFETY: This helper runs before the only other SQLite owner in this process.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut memory) }.unwrap();
        let mut db = Database::open(engine.open(c"full.db").unwrap()).unwrap();
        db.write_file("state", b"previously committed").unwrap();
        let failure = db.write_file("state", &vec![42; 3 << 20]).unwrap_err();
        assert_eq!(failure, Error::Uncertain(13));
        assert_eq!(
            db.write_file("later", b"refused"),
            Err(Error::Uncertain(13))
        );
    }
    {
        // SAFETY: The failed engine has closed; recovery has exclusive ownership.
        let mut engine = unsafe { Engine::initialize(&mut heap, &mut memory) }.unwrap();
        let mut db = Database::open(engine.open(c"full.db").unwrap()).unwrap();
        assert_eq!(db.read_file("state", 100).unwrap(), b"previously committed");
        db.quick_check().unwrap();
    }
}
