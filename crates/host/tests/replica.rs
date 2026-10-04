//! De gerepliceerde hosteigenaar publiceert naar een objectstore en een lege
//! directory herstelt daaruit; een geheugenbucket vervangt S3.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use replica_core::writer::{Backend, LeaseState, MemBackend, discovery};
use replica_core::{
    local::Name,
    object::{Object, Store, StoreError},
    owner::Config,
};
use spin_domain::json::Value;
use spin_host::{
    replica::{Leased, Owner, lease_key, prepare},
    storage::{Files, Random},
};
use spin_security::Cipher;
use spin_store::Persistence;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

#[derive(Clone, Default)]
struct Memory(
    Rc<RefCell<BTreeMap<String, Vec<u8>>>>,
    Rc<RefCell<MemBackend>>,
);
struct MemLease(Rc<RefCell<MemBackend>>);
impl Backend for MemLease {
    fn read(&mut self) -> discovery::Result<(LeaseState, String)> {
        self.0.borrow_mut().read()
    }
    fn write(&mut self, prev: &str, state: &LeaseState) -> discovery::Result<String> {
        self.0.borrow_mut().write(prev, state)
    }
    fn delete(&mut self, handle: &str) -> discovery::Result {
        self.0.borrow_mut().delete(handle)
    }
}
impl Leased for Memory {
    type Lease<'a> = MemLease;
    fn lease<'a>(&'a mut self, _: &'a str, _: u64) -> MemLease {
        MemLease(self.1.clone())
    }
}
impl Store for Memory {
    fn put(&mut self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.0.borrow_mut().insert(key.to_owned(), bytes.to_vec());
        Ok(())
    }
    fn get(&mut self, key: &str, limit: usize) -> Result<Vec<u8>, StoreError> {
        match self.0.borrow().get(key) {
            Some(bytes) if bytes.len() > limit => Err(StoreError::Limit),
            Some(bytes) => Ok(bytes.clone()),
            None => Err(StoreError::Missing),
        }
    }
    fn delete(&mut self, key: &str) -> Result<(), StoreError> {
        self.0.borrow_mut().remove(key);
        Ok(())
    }
    fn list(&mut self, prefix: &str, limit: usize) -> Result<Vec<Object>, StoreError> {
        let out: Vec<_> = self
            .0
            .borrow()
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, bytes)| Object {
                key: key.clone(),
                size: Some(bytes.len() as u64),
            })
            .collect();
        if out.len() > limit {
            return Err(StoreError::Limit);
        }
        Ok(out)
    }
}
fn config() -> Config {
    let destination =
        replica_core::marker::destination("http://memory", "bucket", "test", "local").unwrap();
    Config::new(
        "test/local",
        &destination,
        Name::new("spin.sqlite").unwrap(),
        1 << 24,
    )
    .unwrap()
}
fn open(root: &std::path::Path, store: &Memory, key: [u8; 32]) -> Owner<Memory> {
    let mut files = Files::open(root).unwrap();
    let mut heap = vec![0_u64; spin_persistence::SQLITE_HEAP_BYTES / 8];
    let mut bucket = store.clone();
    let replica = prepare(&mut files, &mut heap, &mut bucket, config()).unwrap();
    Owner::new(
        heap,
        files,
        Cipher::new(key),
        Random::open().unwrap(),
        replica,
        bucket,
        lease_key(&config()),
    )
}
fn replication(owner: &mut Owner<Memory>) -> Value {
    owner.storage_usage().unwrap().unwrap().replication
}

#[test]
fn publishes_and_restores_an_empty_directory() {
    let base = std::env::temp_dir().join(format!("spin-replica-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let store = Memory::default();
    {
        let mut owner = open(&base.join("a"), &store, [7; 32]);
        let mut random = Random::open().unwrap();
        let state = owner
            .load(|| {
                spin_store::IdSource::next(&mut random, "lgn")
                    .map_err(|_| spin_security::Error::Entropy(-1))
            })
            .unwrap();
        owner.save(&state).unwrap();
        owner
            .maintain(&spin_host::server::timestamp().unwrap())
            .unwrap();
        let status = replication(&mut owner);
        let status = status.as_object().unwrap();
        assert_eq!(status.get("complete"), Some(&Value::Bool(true)));
        assert_ne!(status.get("last_sync_at"), Some(&Value::Null));
        assert_eq!(status.get("restored"), Some(&Value::Bool(false)));
    }
    assert!(
        store
            .0
            .borrow()
            .keys()
            .any(|k| k.starts_with("test/local/"))
    );
    // De vorige eigenaar is weg; zijn lease verloopt (hier: de lease-opslag leeg).
    *store.1.borrow_mut() = MemBackend::new();
    // Een lege directory herstelt exact de gepubliceerde staterij.
    let mut owner = open(&base.join("b"), &store, [7; 32]);
    let status = replication(&mut owner);
    assert_eq!(
        status.as_object().unwrap().get("restored"),
        Some(&Value::Bool(true))
    );
    drop(owner);
    *store.1.borrow_mut() = MemBackend::new();
    let published = state_row(&base.join("a"));
    assert!(!published.is_empty());
    assert_eq!(state_row(&base.join("b")), published);
    // Dezelfde sleutel ontsleutelt de herstelde state.
    let mut owner = open(&base.join("b"), &store, [7; 32]);
    owner.load(|| Ok(String::from("lgn_x"))).unwrap();
    drop(owner);
    let _ = std::fs::remove_dir_all(&base);
}
fn state_row(root: &std::path::Path) -> Vec<u8> {
    let mut files = Files::open(root).unwrap();
    let mut heap = vec![0_u64; spin_persistence::SQLITE_HEAP_BYTES / 8];
    // SAFETY: Deze test draait één SQLite-runtime tegelijk op zijn eigen thread.
    let mut engine = unsafe { replica_sqlite::Engine::initialize(&mut heap, &mut files) }.unwrap();
    let mut database =
        spin_persistence::Database::attach(engine.open(c"spin.sqlite").unwrap()).unwrap();
    database
        .read_file("state", spin_persistence::MAX_STATE_BYTES)
        .unwrap()
}
