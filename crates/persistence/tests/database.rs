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
    state_rows_write_only_what_changed();
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
        // Verwijderen markeert; de opruiming gaat in begrensde stappen.
        assert!(
            db.purge_step(1).unwrap(),
            "het eerste stuk ging weg, er ligt meer"
        );
        while db.purge_step(1).unwrap() {}
        assert!(!db.purge_step(1).unwrap());
        assert_eq!(
            db.read_blob("fixture:b", 100).unwrap().0,
            b"Go SQLite fixture\0\xff"
        );
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

fn state_rows_write_only_what_changed() {
    use spin_domain::{TryClone, state::PersistedState};
    use spin_persistence::state_rows;
    let cipher = Cipher::new([7; 32]);
    let state = PersistedState::from_json(
        br#"{"jobs":{"a":{"id":"a","title":"A"},"b":{"id":"b"}},"git_accounts":{"g":{"id":"g","access_token":"secret-access"}},"worker_token":"secret-worker"}"#,
    )
    .unwrap();
    let all = state_rows(&cipher, &mut Random(0), &state, None).unwrap();
    // Twee jobs, een account, worker_token en garbage_refs.
    assert_eq!(all.len(), 5);
    assert!(
        all.iter()
            .all(|r| !r.value.as_deref().unwrap_or("").contains("secret-")),
        "geheimen gaan alleen versleuteld in een rij"
    );
    let mut next = state.try_clone().unwrap();
    next.jobs.get_mut("a").unwrap().title = "Gewijzigd".into();
    next.jobs.remove("b");
    let changes = spin_store::diff(&state, &next).unwrap();
    let rows = state_rows(&cipher, &mut Random(10), &next, Some(&changes)).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r.id == "a" && r.value.is_some()));
    assert!(rows.iter().any(|r| r.id == "b" && r.value.is_none()));

    let mut heap = vec![0_u64; 2 << 20];
    let mut db_bytes = vec![0; 4 << 20];
    let mut journal = vec![0; 4 << 20];
    let mut memory = Memory::new(
        [Slot::new(&mut db_bytes), Slot::new(&mut journal)],
        1_790_000_000_000,
        1,
    );
    // SAFETY: Sequentieel vóór de andere SQLite-eigenaars in dit proces.
    let mut engine = unsafe { Engine::initialize(&mut heap, &mut memory) }.unwrap();
    let mut db = Database::open(engine.open(c"rows.db").unwrap()).unwrap();
    db.write_file("state", b"oude enkele rij").unwrap();
    db.replace_rows(&all).unwrap();
    db.write_rows(&rows).unwrap();
    let (bytes, legacy) = db
        .read_state(spin_persistence::MAX_STATE_BYTES)
        .unwrap()
        .unwrap();
    assert!(!legacy);
    assert!(db.read_file("state", 100).is_err(), "de oude rij is weg");
    let sealed = PersistedState::from_json(&bytes).unwrap();
    let loaded = cipher
        .decrypt_state(&sealed, || Err(spin_security::Error::Payload))
        .unwrap();
    assert_eq!(loaded.jobs.get("a").unwrap().title, "Gewijzigd");
    assert!(loaded.jobs.get("b").is_none());
    assert_eq!(
        loaded.git_accounts.get("g").unwrap().access_token,
        "secret-access"
    );
    assert_eq!(loaded.worker_token, "secret-worker");
}
