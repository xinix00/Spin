//! Alleen boot vestigt de unieke SQLite-runtime en haar parkeerbare C-stack.
use crate::platform::{Environment, Native, Random, failure, timestamp};
mod catalog;
mod lease;
mod storage;
mod tenancy;
use applib::{App, EXEC, appnet, stacktask::Task};
use core::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
    time::Duration,
};
use replica_hopos::{Files, Wait};
use spin_security::{Cipher, Entropy};
use spin_server::{Error, Result};
use spin_store::IdSource;

pub(crate) async fn main(app: &'static App) {
    if let Err(error) = run(app).await {
        applib::log!("SPIN_BOOT_FAILED error={error}");
        app.shutdown(1).await;
    } else {
        app.shutdown(0).await;
    }
}
async fn key(
    app: &App,
    sys: &mut appnet::SystemClient,
    root: &str,
    random: &mut Random,
) -> Result<Cipher> {
    if let Some(value) = app.env("SPIN_MASTER_KEY").filter(|s| !s.trim().is_empty()) {
        return Cipher::from_encoded(value).map_err(Error::from);
    }
    let path =
        spin_core::validation::text(format_args!("{root}/spin-master.key")).map_err(failure)?;
    match sys.stat(&path).await {
        Ok(size) => {
            if size == 0 || size > 1024 {
                return Err(Error::Http(500, "invalid master key file"));
            }
            let mut bytes = [0; 1024];
            let size = usize::try_from(size).map_err(failure)?;
            let mut offset = 0;
            while offset < size {
                let n = sys
                    .read_into(&path, offset as u64, &mut bytes[offset..size])
                    .await
                    .map_err(failure)?;
                if n == 0 {
                    return Err(Error::Http(500, "truncated master key file"));
                }
                offset += n;
            }
            Cipher::from_encoded(core::str::from_utf8(&bytes[..size]).map_err(failure)?)
                .map_err(Error::from)
        }
        Err(applib::sys::Error::NotFound { .. }) => {
            if !app
                .env("SPIN_REPLICATION")
                .unwrap_or("")
                .eq_ignore_ascii_case("off")
            {
                return Err(Error::Http(
                    500,
                    "SPIN_MASTER_KEY required before restoring a replicated database",
                ));
            }
            let mut listing = alloc::vec::Vec::new();
            listing.try_reserve_exact(32 << 10).map_err(failure)?;
            listing.resize(32 << 10, 0);
            let count = sys.list(root, &mut listing).await.map_err(failure)?;
            let names = core::str::from_utf8(&listing[..count]).map_err(failure)?;
            let configured = app.env("SPIN_DATABASE").unwrap_or("spin.sqlite");
            if names.split('\n').any(|name| {
                name == configured || name.ends_with(".sqlite") || name.ends_with(".domain")
            }) {
                return Err(Error::Http(500, "master key missing for existing database"));
            }
            let mut bytes = [0; 32];
            random.fill(&mut bytes)?;
            let cipher = Cipher::new(bytes);
            let encoded = cipher.portable_key()?;
            sys.write_file(&path, encoded.as_bytes())
                .await
                .map_err(failure)?;
            sys.sync(&path).await.map_err(failure)?;
            Ok(cipher)
        }
        Err(error) => Err(failure(error)),
    }
}
async fn run(app: &'static App) -> Result {
    let net = appnet::up(app).map_err(failure)?;
    // SDK alpha.18 exposes a nonzero placeholder before Hop's first SNTP sync:
    // hopos/src/clock.rs BOOT_WALL_SECS = 1790640000 (2026-09-29).
    // Nonzero alone is therefore not evidence of UTC. Recognize that pinned
    // kernel's bootstrap offset before signing S3 or acquiring a timed lease.
    const BOOT_WALL_SECS: u64 = 1_790_640_000;
    let mut ready = false;
    for _ in 0..300 {
        let offset = app.ctrl().wall_offset() / 1_000_000_000;
        if app.wall_ns().is_some() && offset.abs_diff(BOOT_WALL_SECS) > 2 {
            ready = true;
            break;
        }
        EXEC.get().after(Duration::from_millis(100)).await;
    }
    if !ready {
        return Err(Error::Http(
            503,
            "Hop must synchronize UTC before Spin starts",
        ));
    }
    timestamp(app)?;
    let root = app.env("SPIN_DATA_DIR").unwrap_or("/data/spin");
    let port = app
        .env("SPIN_PORT")
        .unwrap_or("8080")
        .parse::<u16>()
        .map_err(failure)?;
    if port == 0 {
        return Err(Error::Http(500, "SPIN_PORT must be nonzero"));
    }
    let secure = app
        .env("SPIN_PUBLIC_URL")
        .is_some_and(|s| s.starts_with("https://"));
    lease::guard(app, net, root, || serve(app, net, root, port, secure)).await
}
async fn serve(
    app: &'static App,
    net: &'static appnet::Net,
    root: &'static str,
    port: u16,
    secure: bool,
) -> Result {
    let mut random = Random::open(app)?;
    let cipher = key(app, &mut net.system_client(), root, &mut random).await?;
    let heap = storage::Arena::new()?;
    let single = app
        .env("SPIN_DATABASE")
        .filter(|s| !s.is_empty())
        .map(|_| app.env("SPIN_DOMAIN").unwrap_or("spin"));
    if single.is_some() {
        tenancy::database(app, "spin")?;
    }
    let tenants =
        spin_runtime::tenancy::Tenants::new(single, app.env("SPIN_DOMAINS").unwrap_or(""))?;
    if single.is_none() {
        tenancy::discover(net, root, &tenants).await?;
    }
    let files = storage::FilesPool::new(
        Files::new(
            net.system_client(),
            root,
            Environment {
                app,
                random: Random::open(app)?,
            },
        )
        .map_err(failure)?,
    );
    let mut discovery = pin!(catalog::discover(app, net, &tenants));
    let mut discovered = false;
    let mut discovery_retry = 0;
    let mut transport = spin_runtime::Transport::routed(&tenants, secure);
    let mut network = Native {
        app,
        net,
        listener: Some(net.tcp_listen(port).map_err(failure)?),
        wait: None,
    };
    let mut owners = alloc::vec::Vec::new();
    owners
        .try_reserve_exact(spin_runtime::tenancy::TENANTS)
        .map_err(failure)?;
    owners.resize_with(spin_runtime::tenancy::TENANTS, || None);
    let mut retry = [0u64; spin_runtime::tenancy::TENANTS];
    let mut first = 0;
    applib::log!(
        "SPIN_LISTEN port={port} version={} platform=hopos",
        env!("CARGO_PKG_VERSION")
    );
    loop {
        let mut tick = pin!(EXEC.get().after(Duration::from_millis(10)));
        poll_fn(|cx| -> Poll<Result> {
            if let Err(error) = transport.poll(&mut network, cx) {
                return Poll::Ready(Err(error));
            }
            let now = applib::clock::now_ns();
            if !discovered && now >= discovery_retry {
                match discovery.as_mut().poll(cx) {
                    Poll::Ready(Ok(())) => discovered = true,
                    Poll::Ready(Err(error)) => {
                        applib::log!("SPIN_DOMAIN_DISCOVERY_FAILED error={error}");
                        discovery.set(catalog::discover(app, net, &tenants));
                        discovery_retry = now.saturating_add(10_000_000_000);
                    }
                    Poll::Pending => {}
                }
            }
            for (index, deadline) in retry.iter_mut().enumerate() {
                if *deadline != 0 && now >= *deadline {
                    *deadline = 0;
                    tenants.retry(index);
                }
            }
            loop {
                match tenants.pending() {
                    Ok(Some((index, domain))) => {
                        match tenancy::owner(
                            app,
                            net,
                            port,
                            &heap,
                            &files,
                            &cipher,
                            domain,
                            tenants.mailbox(index),
                        ) {
                            Ok(owner) => owners[index] = Some(owner),
                            Err(error) => {
                                applib::log!("SPIN_TENANT_OPEN_FAILED slot={index} error={error}");
                                tenants.failed(index);
                                retry[index] = now.saturating_add(10_000_000_000);
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => return Poll::Ready(Err(error)),
                }
            }
            for offset in 0..owners.len() {
                let index = (first + offset) % owners.len();
                let completed = owners[index]
                    .as_mut()
                    .and_then(|owner| match core::pin::Pin::new(owner).poll(cx) {
                        Poll::Ready(result) => Some(result),
                        Poll::Pending => None,
                    });
                if let Some(result) = completed {
                    owners[index] = None;
                    if tenants.mailbox(index).active() {
                        return Poll::Ready(result);
                    }
                    if let Err(error) = result {
                        applib::log!("SPIN_TENANT_OPEN_FAILED slot={index} error={error}");
                    }
                    tenants.failed(index);
                    retry[index] = now.saturating_add(10_000_000_000);
                }
            }
            first = (first + 1) % owners.len();
            if let Err(error) = transport.poll(&mut network, cx) {
                return Poll::Ready(Err(error));
            }
            tick.as_mut().poll(cx).map(|()| Ok(()))
        })
        .await?;
        if spin_runtime::Platform::stopped(&network) {
            return Ok(());
        }
    }
}
