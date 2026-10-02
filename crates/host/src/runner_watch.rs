//! Eén watcher per capsule; één snapshotreader houdt het gezamenlijke bytebudget klein.
use crate::{
    executor,
    process::{DockerExecutor, Process},
};
use spin_core::docker::{Docker, Error, Result};
use spin_domain::{
    self as d, TryClone, Wire,
    protocol::{self as p, WireMessage},
    try_string,
};
use std::{
    cell::{Cell, RefCell},
    io::ErrorKind,
    task::{Context, Poll},
    time::{Duration, Instant},
};
const WATCHERS: usize = 64;
pub(crate) struct Mail {
    pub(crate) event: RefCell<Option<WireMessage>>,
    reading: Cell<bool>,
}
impl Mail {
    pub(crate) fn new() -> Self {
        Self {
            event: RefCell::new(None),
            reading: Cell::new(false),
        }
    }
}
pub(crate) struct Watches<'a> {
    tasks: [Option<executor::Task<'a>>; WATCHERS],
    containers: [Option<String>; WATCHERS],
    mail: &'a Mail,
    docker: &'a Docker,
}
impl<'a> Watches<'a> {
    pub(crate) fn new(mail: &'a Mail, docker: &'a Docker) -> Self {
        Self {
            tasks: std::array::from_fn(|_| None),
            containers: std::array::from_fn(|_| None),
            mail,
            docker,
        }
    }
    pub(crate) fn install(&mut self, value: p::TrackedFilesPayload) -> Result<()> {
        let selection = d::engine::TrackedSelection {
            paths: value.paths,
            excludes: value.excludes,
        };
        let command = self
            .docker
            .watch_tracked_files(&value.runtime, &selection)?;
        let current = self
            .containers
            .iter()
            .position(|id| id.as_deref() == Some(value.runtime.container_id.as_str()));
        let Some(command) = command else {
            if let Some(index) = current {
                self.tasks[index] = None;
                self.containers[index] = None;
            }
            return Ok(());
        };
        let index = current
            .or_else(|| self.containers.iter().position(Option::is_none))
            .ok_or(Error::Invalid("runner watcher table is full"))?;
        let id = value.runtime.container_id.try_clone()?;
        let process = Process::spawn(&command)
            .map_err(|_| Error::Invalid("cannot start tracked file watcher"))?;
        let docker = self.docker;
        let mail = self.mail;
        let task = executor::task(async move {
            if let Err(error) = watch(process, docker, &value.runtime, &selection, mail).await {
                eprintln!("SPIN_WATCH_FAILED error={error}");
            }
            // An exited subprocess cannot keep an acknowledged watcher selection alive.
            // The server retries only if this exact capsule still belongs to the runner.
            let report = (|| -> Result<WireMessage> {
                Ok(WireMessage {
                    version: p::PROTOCOL_VERSION,
                    r#type: try_string(p::MESSAGE_EVENT)?,
                    method: try_string(p::METHOD_TRACKED_CHANGED)?,
                    error: try_string("tracked file watcher stopped")?,
                    payload: d::RawJson(Some(
                        p::TrackedFilesPayload {
                            runtime: value.runtime,
                            ..Default::default()
                        }
                        .to_value()?,
                    )),
                    ..Default::default()
                })
            })();
            if let Ok(report) = report {
                let _lease = lease(mail).await;
                mail.event.replace(Some(report));
            }
        })
        .map_err(|_| Error::Domain(d::Error::OutOfMemory))?;
        // De nieuwe selectie en taak zijn klaar vóór de vorige watcher eindigt.
        self.tasks[index] = Some(task);
        self.containers[index] = Some(id);
        Ok(())
    }
    pub(crate) fn poll(&mut self, context: &mut Context<'_>) {
        for index in 0..WATCHERS {
            if let Some(task) = &mut self.tasks[index]
                && task.as_mut().poll(context).is_ready()
            {
                self.tasks[index] = None;
                self.containers[index] = None;
            }
        }
    }
}
struct Lease<'a>(&'a Cell<bool>);
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}
async fn lease(mail: &Mail) -> Lease<'_> {
    std::future::poll_fn(|_| {
        if mail.reading.get() || mail.event.borrow().is_some() {
            Poll::Pending
        } else {
            mail.reading.set(true);
            Poll::Ready(Lease(&mail.reading))
        }
    })
    .await
}
type Hashes = d::Map<(bool, [u8; 32])>;
fn hashes(files: &d::WireMap<d::Bytes>) -> Result<Hashes> {
    let mut hashes = Hashes::new();
    for (path, data) in files.iter() {
        hashes.insert(
            try_string(path)?,
            (
                data.0.is_some(),
                spin_security::sha256(data.0.as_deref().unwrap_or(&[])),
            ),
        )?;
    }
    Ok(hashes)
}
async fn read(
    docker: &Docker,
    runtime: &d::CapsuleRuntime,
    selection: &d::engine::TrackedSelection,
) -> Result<d::WireMap<d::Bytes>> {
    let mut executor = DockerExecutor;
    let mut future = std::pin::pin!(docker.read_tracked_files(&mut executor, runtime, selection));
    let start = Instant::now();
    std::future::poll_fn(|context| {
        if start.elapsed() > Duration::from_secs(30) {
            Poll::Ready(Err(Error::Invalid("tracked file snapshot timed out")))
        } else {
            future.as_mut().poll(context)
        }
    })
    .await
}
async fn watch(
    mut process: Process,
    docker: &Docker,
    runtime: &d::CapsuleRuntime,
    selection: &d::engine::TrackedSelection,
    mail: &Mail,
) -> Result<()> {
    let mut last = {
        let _lease = lease(mail).await;
        let files = read(docker, runtime, selection).await?;
        let hashes = hashes(&files)?;
        // Always publish the initial snapshot: files may have changed while offline.
        let payload = p::TrackedFilesPayload {
            runtime: runtime.try_clone()?,
            paths: selection.paths.try_clone()?,
            excludes: selection.excludes.try_clone()?,
            files,
        };
        mail.event.replace(Some(WireMessage {
            version: p::PROTOCOL_VERSION,
            r#type: try_string(p::MESSAGE_EVENT)?,
            method: try_string(p::METHOD_TRACKED_CHANGED)?,
            payload: d::RawJson(Some(payload.to_value()?)),
            ..Default::default()
        }));
        hashes
    };
    let mut due = None;
    let mut line = Vec::new();
    let mut bytes = [0; 4096];
    loop {
        // De watcher mag stderr nooit ongelezen vol laten lopen.
        match process.read_error(&mut bytes) {
            Ok(_) => {}
            Err(error) if pending(&error) => {}
            Err(_) => return Err(Error::Invalid("watcher stderr failed")),
        }
        match process.read_output(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                for byte in &bytes[..n] {
                    if *byte == b'\n' {
                        if line == b"CHANGED" {
                            due = Some(Instant::now() + Duration::from_millis(700));
                        }
                        line.clear();
                    } else if line.len() < 128 {
                        d::try_push(&mut line, *byte)?;
                    }
                }
            }
            Err(error) if pending(&error) => {}
            Err(_) => return Err(Error::Invalid("watcher stdout failed")),
        }
        if due.is_some_and(|at| Instant::now() >= at) {
            due = None;
            let _lease = lease(mail).await;
            if let Ok(files) = read(docker, runtime, selection).await {
                let next = hashes(&files)?;
                if next != last {
                    let payload = p::TrackedFilesPayload {
                        runtime: runtime.try_clone()?,
                        paths: selection.paths.try_clone()?,
                        excludes: selection.excludes.try_clone()?,
                        files,
                    };
                    mail.event.replace(Some(WireMessage {
                        version: p::PROTOCOL_VERSION,
                        r#type: try_string(p::MESSAGE_EVENT)?,
                        method: try_string(p::METHOD_TRACKED_CHANGED)?,
                        payload: d::RawJson(Some(payload.to_value()?)),
                        ..Default::default()
                    }));
                    last = next;
                }
            }
        }
        executor::next_round().await;
    }
}
fn pending(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted)
}
