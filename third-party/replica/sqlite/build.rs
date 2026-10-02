//! Bouwt alleen de gevendorde C-engine; geen netwerk of externe Rust-crates.
use std::{env, fmt, path::PathBuf, process::Command};

#[derive(Debug)]
enum BuildError {
    Environment(env::VarError),
    Io(std::io::Error),
    Utf8(std::str::Utf8Error),
    Target(String),
    Sdk,
    Compile(&'static str),
    Archive,
}
impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment(e) => write!(f, "bouwomgeving: {e}"),
            Self::Io(e) => write!(f, "bouwprogramma: {e}"),
            Self::Utf8(e) => write!(f, "SDK-pad: {e}"),
            Self::Target(t) => write!(f, "SQLite-target niet ondersteund: {t}"),
            Self::Sdk => f.write_str("macOS SDK niet gevonden via xcrun"),
            Self::Compile(s) => write!(
                f,
                "C-compilatie mislukt: {s}; ARM64 vereist clang met aapcs-soft (getoetst: LLVM 23.1.0)"
            ),
            Self::Archive => f.write_str("SQLite-archief bouwen mislukt"),
        }
    }
}
impl std::error::Error for BuildError {}
impl From<env::VarError> for BuildError {
    fn from(e: env::VarError) -> Self {
        Self::Environment(e)
    }
}
impl From<std::io::Error> for BuildError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<std::str::Utf8Error> for BuildError {
    fn from(e: std::str::Utf8Error) -> Self {
        Self::Utf8(e)
    }
}

fn main() -> Result<(), BuildError> {
    let out = PathBuf::from(env::var("OUT_DIR")?);
    let target = env::var("TARGET")?;
    let clang = env::var("SQLITE_CC").unwrap_or_else(|_| "clang".into());
    let ar = env::var("SQLITE_AR").unwrap_or_else(|_| "ar".into());
    let bare = target.contains("-none-");
    if !bare
        && (target != env::var("HOST")? || !(target.contains("apple") || target.contains("linux")))
    {
        return Err(BuildError::Target(target));
    }
    let objects = [
        out.join("sqlite3.o"),
        out.join("bridge.o"),
        out.join("runtime.o"),
    ];
    for ((name, source), obj) in [
        ("sqlite3", "../vendor/sqlite/sqlite3.c"),
        ("bridge", "c/bridge.c"),
        ("runtime", "c/runtime.c"),
    ]
    .into_iter()
    .zip(&objects)
    {
        if name == "runtime" && !bare {
            continue;
        }
        let mut cmd = Command::new(&clang);
        cmd.args([
            "-std=c11",
            "-O2",
            "-fno-strict-aliasing",
            "-fno-builtin",
            "-DNDEBUG",
            "-DSQLITE_OS_OTHER=1",
            "-DSQLITE_THREADSAFE=0",
            "-DSQLITE_OMIT_AUTOINIT",
            "-DSQLITE_ZERO_MALLOC",
            "-DSQLITE_ENABLE_MEMSYS5",
            "-DSQLITE_OMIT_LOAD_EXTENSION",
            "-DSQLITE_OMIT_LOCALTIME",
            "-DSQLITE_OMIT_WAL",
            "-DSQLITE_TEMP_STORE=3",
            "-DSQLITE_DEFAULT_MEMSTATUS=0",
            "-DSQLITE_MAX_WORKER_THREADS=0",
            "-I../vendor/sqlite",
            "-Ic",
        ]);
        if name != "sqlite3" {
            cmd.args(["-Wall", "-Wextra", "-Werror"]);
        }
        if bare {
            cmd.args(["-ffreestanding", "-fno-stack-protector", "-Iinclude"]);
            if target.starts_with("aarch64") {
                cmd.args([
                    "--target=aarch64-none-elf",
                    "-mgeneral-regs-only",
                    "-mabi=aapcs-soft",
                ]);
            } else if target.starts_with("riscv64") {
                cmd.args(["--target=riscv64-none-elf", "-march=rv64gc", "-mabi=lp64d"]);
            } else {
                return Err(BuildError::Target(target));
            }
        } else if target.contains("apple") {
            let sdk = Command::new("xcrun").arg("--show-sdk-path").output()?;
            if !sdk.status.success() {
                return Err(BuildError::Sdk);
            }
            cmd.arg("-isysroot")
                .arg(std::str::from_utf8(&sdk.stdout)?.trim());
        }
        if !cmd
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(obj)
            .status()?
            .success()
        {
            return Err(BuildError::Compile(source));
        }
        println!("cargo:rerun-if-changed={source}");
    }
    if !Command::new(ar)
        .arg("crs")
        .arg(out.join("libreplica_sqlite.a"))
        .args(objects.iter().take(if bare { 3 } else { 2 }))
        .status()?
        .success()
    {
        return Err(BuildError::Archive);
    }
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=replica_sqlite");
    println!("cargo:rerun-if-changed=c");
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed=../vendor/sqlite/sqlite3.h");
    println!("cargo:rerun-if-env-changed=SQLITE_CC");
    println!("cargo:rerun-if-env-changed=SQLITE_AR");
    Ok(())
}
