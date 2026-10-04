//! De hostboot bezit de unieke SQLite-runtime, sleutel en HTTP-listener.
use spin_host::storage::{Files, Random};
use spin_security::{Cipher, Entropy};
use spin_store::IdSource;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
fn key(root: &Path, random: &mut Random) -> std::io::Result<Cipher> {
    if let Ok(value) = std::env::var("SPIN_MASTER_KEY")
        && !value.trim().is_empty()
    {
        return Cipher::from_encoded(&value).map_err(std::io::Error::other);
    }
    let path = std::env::var_os("SPIN_MASTER_KEY_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("spin-master.key"));
    match File::open(&path) {
        Ok(file) => {
            let mut value = String::new();
            file.take(1024).read_to_string(&mut value)?;
            Cipher::from_encoded(&value).map_err(std::io::Error::other)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if root.join("spin.sqlite").exists() {
                return Err(std::io::Error::other(
                    "master key missing for existing database",
                ));
            }
            let mut key = [0; 32];
            random.fill(&mut key).map_err(std::io::Error::other)?;
            let cipher = Cipher::new(key);
            let encoded = cipher.portable_key().map_err(std::io::Error::other)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(encoded.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(cipher)
        }
        Err(e) => Err(e),
    }
}
fn run() -> std::io::Result<()> {
    let mut addr = String::from("127.0.0.1:8080");
    let mut root = std::env::var_os("SPIN_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./var/rust"));
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" | "-version" => {
                println!("spin-server {} rust", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "spin-server --addr 127.0.0.1:8080 --data-dir ./var/rust\nSPIN_MASTER_KEY or SPIN_MASTER_KEY_FILE supplies an existing encryption key.\nSPIN_S3_ENDPOINT/BUCKET/ACCESS_KEY/SECRET_KEY/PREFIX enable Replica to S3 (prefix never \"spin\")."
                );
                return Ok(());
            }
            "--addr" | "-addr" => {
                addr = args
                    .next()
                    .ok_or_else(|| std::io::Error::other("--addr needs a value"))?
            }
            "--data-dir" | "-data-dir" => {
                root = PathBuf::from(
                    args.next()
                        .ok_or_else(|| std::io::Error::other("--data-dir needs a value"))?,
                )
            }
            _ => return Err(std::io::Error::other("unknown argument; see --help")),
        }
    }
    let listener = TcpListener::bind(&addr)?;
    let mut files = Files::open(&root)?;
    let mut random = Random::open()?;
    let mut heap = Vec::new();
    heap.try_reserve_exact(spin_persistence::SQLITE_HEAP_BYTES / 8)
        .map_err(std::io::Error::other)?;
    heap.resize(spin_persistence::SQLITE_HEAP_BYTES / 8, 0_u64);
    if let Some(settings) =
        spin_host::replica::settings(|key| std::env::var(key).unwrap_or_default())?
    {
        // Prepare vóór de sleutel: een herstelde database zonder sleutel weigert
        // dan te starten in plaats van een nieuwe sleutel te maken.
        let mut bucket = spin_host::replica::bucket(settings.client)?;
        let replica =
            spin_host::replica::prepare(&mut files, &mut heap, &mut bucket, settings.config)?;
        let cipher = key(&root, &mut random)?;
        let mut owner =
            spin_host::replica::Owner::new(heap, files, cipher, Random::open()?, replica, bucket);
        let state = owner
            .load(|| {
                random
                    .next("lgn")
                    .map_err(|_| spin_security::Error::Entropy(-1))
            })
            .map_err(std::io::Error::other)?;
        return serve(
            listener,
            spin_server::Server::new(spin_store::Store::new(state, owner)),
            &mut random,
        );
    }
    let cipher = key(&root, &mut random)?;
    // SAFETY: main initialiseert exact één SQLite-runtime vóór het netwerkwerk.
    // Alle calls en VFS-callbacks blijven op deze thread. De geleende heap en
    // opslag overleven server en verbinding; callbacks herintreden niet in SQLite.
    let mut engine = unsafe { replica_sqlite::Engine::initialize(&mut heap, &mut files) }
        .map_err(std::io::Error::other)?;
    let database = spin_persistence::Database::open(
        engine.open(c"spin.sqlite").map_err(std::io::Error::other)?,
    )
    .map_err(std::io::Error::other)?;
    let mut persistence = spin_persistence::Encrypted::new(database, cipher, Random::open()?);
    let state = persistence
        .load(|| {
            random
                .next("lgn")
                .map_err(|_| spin_security::Error::Entropy(-1))
        })
        .map_err(std::io::Error::other)?;
    serve(
        listener,
        spin_server::Server::new(spin_store::Store::new(state, persistence)),
        &mut random,
    )
}
/// Dezelfde serverinrichting voor de lokale en de gerepliceerde opslag.
fn serve<P: spin_store::Persistence>(
    listener: TcpListener,
    mut server: spin_server::Server<P>,
    random: &mut Random,
) -> std::io::Result<()> {
    server
        .set_internal_url(&std::env::var("SPIN_INTERNAL_URL").unwrap_or_default())
        .map_err(std::io::Error::other)?;
    server
        .recover(&spin_host::server::timestamp()?)
        .map_err(std::io::Error::other)?;
    server
        .ensure_worker_token(
            &std::env::var("SPIN_WORKER_TOKEN").unwrap_or_default(),
            random,
        )
        .map_err(std::io::Error::other)?;
    server
        .set_public_url(&std::env::var("SPIN_PUBLIC_URL").unwrap_or_default())
        .map_err(std::io::Error::other)?;
    for (provider, id, secret) in [
        (
            "github",
            "SPIN_GITHUB_CLIENT_ID",
            "SPIN_GITHUB_CLIENT_SECRET",
        ),
        (
            "gitlab",
            "SPIN_GITLAB_CLIENT_ID",
            "SPIN_GITLAB_CLIENT_SECRET",
        ),
    ] {
        server
            .set_oauth_environment(
                provider,
                &std::env::var(id).unwrap_or_default(),
                &std::env::var(secret).unwrap_or_default(),
            )
            .map_err(std::io::Error::other)?;
    }
    let secure = std::env::var("SPIN_PUBLIC_URL").is_ok_and(|url| url.starts_with("https://"));
    println!(
        "SPIN_LISTEN addr={} version={}",
        listener.local_addr()?,
        env!("CARGO_PKG_VERSION")
    );
    spin_host::server::serve(
        listener,
        &mut server,
        random,
        secure,
        &AtomicBool::new(false),
    )
}
fn main() {
    if let Err(error) = run() {
        eprintln!("SPIN_BOOT_FAILED error={error}");
        std::process::exit(1);
    }
}
