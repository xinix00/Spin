//! Eén runner-eigenaar, een begrensde procespool en een afzonderlijke socketlevensduur.
use crate::{
    executor,
    process::DockerExecutor,
    runner_socket::{self, Socket},
    storage::Random,
};
use spin_core::{
    docker::{Docker, Error as EngineError},
    validation::text,
    websocket::Event,
    worker::{Admission, OPERATIONS, Worker},
};
use spin_domain::{
    self as d, TryClone, Wire,
    json::Value,
    protocol::{self as p, WireMessage},
    try_string,
};
use std::{
    cell::{Cell, RefCell},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

/// De procesidentiteit verandert per boot; instance_id blijft bij de machine/runnernaam.
pub struct Config {
    /// URL van de centrale Spin-server.
    pub server: String,
    /// Gedeeld bearer-token; wordt alleen in de handshake geschreven.
    pub token: String,
    /// Stabiele runner-identiteit.
    pub instance_id: String,
    /// Een verse waarde voor deze specifieke proceslevensduur.
    pub process: String,
    /// Zichtbare naam in de vloot.
    pub name: String,
    /// Geadverteerde tools.
    pub tools: d::List<String>,
    /// Maximaal aantal gelijktijdige capsules, nul betekent onbeperkt.
    pub max_workloads: usize,
    /// Lokale map met benoemde app-envbestanden.
    pub env_dir: String,
    /// Adres waarop gepubliceerde servicepoorten bereikbaar zijn.
    pub advertise_host: String,
}
type Replies = RefCell<[Option<d::Fallible<WireMessage>>; OPERATIONS]>;
struct Reservation<'a>(&'a Cell<usize>);
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}
fn io(error: impl std::error::Error + Send + Sync + 'static) -> std::io::Error {
    std::io::Error::other(error)
}
fn engine_io(error: std::io::Error) -> EngineError {
    match text(format_args!("{error}")) {
        Ok(message) => EngineError::Transport(message),
        Err(error) => error.into(),
    }
}
fn response(id: &str, result: spin_core::docker::Result<Value>) -> d::Fallible<WireMessage> {
    let mut message = WireMessage {
        version: p::PROTOCOL_VERSION,
        r#type: try_string(p::MESSAGE_RESPONSE)?,
        id: try_string(id)?,
        ..Default::default()
    };
    match result {
        Ok(value) => message.payload = d::RawJson(Some(value)),
        Err(error) => message.error = text(format_args!("{error}"))?,
    }
    Ok(message)
}
async fn invoke(
    docker: &Docker,
    request: &WireMessage,
    maximum: usize,
    starting: &Cell<usize>,
    archive: &crate::blob_client::Client,
    apps: &crate::apps::Config,
) -> spin_core::docker::Result<Value> {
    let payload = request.payload.0.as_ref().unwrap_or(&Value::Null);
    let mut executor = DockerExecutor;
    match request.method.as_str() {
        p::METHOD_START_APP | p::METHOD_STOP_APP | p::METHOD_APP_STATUS | p::METHOD_APP_LOGS => {
            let value = p::AppPayload::from_value(payload)?;
            apps.invoke(docker, &request.method, &value)
                .await
                .map_err(engine_io)
        }
        p::METHOD_INSPECT_WORKSPACE | p::METHOD_INSPECT_WORKSPACE_AT => {
            let value = p::WorkspacePathPayload::from_value(payload)?;
            Ok(docker
                .inspect_workspace(&mut executor, &value.runtime, &value.path)
                .await?
                .to_value()?)
        }
        p::METHOD_INSPECT_RANGE => {
            let value = p::InspectRangePayload::from_value(payload)?;
            Ok(docker
                .inspect_workspace_range(&mut executor, &value.runtime, &value.comparison)
                .await?
                .to_value()?)
        }
        p::METHOD_COMPARE_REPOSITORY => {
            let value = p::RepositoryComparePayload::from_value(payload)?;
            Ok(docker
                .compare_repository(&mut executor, &value.comparison)
                .await?
                .to_value()?)
        }
        p::METHOD_BROWSE_REPOSITORY => {
            let value = p::RepositoryBrowsePayload::from_value(payload)?;
            Ok(docker
                .browse_repository(&mut executor, &value.browse)
                .await?
                .to_value()?)
        }
        p::METHOD_ACCEPT_WORKSPACE => {
            let value = p::AcceptWorkspacePayload::from_value(payload)?;
            Ok(docker
                .accept_workspace(&mut executor, &value.runtime, &value.acceptance)
                .await?
                .to_value()?)
        }
        p::METHOD_SYNC_WORKSPACE => {
            let value = p::SyncPayload::from_value(payload)?;
            Ok(docker
                .sync_workspace(&mut executor, &value.runtime, &value.sync)
                .await?
                .to_value()?)
        }
        p::METHOD_MERGE_WORKSPACE => {
            let value = p::MergePayload::from_value(payload)?;
            Ok(docker
                .merge_workspace(&mut executor, &value.runtime, &value.merge)
                .await?
                .to_value()?)
        }
        p::METHOD_ACCEPT_REPOSITORY => {
            let value = p::RepositoryAcceptPayload::from_value(payload)?;
            Ok(docker
                .accept_repository(&mut executor, &value.acceptance)
                .await?
                .to_value()?)
        }
        p::METHOD_MERGE_REPOSITORY => {
            let value = p::RepositoryMergePayload::from_value(payload)?;
            Ok(docker
                .merge_repository(&mut executor, &value.merge)
                .await?
                .to_value()?)
        }
        p::METHOD_ACCEPTS => {
            let live = docker.live_capsules(&mut executor).await?;
            let running = live.compositions.len() + live.recordings.len() + starting.get();
            Ok(p::AcceptsReply {
                accepts: maximum == 0 || running < maximum,
                running: i64::try_from(running).unwrap_or(i64::MAX),
                limit: i64::try_from(maximum).unwrap_or(i64::MAX),
            }
            .to_value()?)
        }
        p::METHOD_START_RECORDING => {
            let value = p::StartRecordingPayload::from_value(payload)?;
            starting.set(starting.get() + 1);
            let _reservation = Reservation(starting);
            let live = docker.live_capsules(&mut executor).await?;
            if maximum > 0
                && !live.recordings.contains(&value.recording.id)
                && live.compositions.len() + live.recordings.len() + starting.get() > maximum
            {
                return Err(EngineError::Invalid(p::RUNNER_FULL));
            }
            if let Some(stack) = &value.stack {
                if !stack.layers.is_empty() && value.parents.len() == 1 {
                    let composition = d::Composition {
                        layers: stack.layers.try_clone()?,
                        ..Default::default()
                    };
                    archive
                        .ensure(docker, &composition, &stack.artifacts)
                        .await
                        .map_err(engine_io)?;
                } else {
                    for parent in value.parents.iter() {
                        archive
                            .pull(docker, &parent.snapshot, 0)
                            .await
                            .map_err(engine_io)?;
                    }
                }
            } else {
                for parent in value.parents.iter() {
                    archive
                        .pull(docker, &parent.snapshot, 0)
                        .await
                        .map_err(engine_io)?;
                }
            }
            Ok(crate::images::start_recording(docker, &value)
                .await
                .map_err(engine_io)?
                .to_value()?)
        }
        p::METHOD_MATERIALIZE => {
            let value = p::MaterializePayload::from_value(payload)?;
            starting.set(starting.get() + 1);
            let _reservation = Reservation(starting);
            let live = docker.live_capsules(&mut executor).await?;
            if maximum > 0
                && !live.compositions.contains(&value.composition.id)
                && live.compositions.len() + live.recordings.len() + starting.get() > maximum
            {
                return Err(EngineError::Invalid(p::RUNNER_FULL));
            }
            archive
                .ensure(docker, &value.composition, &value.artifacts)
                .await
                .map_err(engine_io)?;
            Ok(crate::images::materialize(docker, &value)
                .await
                .map_err(engine_io)?
                .to_value()?)
        }
        p::METHOD_SEAL => {
            let value = p::RecordingPayload::from_value(payload)?;
            Ok(crate::images::seal(docker, &value.recording)
                .await
                .map_err(engine_io)?
                .to_value()?)
        }
        p::METHOD_INJECT_ATTACHMENTS => {
            let value = p::InjectAttachmentsPayload::from_value(payload)?;
            docker
                .inject_attachments(&mut executor, &value.runtime, &value.attachments)
                .await?;
            Ok(Value::Null)
        }
        p::METHOD_BUNDLE_DELIVERABLE => {
            let value = p::BundleDeliverablePayload::from_value(payload)?;
            Ok(crate::bundles::create(archive, docker, &value)
                .await
                .map_err(engine_io)?
                .to_value()?)
        }
        p::METHOD_PLACE_DELIVERABLE => {
            let value = p::PlaceDeliverablePayload::from_value(payload)?;
            crate::bundles::place(archive, docker, &value)
                .await
                .map_err(engine_io)?;
            Ok(Value::Null)
        }
        p::METHOD_ARCHIVE_SNAPSHOT => {
            let value = p::SnapshotPayload::from_value(payload)?;
            Ok(archive
                .archive(docker, &value.snapshot)
                .await
                .map_err(engine_io)?
                .to_value()?)
        }
        p::METHOD_HAS_SNAPSHOT => {
            let value = p::SnapshotPayload::from_value(payload)?;
            Ok(p::PresenceResult {
                present: crate::images::has_snapshot(docker, &value.snapshot)
                    .await
                    .map_err(engine_io)?,
            }
            .to_value()?)
        }
        p::METHOD_CAPSULE_CHANGES => {
            let value = p::RuntimePayload::from_value(payload)?;
            Ok(crate::images::changes(docker, &value.runtime)
                .await
                .map_err(engine_io)?
                .to_value()?)
        }
        p::METHOD_EXECUTE => {
            let value = p::ExecutePayload::from_value(payload)?;
            Ok(docker
                .execute(&mut executor, &value.recording, &value.input)
                .await?
                .to_value()?)
        }
        p::METHOD_PROBE_ENABLED => {
            let value = p::EnabledPayload::from_value(payload)?;
            crate::runner_agent::probe(docker, &value)
                .await
                .map_err(engine_io)
        }
        p::METHOD_REMOVE_CAPSULES => {
            let value = p::RemoveCapsulesPayload::from_value(payload)?;
            docker
                .remove_capsules(&mut executor, &value.compositions, &value.recordings)
                .await?;
            Ok(Value::Null)
        }
        p::METHOD_CANCEL_RECORDING => {
            docker
                .cancel(
                    &mut executor,
                    &p::RecordingPayload::from_value(payload)?.recording,
                )
                .await?;
            Ok(Value::Null)
        }
        p::METHOD_STOP => {
            docker
                .stop(
                    &mut executor,
                    &p::RuntimePayload::from_value(payload)?.runtime,
                )
                .await?;
            Ok(Value::Null)
        }
        p::METHOD_READ_TRACKED => {
            let value = p::TrackedFilesPayload::from_value(payload)?;
            Ok(docker
                .read_tracked_files(
                    &mut executor,
                    &value.runtime,
                    &d::engine::TrackedSelection {
                        paths: value.paths,
                        excludes: value.excludes,
                    },
                )
                .await?
                .to_value()?)
        }
        p::METHOD_WRITE_TRACKED => {
            let value = p::TrackedFilesPayload::from_value(payload)?;
            docker
                .write_tracked_files(&mut executor, &value.runtime, &value.files)
                .await?;
            Ok(Value::Null)
        }
        p::METHOD_REMOVE_SNAPSHOT => {
            let value = p::SnapshotPayload::from_value(payload)?;
            docker
                .remove_snapshot(&mut executor, &value.snapshot)
                .await?;
            Ok(Value::Null)
        }
        _ => Err(EngineError::Invalid("runner method is not available")),
    }
}
/// Draait totdat de eigenaar stop zet. Een socketbreuk laat de procespool ongemoeid.
pub fn run(config: Config, docker: Docker, stop: &AtomicBool) -> std::io::Result<()> {
    let result = run_owned(config, &docker, stop);
    // Alle futures zijn nu gevallen. Ook op een vroege fout blijven hun
    // containerprocessen van deze eigenaar totdat cleanup klaar is of de
    // begrensde shutdowntermijn verstrijkt.
    let deadline = Instant::now() + Duration::from_secs(30);
    executor::block_on(async {
        let mut retry = Instant::now();
        while docker.needs_cleanup() && Instant::now() < deadline {
            if Instant::now() < retry {
                executor::next_round().await;
                continue;
            }
            let mut cleanup_executor = DockerExecutor;
            let mut future = std::pin::pin!(docker.clean_one(&mut cleanup_executor));
            let outcome = std::future::poll_fn(|cx| {
                if Instant::now() >= deadline {
                    return std::task::Poll::Ready(Err(EngineError::Invalid(
                        "runner cleanup shutdown deadline",
                    )));
                }
                future.as_mut().poll(cx)
            })
            .await;
            if let Err(error) = outcome {
                eprintln!("SPIN_CLEANUP_RETRY error={error}");
                retry = Instant::now() + Duration::from_secs(1);
            }
        }
    });
    if docker.needs_cleanup() {
        eprintln!("SPIN_CLEANUP_INCOMPLETE");
    }
    result
}
fn run_owned(config: Config, docker: &Docker, stop: &AtomicBool) -> std::io::Result<()> {
    let apps = crate::apps::Config::new(config.env_dir, config.advertise_host)?;
    let archive = crate::blob_client::Client::new(&config.server, &config.token)?;
    let endpoint = runner_socket::endpoint(&config.server)?;
    // Resolutie is bootwerk; de lus probeert hoogstens zestien bekende adressen.
    let mut addresses: Vec<SocketAddr> = Vec::new();
    for address in (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()?
        .take(16)
    {
        d::try_push(&mut addresses, address).map_err(io)?;
    }
    if addresses.is_empty() {
        return Err(std::io::Error::other("runner server has no addresses"));
    }
    let mut random = Random::open()?;
    let engine = executor::block_on(docker.probe(&mut DockerExecutor)).map_err(io)?;
    let mut hello = WireMessage {
        version: p::PROTOCOL_VERSION,
        r#type: try_string(p::MESSAGE_HELLO).map_err(io)?,
        instance_id: config.instance_id,
        process: config.process,
        name: config.name,
        capabilities: d::ClientCapabilities {
            os: try_string(std::env::consts::OS).map_err(io)?,
            arch: try_string(match std::env::consts::ARCH {
                "aarch64" => "arm64",
                "x86_64" => "amd64",
                arch => arch,
            })
            .map_err(io)?,
            tools: config.tools,
            engine,
            max_workloads: i64::try_from(config.max_workloads).map_err(io)?,
            ..Default::default()
        },
        streams_reported: true,
        ..Default::default()
    };
    let replies: Replies = RefCell::new(std::array::from_fn(|_| None));
    let starting = Cell::new(0);
    let watch_mail = crate::runner_watch::Mail::new();
    let stream_mail = crate::runner_stream::Mail::new();
    let mut watches = crate::runner_watch::Watches::new(&watch_mail, docker);
    let mut streams = crate::runner_stream::Streams::new(&stream_mail, docker, &archive);
    let mut tasks: [Option<executor::Task<'_>>; OPERATIONS] = std::array::from_fn(|_| None);
    let mut ids: [Option<String>; OPERATIONS] = std::array::from_fn(|_| None);
    let mut owner = Worker::new();
    let mut socket = None;
    let connected =
        RefCell::new(None::<std::io::Result<(Socket, Option<d::engine::LiveCapsules>)>>);
    let mut connecting: Option<executor::Task<'_>> = None;
    let mut hello_sent = false;
    let mut welcomed = false;
    let mut control: Option<(u8, Vec<u8>)> = None;
    let mut next_connect = Instant::now();
    let mut ping_at = Instant::now();
    let mut dial_index = 0;
    let mut context = Context::from_waker(Waker::noop());
    let mut cleanup: Option<executor::Task<'_>> = None;
    let mut cleanup_at = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if let Some(task) = &mut cleanup
            && task.as_mut().poll(&mut context).is_ready()
        {
            cleanup = None;
            cleanup_at = Instant::now() + Duration::from_secs(1);
        }
        if cleanup.is_none() && docker.needs_cleanup() && Instant::now() >= cleanup_at {
            cleanup = Some(executor::task(async {
                if let Err(error) = docker.clean_one(&mut DockerExecutor).await {
                    eprintln!("SPIN_CLEANUP_RETRY error={error}");
                }
            })?);
        }
        watches.poll(&mut context);
        let event_sent = match watch_mail.event.borrow().as_ref() {
            Some(event) => owner.enqueue(event).map_err(io)?,
            None => false,
        };
        if event_sent {
            watch_mail.event.replace(None);
        }
        for index in 0..OPERATIONS {
            if let Some(id) = &ids[index]
                && owner.cancelled(id)
                && tasks[index].is_some()
            {
                tasks[index] = None;
                replies.borrow_mut()[index] =
                    Some(response(id, Err(EngineError::Invalid("request cancelled"))));
            }
            if let Some(task) = &mut tasks[index]
                && task.as_mut().poll(&mut context).is_ready()
            {
                tasks[index] = None;
            }
            if let Some(result) = replies.borrow_mut()[index].as_ref()
                && owner
                    .finish(
                        result
                            .as_ref()
                            .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?,
                    )
                    .map_err(io)?
            {
                // De RefCell-lening is aan het einde van deze if-conditie weg.
                ids[index] = None;
            }
            if ids[index].is_none() {
                replies.borrow_mut()[index] = None;
            }
        }
        streams.poll(&mut context, &mut owner)?;
        if let Some(task) = &mut connecting
            && task.as_mut().poll(&mut context).is_ready()
        {
            connecting = None;
            match connected.borrow_mut().take() {
                Some(Ok((connection, capsules))) => {
                    socket = Some(connection);
                    hello.capsules = capsules;
                    hello_sent = false;
                    welcomed = false;
                    control = None;
                }
                Some(Err(error)) => {
                    eprintln!("SPIN_RUNNER_CONNECT_FAILED error={error}");
                    next_connect = Instant::now() + Duration::from_secs(1);
                }
                None => return Err(std::io::Error::other("runner dial produced no result")),
            }
        }
        if socket.is_none() && connecting.is_none() && Instant::now() >= next_connect {
            let address = addresses[dial_index % addresses.len()];
            dial_index = dial_index.wrapping_add(1);
            match TcpStream::connect_timeout(&address, Duration::from_millis(500)) {
                Ok(stream) => {
                    let endpoint = &endpoint;
                    let token = &config.token;
                    let connected = &connected;
                    connecting = Some(executor::task(async move {
                        let result = match Socket::connect(stream, endpoint, token).await {
                            Ok(socket) => {
                                Ok((socket, docker.live_capsules(&mut DockerExecutor).await.ok()))
                            }
                            Err(error) => Err(error),
                        };
                        connected.replace(Some(result));
                    })?);
                }
                Err(_) => next_connect = Instant::now() + Duration::from_secs(1),
            }
        }
        let result = (|| -> std::io::Result<()> {
            let Some(connection) = &mut socket else {
                return Ok(());
            };
            let event = connection.poll(&mut context)?;
            if let Some(ticket) = connection.acknowledged() {
                owner.acknowledge(ticket);
            }
            if connection.ready() && !hello_sent && connection.idle_writer() {
                hello.streams = streams.ids(&owner).map_err(io)?;
                hello_sent = connection.send(
                    1,
                    hello.to_json().map_err(io)?.as_bytes(),
                    None,
                    &mut random,
                )?;
                // Een latere reconnect mag geen inmiddels verouderde capsulelijst publiceren.
                hello.capsules = None;
            }
            if let Some(event) = event {
                match event {
                    Event::Ping(data) => {
                        control = Some((10, data));
                    }
                    Event::Pong(_) => {}
                    Event::Close(body) => {
                        if body.get(..2) == Some(&1008u16.to_be_bytes()) {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::PermissionDenied,
                                "runner rejected by server policy; check token and duplicate identity",
                            ));
                        }
                        return Err(std::io::ErrorKind::ConnectionAborted.into());
                    }
                    Event::Text(value) => dispatch(
                        value.as_bytes(),
                        &mut welcomed,
                        &mut owner,
                        &mut tasks,
                        &mut ids,
                        &replies,
                        docker,
                        config.max_workloads,
                        &starting,
                        &archive,
                        &apps,
                        &mut watches,
                        &mut streams,
                    )?,
                    Event::Binary(value) => dispatch(
                        &value,
                        &mut welcomed,
                        &mut owner,
                        &mut tasks,
                        &mut ids,
                        &replies,
                        docker,
                        config.max_workloads,
                        &starting,
                        &archive,
                        &apps,
                        &mut watches,
                        &mut streams,
                    )?,
                }
            }
            if connection.idle_writer() && hello_sent {
                if let Some((opcode, bytes)) = control.take() {
                    connection.send(opcode, &bytes, None, &mut random)?;
                } else if welcomed {
                    if ping_at.elapsed() >= Duration::from_secs(30) {
                        connection.send(9, b"", None, &mut random)?;
                        ping_at = Instant::now();
                    } else if let Some((ticket, bytes)) = owner.outbound() {
                        connection.send(1, bytes, Some(ticket), &mut random)?;
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return Err(error);
            }
            socket = None;
            next_connect = Instant::now() + Duration::from_secs(1);
        }
        executor::idle();
    }
    if welcomed && let Some(connection) = &mut socket {
        // Bound inventory and the final write; shutdown never waits on a wedged Docker daemon.
        let inventory_deadline = Instant::now() + Duration::from_millis(200);
        let no_capsules = executor::block_on(async {
            let mut executor = DockerExecutor;
            let mut inventory = std::pin::pin!(docker.live_capsules(&mut executor));
            std::future::poll_fn(|cx| {
                if Instant::now() >= inventory_deadline {
                    return Poll::Ready(false);
                }
                inventory.as_mut().poll(cx).map(|value| {
                    value.is_ok_and(|v| v.recordings.is_empty() && v.compositions.is_empty())
                })
            })
            .await
        });
        let goodbye = WireMessage {
            version: p::PROTOCOL_VERSION,
            r#type: try_string(p::MESSAGE_GOODBYE).map_err(io)?,
            idle: no_capsules && owner.active() == 0 && streams.ids(&owner).map_err(io)?.is_empty(),
            ..Default::default()
        }
        .to_json()
        .map_err(io)?;
        let deadline = Instant::now() + Duration::from_millis(350);
        let mut sent = false;
        while Instant::now() < deadline {
            if connection.poll(&mut context).is_err() {
                break;
            }
            if connection.idle_writer() {
                if sent {
                    break;
                }
                match connection.send(1, goodbye.as_bytes(), None, &mut random) {
                    Ok(value) => sent = value,
                    Err(_) => break,
                }
            }
            executor::idle();
        }
    }
    Ok(())
}
// Alle argumenten zijn geleende componenten van dezelfde runner-eigenaar, zonder locks.
#[allow(clippy::too_many_arguments)]
fn dispatch<'a>(
    bytes: &[u8],
    welcomed: &mut bool,
    owner: &mut Worker,
    tasks: &mut [Option<executor::Task<'a>>; OPERATIONS],
    ids: &mut [Option<String>; OPERATIONS],
    replies: &'a Replies,
    docker: &'a Docker,
    maximum: usize,
    starting: &'a Cell<usize>,
    archive: &'a crate::blob_client::Client,
    apps: &'a crate::apps::Config,
    watches: &mut crate::runner_watch::Watches<'a>,
    streams: &mut crate::runner_stream::Streams<'a>,
) -> std::io::Result<()> {
    let request = WireMessage::decode(bytes).map_err(io)?;
    if !*welcomed {
        if request.r#type != p::MESSAGE_WELCOME || request.client.is_none() {
            if request
                .error
                .contains("already connected from another process")
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "runner identity is already connected from another process",
                ));
            }
            return Err(std::io::Error::other("runner welcome rejected"));
        }
        *welcomed = true;
        return Ok(());
    }
    match request.r#type.as_str() {
        p::MESSAGE_CANCEL => {
            owner.cancel(&request.id);
            streams.close(&request.id, true)?;
        }
        p::MESSAGE_STREAM_INPUT => streams.input(&request)?,
        p::MESSAGE_STREAM_CLOSE => streams.close(&request.id, false)?,
        p::MESSAGE_STREAM_RESIZE => streams.resize(&request)?,
        p::MESSAGE_REQUEST => match owner.begin(&request).map_err(io)? {
            Admission::Start => {
                let index = ids
                    .iter()
                    .position(Option::is_none)
                    .ok_or_else(|| std::io::Error::other("runner operation table is full"))?;
                let id = match request.id.try_clone() {
                    Ok(id) => id,
                    Err(error) => {
                        owner.release_unstarted(&request.id);
                        return Err(io(error));
                    }
                };
                if request.method == p::METHOD_WATCH_TRACKED {
                    let result = p::TrackedFilesPayload::from_value(
                        request.payload.0.as_ref().unwrap_or(&Value::Null),
                    )
                    .map_err(EngineError::from)
                    .and_then(|value| watches.install(value))
                    .map(|()| Value::Null);
                    replies.borrow_mut()[index] = Some(response(&id, result));
                    ids[index] = Some(id);
                    return Ok(());
                }
                if matches!(
                    request.method.as_str(),
                    p::METHOD_EXPORT_SNAPSHOT
                        | p::METHOD_IMPORT_SNAPSHOT
                        | p::METHOD_PULL_SNAPSHOT
                        | p::METHOD_START_ENABLED
                        | p::METHOD_START_INTERACTIVE
                ) {
                    let result = streams.install(&request).map_err(engine_io);
                    replies.borrow_mut()[index] = Some(response(&id, result));
                    ids[index] = Some(id);
                    return Ok(());
                }
                let task = executor::task(async move {
                    let result = invoke(docker, &request, maximum, starting, archive, apps).await;
                    replies.borrow_mut()[index] = Some(response(&request.id, result));
                });
                match task {
                    Ok(task) => {
                        ids[index] = Some(id);
                        tasks[index] = Some(task);
                    }
                    Err(error) => {
                        owner.release_unstarted(&id);
                        return Err(error);
                    }
                }
            }
            Admission::Busy => {
                let reply = response(
                    &request.id,
                    Err(EngineError::Invalid("runner operation queue is full")),
                )
                .map_err(io)?;
                if !owner.enqueue(&reply).map_err(io)? {
                    return Err(std::io::Error::other("runner outbox is full"));
                }
            }
            Admission::InFlight | Admission::Replayed => {}
        },
        _ => {}
    }
    Ok(())
}
