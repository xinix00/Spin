//! De hostboot van de Docker-runner; de eigenaar houdt verbindingen en processen apart.
use spin_domain::{List, try_string};
use spin_host::{
    runner::{self, Config},
    storage::Random,
};
use spin_store::IdSource;
use std::{
    fs::File,
    io::Read,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
fn env(name: &str, fallback: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| fallback.into())
}
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_handler(_: libc::c_int) {
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}
fn signals() -> std::io::Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: dit is de Unix-bootgrens. De statische handler alloceert niet,
        // neemt geen locks en schrijft uitsluitend een lockvrije AtomicBool.
        let previous =
            unsafe { libc::signal(signal, stop_handler as *const () as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
fn bounded_file(path: &std::path::Path) -> std::io::Result<String> {
    let mut value = String::new();
    File::open(path)?.take(4097).read_to_string(&mut value)?;
    if value.len() > 4096 {
        return Err(std::io::Error::other(
            "runner identity or token file is too large",
        ));
    }
    Ok(value.trim().into())
}
fn run() -> std::io::Result<()> {
    let hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let hostname = hostname.trim();
    let mut server = env("SPIN_SERVER", "http://127.0.0.1:8080");
    let mut name = env(
        "SPIN_CLIENT_NAME",
        if hostname.is_empty() {
            "spin-client"
        } else {
            hostname
        },
    );
    let mut id = env("SPIN_CLIENT_ID", "");
    let mut id_file = PathBuf::from(env("SPIN_CLIENT_ID_FILE", "./var/spin-client.id"));
    let mut token_file = PathBuf::from(env("SPIN_WORKER_TOKEN_FILE", "./var/spin-worker.token"));
    let mut tools = String::from("docker");
    let mut maximum: usize = env("SPIN_MAX_WORKLOADS", "4").parse().unwrap_or(4);
    let mut base = String::from("alpine:3.24");
    let mut network = String::from("bridge");
    let mut env_dir = env("SPIN_ENV_DIR", "./var/env");
    let mut advertise_host = env("SPIN_ADVERTISE_HOST", "");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if matches!(arg.as_str(), "-version" | "--version") {
            println!("spin-client {} rust", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        if matches!(arg.as_str(), "-h" | "--help" | "-help") {
            println!(
                "spin-client --server http://127.0.0.1:8080 --name NAME\n  --id ID --id-file PATH --token-file PATH --tools docker\n  --max-workloads 4 --capsule-base alpine:3.24 --capsule-network bridge\nSPIN_WORKER_TOKEN supplies the runner bearer token; SPIN_DOCKER selects its Docker CLI."
            );
            return Ok(());
        }
        let value = args
            .next()
            .ok_or_else(|| std::io::Error::other("runner argument needs a value"))?;
        match arg.trim_start_matches('-') {
            "env-dir" => env_dir = value,
            "advertise-host" => advertise_host = value,
            "server" => server = value,
            "name" => name = value,
            "id" => id = value,
            "id-file" => id_file = value.into(),
            "token-file" => token_file = value.into(),
            "tools" => tools = value,
            "max-workloads" => maximum = value.parse().map_err(std::io::Error::other)?,
            "capsule-base" => base = value,
            "capsule-network" => network = value,
            _ => return Err(std::io::Error::other("unknown runner argument; see --help")),
        }
    }
    let mut token = env("SPIN_WORKER_TOKEN", "");
    if token.is_empty() {
        let start = Instant::now();
        loop {
            match bounded_file(&token_file) {
                Ok(value) if !value.is_empty() => {
                    token = value;
                    break;
                }
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            if start.elapsed() >= Duration::from_secs(15) {
                return Err(std::io::Error::other(
                    "worker token file did not become available",
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if id.is_empty() {
        match bounded_file(&id_file) {
            Ok(value) if !value.is_empty() => id = value,
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {
                let value = spin_core::validation::text(format_args!(
                    "spin-client\0{}\0{}",
                    hostname.to_lowercase(),
                    name.trim()
                ))
                .map_err(std::io::Error::other)?;
                let hash =
                    spin_security::digest_hex(value.as_bytes()).map_err(std::io::Error::other)?;
                id = spin_core::validation::text(format_args!("host-{}", &hash[..24]))
                    .map_err(std::io::Error::other)?;
            }
        }
    }
    let mut advertised = List::new();
    for tool in tools.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        advertised
            .push(try_string(tool).map_err(std::io::Error::other)?)
            .map_err(std::io::Error::other)?;
    }
    let process = Random::open()?
        .next("proc")
        .map_err(std::io::Error::other)?;
    let docker = spin_core::docker::Docker::new(&env("SPIN_DOCKER", "docker"), &base, &network)
        .map_err(std::io::Error::other)?;
    signals()?;
    runner::run(
        Config {
            env_dir,
            advertise_host,
            server,
            token,
            instance_id: id,
            process,
            name,
            tools: advertised,
            max_workloads: maximum,
        },
        docker,
        &STOP,
    )
}
fn main() {
    if let Err(error) = run() {
        eprintln!("SPIN_CLIENT_FAILED error={error}");
        std::process::exit(1);
    }
}
