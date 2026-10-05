//! Alleen boot vestigt de unieke SQLite-runtime en haar parkeerbare C-stack.
use crate::platform::{Environment, Native, Random, failure, timestamp};
mod catalog;
mod storage;
mod tenancy;
use applib::{App, EXEC, appnet, stacktask::Task};
use core::{
    future::{Future, poll_fn},
    pin::pin,
    sync::atomic::Ordering::Relaxed,
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
    // Eén schrijver per namespace bewaakt de S3-lease van Replica per tenant
    // (storage::Owner::new); de oude lease op de lokale schijf is weg.
    serve(app, net, root, port, secure).await
}
/// De meetlat van de boot-lus (handboek "De meetlat"): elke 30 s één regel
/// met de tellers van de executor sinds de vorige regel, en een regel per
/// ronde die langer dan 200 ms duurde. Op rondekorrel, zonder eigen timer:
/// een stille core wordt voor een meetregel niet gewekt.
struct Report {
    /// Tijdstip van de volgende `SPIN_EXEC`-regel.
    due: u64,
    /// Standen bij de vorige regel: rounds, polls, sleeps, timer_overflows.
    last: [u64; 4],
}
impl Report {
    const INTERVAL: u64 = 30_000_000_000;
    const SLOW: u64 = 200_000_000;
    fn new() -> Self {
        Self {
            due: applib::clock::now_ns().saturating_add(Self::INTERVAL),
            last: [0; 4],
        }
    }
    /// Sluit de ronde die op `started` begon af; `true` als de 30 s-regel net
    /// geschreven is.
    fn round(&mut self, started: u64) -> bool {
        let now = applib::clock::now_ns();
        let busy = now.saturating_sub(started);
        if busy > Self::SLOW {
            applib::log!("SPIN_BOOT_SLOW ms={}", busy / 1_000_000);
        }
        if now < self.due {
            return false;
        }
        self.due = now.saturating_add(Self::INTERVAL);
        let exec = EXEC.get();
        let current = [
            exec.stats.rounds.load(Relaxed),
            exec.stats.polls.load(Relaxed),
            exec.stats.sleeps.load(Relaxed),
            exec.stats.timer_overflows.load(Relaxed),
        ];
        let delta = [
            current[0].wrapping_sub(self.last[0]),
            current[1].wrapping_sub(self.last[1]),
            current[2].wrapping_sub(self.last[2]),
            current[3].wrapping_sub(self.last[3]),
        ];
        self.last = current;
        // `live_tasks` telt de slots met een taak; de boot-taak zelf is
        // tijdens zijn poll even uit zijn slot en telt dus niet mee.
        applib::log!(
            "SPIN_EXEC rounds={} polls={} sleeps={} timer_overflows={} tasks={}",
            delta[0],
            delta[1],
            delta[2],
            delta[3],
            exec.live_tasks()
        );
        true
    }
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
    let mut discovery = pin!(catalog::discover(app, &tenants));
    let mut discovered = false;
    let mut discovery_retry = 0;
    let mut transport = spin_runtime::Transport::routed(&tenants, secure);
    let mut network = Native {
        app,
        listener: Some(net.tcp_listen(port).map_err(failure)?),
        wait: None,
    };
    let mut uploads = alloc::vec::Vec::new();
    uploads
        .try_reserve_exact(spin_runtime::tenancy::TENANTS)
        .map_err(failure)?;
    uploads.resize_with(spin_runtime::tenancy::TENANTS, storage::Uploads::new);
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
    // De tik van de lus: één timer voor alle sockets (conn.rs), de
    // tenant-retries en de domeinontdekking; per ronde opnieuw gezet.
    let mut tick = pin!(EXEC.get().after_deferrable(Duration::from_millis(10)));
    let mut report = Report::new();
    loop {
        poll_fn(|cx| -> Poll<Result> {
            let started = applib::clock::now_ns();
            crate::conn::NEXT_DEADLINE.store(u64::MAX, Relaxed);
            if let Err(error) = transport.poll(&mut network, cx) {
                return Poll::Ready(Err(error));
            }
            let now = started;
            if !discovered && now >= discovery_retry {
                match discovery.as_mut().poll(cx) {
                    Poll::Ready(Ok(())) => discovered = true,
                    Poll::Ready(Err(error)) => {
                        applib::log!("SPIN_DOMAIN_DISCOVERY_FAILED error={error}");
                        discovery.set(catalog::discover(app, &tenants));
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
                            port,
                            &heap,
                            &files,
                            &cipher,
                            domain,
                            tenants.mailbox(index),
                            &uploads[index],
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
            // Een ronde komt op werk (sockets, eigenaren, de uploader) of op
            // de vroegste termijn: van een socket die deze ronde `Pending`
            // gaf, een tenant-retry of de domeinontdekking; de 10 ms is een
            // uitstelbare vangrail die een slapende core niet wekt.
            let sockets = crate::conn::NEXT_DEADLINE.load(Relaxed);
            let deadline = retry
                .iter()
                .copied()
                .filter(|at| *at != 0)
                .chain((!discovered && discovery_retry != 0).then_some(discovery_retry))
                .chain((sockets != u64::MAX).then_some(sockets))
                .min();
            tick.set(match deadline {
                Some(at) => EXEC.get().until(at),
                None => EXEC.get().after_deferrable(Duration::from_millis(10)),
            });
            let outcome = tick.as_mut().poll(cx).map(|()| Ok(()));
            if report.round(started) {
                // Een eigenaar slaapt hooguit een seconde en een stap duurt
                // seconden; wie 5 s of langer in één fase zit, staat hier.
                let now_ms = applib::clock::now_ns() / 1_000_000;
                for index in 0..spin_runtime::tenancy::TENANTS {
                    let mail = tenants.mailbox(index);
                    let (phase, since) = mail.phase();
                    let ms = now_ms.saturating_sub(since);
                    if mail.active() && ms >= 5_000 {
                        applib::log!("SPIN_OWNER_PHASE slot={index} phase={phase} ms={ms}");
                    }
                }
            }
            outcome
        })
        .await?;
        if spin_runtime::Platform::stopped(&network) {
            return Ok(());
        }
    }
}
