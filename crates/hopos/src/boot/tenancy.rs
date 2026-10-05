//! Domain discovery and one parked application stack per isolated database.
use super::*;
use alloc::string::String;
use core::{future::poll_fn, pin::Pin, task::Poll};
use spin_runtime::tenancy::{Tenants, normalize_host};

pub(super) fn database(app: &App, domain: &str) -> Result<String> {
    if let Some(name) = app.env("SPIN_DATABASE").filter(|s| !s.is_empty()) {
        // Files confines every database and Replica sidecar to the leased root.
        if name.len() > 180
            || name.starts_with('.')
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(Error::Http(
                500,
                "SPIN_DATABASE must be a basename inside SPIN_DATA_DIR",
            ));
        }
        return spin_domain::try_string(name).map_err(Error::from);
    }
    spin_core::validation::text(format_args!(
        "spin-{}.sqlite",
        spin_security::digest_hex(domain.as_bytes())?
    ))
    .map_err(Error::from)
}
pub(super) async fn discover(net: &'static appnet::Net, root: &str, tenants: &Tenants) -> Result {
    let mut names = alloc::vec::Vec::new();
    names.try_reserve_exact(32 << 10).map_err(failure)?;
    names.resize(32 << 10, 0);
    let mut sys = net.system_client();
    let size = sys.list(root, &mut names).await.map_err(failure)?;
    let names = core::str::from_utf8(&names[..size]).map_err(failure)?;
    for name in names.split('\n') {
        if let Some(domain) = name.strip_suffix(".db") {
            if normalize_host(domain).is_ok_and(|normalized| normalized == domain) {
                tenants.discover(domain)?;
            }
            continue;
        }
        let Some(hash) = name
            .strip_prefix("spin-")
            .and_then(|s| s.strip_suffix(".domain"))
        else {
            continue;
        };
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let path = spin_core::validation::text(format_args!("{root}/{name}"))?;
        let size = sys.stat(&path).await.map_err(failure)?;
        if size == 0 || size > 253 {
            continue;
        }
        let mut bytes = [0; 253];
        let mut count = 0;
        while count < size as usize {
            let read = sys
                .read_into(&path, count as u64, &mut bytes[count..size as usize])
                .await
                .map_err(failure)?;
            if read == 0 {
                break;
            }
            count += read;
        }
        let Ok(domain) = core::str::from_utf8(&bytes[..count]) else {
            continue;
        };
        if count != size as usize || spin_security::digest_hex(domain.as_bytes())? != hash {
            continue;
        }
        if normalize_host(domain).is_ok_and(|normalized| normalized == domain) {
            tenants.discover(domain)?;
        }
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
pub(super) fn owner<'a>(
    app: &'static App,
    port: u16,
    arena: &'a storage::Arena,
    files: &'a storage::FilesPool,
    cipher: &'a Cipher,
    domain: String,
    mail: &'a spin_runtime::Mailbox,
    uploads: &'a storage::Uploads,
) -> Result<impl Future<Output = Result> + Unpin + 'a> {
    uploads.reset();
    let mut uploader = if storage::replicating(app) {
        let client = storage::s3_client(app).map_err(failure)?;
        let tag = spin_domain::try_string(&domain)?;
        // SAFETY: A separate stack and backend own only Replica's spool reads and one S3
        // connection. They never enter SQLite; the owner keeps marker and tracking.
        let task = unsafe {
            Task::new(2 << 20, move |s| -> Result {
                let wait = Wait(s);
                let mut remote = storage::Bucket::new(client, crate::s3::network, Wait(s))
                    .map_err(|_| Error::Http(503, "invalid Replica S3 configuration"))?;
                loop {
                    let (pending, database) = s
                        .wait(uploads.next())
                        .map_err(|_| Error::Http(503, "replica upload cancelled"))?;
                    let mut backend = storage::Backend::new(files, &wait, database);
                    let started = applib::clock::now_ns();
                    let result = pending.upload(&mut backend, &mut remote);
                    applib::log!(
                        "SPIN_REPLICA_UPLOADED domain={tag} ok={} ms={}",
                        result.is_ok(),
                        applib::clock::now_ns().saturating_sub(started) / 1_000_000
                    );
                    uploads.done(pending, result);
                    mail.nudge();
                }
            })
        }
        .map_err(|_| Error::Http(503, "replica upload stack allocation failed"))?;
        Some(task)
    } else {
        None
    };
    // SAFETY: This bounded stack owns one Store and all its SQLite calls. Arena loans
    // exclude every other SQLite engine and are returned only after Engine::drop.
    let mut owner = unsafe {
        Task::new(8 << 20, move |s| -> Result {
            let wait = Wait(s);
            if app.env("SPIN_DATABASE").is_none_or(|s| s.is_empty()) {
                files.marker(&wait, &domain)?;
            }
            s.wait(super::catalog::register(app, &domain))
                .map_err(|_| Error::Http(503, "domain registration cancelled"))??;
            let mut path = database(app, &domain)?;
            let mut go = false;
            if app.env("SPIN_DATABASE").is_none_or(|s| s.is_empty()) {
                let legacy = spin_core::validation::text(format_args!("{domain}.db"))?;
                if files.exists(&wait, &legacy)? {
                    if files.exists(&wait, &path)? {
                        return Err(Error::Http(
                            409,
                            "both Go and Rust databases exist for domain",
                        ));
                    }
                    path = legacy;
                    go = true;
                    applib::log!("SPIN_LEGACY_DATABASE domain={domain}");
                }
            }
            // Een map per tenant (spin-<hash>/spin.sqlite). Staat de database nog
            // los in de root, dan herstelt Replica hem in de map en gaan de oude
            // bestanden na een geslaagde start weg; zonder replicatie blijft hij
            // waar hij is.
            let dir = match path.strip_suffix(".sqlite") {
                Some(base) => spin_domain::try_string(base)?,
                None => spin_core::validation::text(format_args!("{path}.d"))?,
            };
            let in_dir = files.exists(
                &wait,
                &spin_core::validation::text(format_args!("{dir}/spin.sqlite"))?,
            )?;
            let in_root = files.exists(&wait, &path)?;
            let moving = in_root && !in_dir && !go && storage::replicating(app);
            let location = if go || (in_root && !in_dir && !moving) {
                storage::Location::Root(spin_domain::try_string(&path)?)
            } else {
                if moving {
                    applib::log!("SPIN_STORAGE_MOVING from={path} to={dir}/ via=replica");
                }
                storage::Location::Dir(dir)
            };
            let in_map = matches!(location, storage::Location::Dir(_));
            let bridge = storage::Backend::new(files, &wait, location);
            let cipher = Cipher::from_encoded(&cipher.portable_key()?)?;
            let mut random = Random::open(app)?;
            let mut persistence = storage::Owner::new(
                arena,
                bridge,
                cipher,
                Random::open(app)?,
                app,
                &domain,
                uploads,
                mail.restore(),
            )
            .map_err(failure)?;
            let state = persistence
                .load(|| {
                    random
                        .next("lgn")
                        .map_err(|_| spin_security::Error::Entropy(-1))
                })
                .map_err(failure)?;
            if in_root && !go && in_map {
                // De database staat nu in de map; de oude bestanden in de root
                // (database, journal, Replica-marker, -logs, -spool) gaan weg.
                for suffix in [
                    "",
                    "-journal",
                    ".replica",
                    ".replica-dirty-a",
                    ".replica-dirty-b",
                    ".replica-capture",
                    ".replica-restore-data",
                    ".replica-restore-data-journal",
                    ".replica-restoring",
                ] {
                    let name = spin_core::validation::text(format_args!("{path}{suffix}"))?;
                    if let Err(error) = files.remove(&wait, &name) {
                        applib::log!("SPIN_STORAGE_MOVE_CLEANUP_FAILED file={name} error={error}");
                    }
                }
                applib::log!("SPIN_STORAGE_MOVED to=map");
            }
            let mut server = spin_server::Server::new(spin_store::Store::new(state, persistence));
            let domain_url = if app.env("SPIN_DATABASE").is_some_and(|s| !s.is_empty()) {
                String::new()
            } else {
                spin_core::validation::text(format_args!("https://{domain}"))?
            };
            let public_url = app
                .env("SPIN_PUBLIC_URL")
                .filter(|s| !s.is_empty())
                .unwrap_or(&domain_url);
            server.set_internal_url(
                app.env("SPIN_INTERNAL_URL")
                    .filter(|s| !s.is_empty())
                    .unwrap_or(public_url),
            )?;
            server.set_public_url(public_url)?;
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
                server.set_oauth_environment(
                    provider,
                    app.env(id).unwrap_or(""),
                    app.env(secret).unwrap_or(""),
                )?;
            }
            server.recover(&timestamp(app)?)?;
            server.ensure_worker_token(app.env("SPIN_WORKER_TOKEN").unwrap_or(""), &mut random)?;
            applib::log!(
                "SPIN_TENANT_READY domain={domain} port={port} version={} platform=hopos",
                env!("CARGO_PKG_VERSION")
            );
            spin_runtime::serve_owner(
                Native {
                    app,
                    listener: None,
                    wait: Some(s),
                },
                &mut server,
                &mut random,
                mail,
            )
        })
    }
    .map_err(|_| Error::Http(503, "SQLite stack allocation failed"))?;
    // The uploader only ends by failing; the owner then stops with it, like a lost lease.
    Ok(poll_fn(move |cx| {
        if let Some(task) = &mut uploader
            && let Poll::Ready(result) = Pin::new(task).poll(cx)
        {
            applib::log!("SPIN_REPLICA_UPLOADER_STOPPED");
            return Poll::Ready(result.and(Err(Error::Http(503, "replica uploader stopped"))));
        }
        Pin::new(&mut owner).poll(cx)
    }))
}
