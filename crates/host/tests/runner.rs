//! De echte runnerlus tegen een protocolpeer en een begrensde Docker-CLI-dubbel.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use spin_core::websocket::{self as ws, Decoder, Event, Role};
use spin_domain::{
    self as d, TryClone, Wire,
    protocol::{self as p, WireMessage},
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
struct Peer {
    socket: TcpStream,
    decoder: Decoder,
}
impl Peer {
    fn accept(listener: &TcpListener) -> Self {
        let start = Instant::now();
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && start.elapsed() < Duration::from_secs(8) =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("runner accept: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
            assert!(header.len() <= 32768);
        }
        let header = String::from_utf8(header).unwrap();
        assert!(header.starts_with("GET /api/runner/ws HTTP/1.1\r\n"));
        assert!(header.contains("Authorization: Bearer test-worker-token\r\n"));
        let key = header
            .lines()
            .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
            .unwrap();
        let accept = ws::accept(key).unwrap();
        socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).unwrap();
        Self {
            socket,
            decoder: Decoder::new(Role::Server, p::MAX_MESSAGE_BYTES),
        }
    }
    fn read(&mut self) -> WireMessage {
        let start = Instant::now();
        loop {
            assert!(start.elapsed() < Duration::from_secs(8));
            if let Some(event) = self.decoder.next_event().unwrap() {
                match event {
                    Event::Text(text) => return WireMessage::decode(text.as_bytes()).unwrap(),
                    Event::Ping(data) => {
                        self.socket
                            .write_all(&ws::encode(10, &data, None).unwrap())
                            .unwrap();
                        continue;
                    }
                    Event::Pong(_) => continue,
                    other => panic!("unexpected runner event {other:?}"),
                }
            }
            let mut bytes = [0; 8192];
            let n = self.socket.read(&mut bytes).unwrap();
            assert!(n > 0);
            assert_eq!(self.decoder.push(&bytes[..n]).unwrap(), n);
        }
    }
    fn send(&mut self, message: &WireMessage) {
        self.socket
            .write_all(&ws::encode(1, message.to_json().unwrap().as_bytes(), None).unwrap())
            .unwrap();
    }
    fn answer(&mut self, id: &str) -> WireMessage {
        // Replays kunnen kruisen met de bevestiging van de vorige socketwrite.
        loop {
            let message = self.read();
            if message.id == id {
                return message;
            }
            assert_eq!(message.id, "req-once");
        }
    }
    fn welcome(&mut self) -> WireMessage {
        let hello = self.read();
        assert!(hello.is_supported_hello());
        assert_eq!(hello.instance_id, "host-test");
        self.send(&WireMessage {
            r#type: p::MESSAGE_WELCOME.into(),
            client: Some(d::Client {
                id: "cli-test".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        hello
    }
}
fn request(id: &str, input: &str) -> WireMessage {
    WireMessage {
        version: p::PROTOCOL_VERSION,
        r#type: p::MESSAGE_REQUEST.into(),
        id: id.into(),
        method: p::METHOD_EXECUTE.into(),
        payload: d::RawJson(Some(
            p::ExecutePayload {
                recording: d::Recording {
                    runtime: Some(d::CapsuleRuntime {
                        driver: "docker".into(),
                        container_id: "test-container".into(),
                        status: "recording".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                input: input.into(),
            }
            .to_value()
            .unwrap(),
        )),
        ..Default::default()
    }
}
struct Stop<'a>(&'a AtomicBool);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
struct Directory(std::path::PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn runner_reconnects_without_repeating_work_and_cancellation_releases_the_process() {
    let root = std::env::temp_dir().join(format!("spin-rust-runner-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let _directory = Directory(root.clone());
    let binary = root.join("docker");
    std::fs::write(&binary, r#"#!/bin/sh
case "$1" in
version) printf '28.0.1\n';;
ps) exit 0;;
exec)
  printf '%s\n' "$*" >> "$0.calls"
  case "$*" in
    *spin-watch*) printf 'started\n' >> "$0.watch"; sleep 0.2; printf 'CHANGED\n'; sleep 2; exit 0;;
    *SPIN_FILE*) if [ -f "$0.read" ]; then printf 'SPIN_FILE /root/token bmV3\n'; else touch "$0.read"; printf 'SPIN_FILE /root/token b2xk\n'; fi;;
    *cancel*) exec sleep 30;;
    *) sleep 0.3; printf 'command finished\n';;
  esac;;
*) exit 2;;
esac
"#).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = spin_host::runner::Config {
        env_dir: String::new(),
        advertise_host: "127.0.0.1".into(),
        server: format!("http://{}", listener.local_addr().unwrap()),
        token: "test-worker-token".into(),
        instance_id: "host-test".into(),
        process: "process-test".into(),
        name: "test runner".into(),
        tools: d::List::new(),
        max_workloads: 4,
    };
    let docker = spin_core::docker::Docker::new(binary.to_str().unwrap(), "", "").unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let runner = scope.spawn(|| spin_host::runner::run(config, docker, &stop));
        let _stop = Stop(&stop);
        let mut first = Peer::accept(&listener);
        let hello = first.welcome();
        let command = request("req-once", "slow");
        first.send(&command);
        first.send(&command);
        let start = Instant::now();
        while !binary.with_extension("calls").exists() {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(first);
        let mut second = Peer::accept(&listener);
        let next = second.welcome();
        assert_eq!(hello.process, next.process);
        second.send(&command);
        let answer = second.read();
        assert_eq!(answer.id, "req-once");
        assert!(answer.error.is_empty());
        assert_eq!(
            d::engine::Execution::from_value(answer.payload.0.as_ref().unwrap())
                .unwrap()
                .output,
            "command finished"
        );
        assert_eq!(
            std::fs::read_to_string(binary.with_extension("calls"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        second.send(&request("req-cancel", "cancel"));
        let start = Instant::now();
        while std::fs::read_to_string(binary.with_extension("calls"))
            .unwrap()
            .lines()
            .count()
            < 2
        {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        second.send(&WireMessage {
            r#type: p::MESSAGE_CANCEL.into(),
            id: "req-cancel".into(),
            ..Default::default()
        });
        let answer = second.answer("req-cancel");
        assert_eq!(answer.error, "request cancelled");
        second.send(&request("req-after-cancel", "normal"));
        let answer = second.answer("req-after-cancel");
        assert!(answer.error.is_empty());
        let watch = WireMessage { r#type: p::MESSAGE_REQUEST.into(), id: "req-watch".into(), method: p::METHOD_WATCH_TRACKED.into(),
            payload: d::RawJson(Some(p::TrackedFilesPayload::from_json(br#"{"runtime":{"driver":"docker","container_id":"test-container","status":"ready"},"paths":["/root/token"]}"#).unwrap().to_value().unwrap())), ..Default::default() };
        second.send(&watch);
        assert!(second.answer("req-watch").error.is_empty());
        let initial = second.read();
        assert_eq!(initial.method, p::METHOD_TRACKED_CHANGED);
        let initial =
            p::TrackedFilesPayload::from_value(initial.payload.0.as_ref().unwrap()).unwrap();
        assert_eq!(
            initial.files.get("/root/token").unwrap().0.as_deref(),
            Some(b"old".as_slice())
        );
        let changed = second.read();
        assert_eq!(changed.r#type, p::MESSAGE_EVENT);
        assert_eq!(changed.method, p::METHOD_TRACKED_CHANGED);
        let payload =
            p::TrackedFilesPayload::from_value(changed.payload.0.as_ref().unwrap()).unwrap();
        assert_eq!(payload.runtime.container_id, "test-container");
        assert_eq!(
            payload.files.get("/root/token").unwrap().0.as_deref(),
            Some(b"new".as_slice())
        );
        second.send(&watch);
        assert!(second.answer("req-watch").error.is_empty());
        assert_eq!(
            std::fs::read_to_string(binary.with_extension("watch"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let stopped = second.read();
        assert_eq!(stopped.method, p::METHOD_TRACKED_CHANGED);
        assert!(!stopped.error.is_empty());
        let mut retry = watch.try_clone().unwrap();
        retry.id = "req-watch-retry".into();
        second.send(&retry);
        assert!(second.answer("req-watch-retry").error.is_empty());
        let initial = second.read();
        assert_eq!(initial.method, p::METHOD_TRACKED_CHANGED);
        assert!(initial.error.is_empty());
        let initial =
            p::TrackedFilesPayload::from_value(initial.payload.0.as_ref().unwrap()).unwrap();
        assert_eq!(
            initial.files.get("/root/token").unwrap().0.as_deref(),
            Some(b"new".as_slice())
        );
        stop.store(true, Ordering::Relaxed);
        let goodbye = second.read();
        assert_eq!(goodbye.r#type, p::MESSAGE_GOODBYE);
        runner.join().unwrap().unwrap();
    });
}

#[test]
fn snapshot_streams_survive_reconnect_verify_images_and_cancel_without_loading() {
    let root = std::env::temp_dir().join(format!("spin-rust-transfer-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let _directory = Directory(root.clone());
    let binary = root.join("docker");
    std::fs::write(
        root.join("save.tar"),
        include_bytes!("fixtures/archive/save.tar"),
    )
    .unwrap();
    std::fs::write(
        &binary,
        r#"#!/bin/sh
case "$1" in
version) printf '28.0.1\n';;
ps) exit 0;;
image)
 case "$2" in
  save) cat "$(dirname "$0")/save.tar";;
  load) printf 'load\n' >> "$0.loads"; cat > "$0.loaded";;
  inspect)
   case "$4" in
    '{{.Id}}') printf 'sha256:different-store-id\n';;
    '{{json .RootFS.Layers}}') printf '["sha256:test-layer"]\n';;
    *) exit 2;;
   esac;;
  *) exit 2;;
 esac;;
*) exit 2;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = spin_host::runner::Config {
        env_dir: String::new(),
        advertise_host: "127.0.0.1".into(),
        server: format!("http://{}", listener.local_addr().unwrap()),
        token: "test-worker-token".into(),
        instance_id: "host-test".into(),
        process: "process-transfer".into(),
        name: "transfer runner".into(),
        tools: d::List::new(),
        max_workloads: 4,
    };
    let docker = spin_core::docker::Docker::new(binary.to_str().unwrap(), "", "").unwrap();
    let snapshot = d::CapsuleSnapshot {
        driver: "docker".into(),
        r#ref: "spin/artifact:test".into(),
        root_fs: format!(
            "sha256:{}",
            spin_security::digest_hex(b"sha256:test-layer").unwrap()
        ),
        ..Default::default()
    };
    let rpc = |id: &str, method: &str| WireMessage {
        version: 1,
        r#type: p::MESSAGE_REQUEST.into(),
        id: id.into(),
        method: method.into(),
        payload: d::RawJson(Some(
            p::SnapshotPayload {
                snapshot: snapshot.try_clone().unwrap(),
            }
            .to_value()
            .unwrap(),
        )),
        ..Default::default()
    };
    let chunk = |id: &str, bytes: &[u8]| WireMessage {
        r#type: p::MESSAGE_STREAM_INPUT.into(),
        id: id.into(),
        data: d::Bytes(Some(bytes.to_vec())),
        ..Default::default()
    };
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let runner = scope.spawn(|| spin_host::runner::run(config, docker, &stop));
        let _stop = Stop(&stop);
        let mut peer = Peer::accept(&listener);
        peer.welcome();
        peer.send(&rpc("export", p::METHOD_EXPORT_SNAPSHOT));
        let response = peer.read();
        assert_eq!(response.r#type, p::MESSAGE_RESPONSE);
        assert!(response.error.is_empty());
        assert_eq!(
            p::StreamResponse::from_value(response.payload.0.as_ref().unwrap())
                .unwrap()
                .stream_id,
            "export"
        );
        let mut archive = Vec::new();
        loop {
            let message = peer.read();
            assert_eq!(message.id, "export");
            if message.r#type == p::MESSAGE_STREAM_EXIT {
                assert!(message.error.is_empty(), "{}", message.error);
                assert_eq!(message.execution.unwrap().exit_code, 0);
                break;
            }
            assert_eq!(message.r#type, p::MESSAGE_STREAM_DATA);
            archive.extend_from_slice(message.data.0.as_deref().unwrap());
        }
        assert_eq!(&archive[..2], &[0x1f, 0x8b]);
        peer.send(&rpc("import", p::METHOD_IMPORT_SNAPSHOT));
        assert!(peer.read().error.is_empty());
        let midpoint = archive.len() / 2;
        peer.send(&chunk("import", &archive[..midpoint]));
        // Deze RPC vormt een ontvangstbarrière voor het vorige fragment.
        peer.send(&rpc("presence", p::METHOD_HAS_SNAPSHOT));
        let response = peer.read();
        assert!(
            p::PresenceResult::from_value(response.payload.0.as_ref().unwrap())
                .unwrap()
                .present
        );
        assert!(!binary.with_extension("loads").exists());
        drop(peer);
        let mut peer = Peer::accept(&listener);
        let hello = peer.welcome();
        assert!(hello.streams.contains(&"import".into()));
        peer.send(&rpc("import", p::METHOD_IMPORT_SNAPSHOT));
        assert_eq!(peer.read().r#type, p::MESSAGE_RESPONSE);
        peer.send(&chunk("import", &archive[midpoint..]));
        let close = WireMessage {
            r#type: p::MESSAGE_STREAM_CLOSE.into(),
            id: "import".into(),
            ..Default::default()
        };
        peer.send(&close);
        peer.send(&close);
        let exit = peer.read();
        assert_eq!(exit.r#type, p::MESSAGE_STREAM_EXIT);
        assert!(exit.error.is_empty(), "{}", exit.error);
        assert_eq!(
            std::fs::read(binary.with_extension("loaded")).unwrap(),
            archive
        );
        assert_eq!(
            std::fs::read_to_string(binary.with_extension("loads")).unwrap(),
            "load\n"
        );

        peer.send(&rpc("abandon", p::METHOD_IMPORT_SNAPSHOT));
        assert!(peer.read().error.is_empty());
        peer.send(&chunk("abandon", &archive[..midpoint]));
        peer.send(&WireMessage {
            r#type: p::MESSAGE_CANCEL.into(),
            id: "abandon".into(),
            ..Default::default()
        });
        let exit = peer.read();
        assert_eq!(exit.r#type, p::MESSAGE_STREAM_EXIT);
        assert_eq!(exit.execution.unwrap().exit_code, 1);
        assert_eq!(
            std::fs::read_to_string(binary.with_extension("loads")).unwrap(),
            "load\n"
        );
        stop.store(true, Ordering::Relaxed);
        runner.join().unwrap().unwrap();
    });
}

#[test]
fn agent_stdio_survives_reconnect_replacement_waits_for_cleanup_and_probe_returns_while_alive() {
    let root = std::env::temp_dir().join(format!("spin-rust-agent-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let _directory = Directory(root.clone());
    let binary = root.join("docker");
    std::fs::write(&binary, r#"#!/bin/sh
case "$1" in
version) printf '28.0.1\n';;
ps) exit 0;;
exec)
 case "$*" in
  *'read enabled_pid'*) printf 'cleanup\n' >> "$0.calls"; sleep 0.1;;
  *'exec probe-tool'*)
   printf 'probe\n' >> "$0.calls"
   IFS= read -r request
   printf '%s\n' '{"jsonrpc":"2.0","method":"notification"}' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}'
   exec sleep 30;;
  *)
   printf 'start\n' >> "$0.calls"
   printf 'diagnostic on stderr\n' >&2
   while IFS= read -r request; do printf '%s\n' "$request"; done;;
 esac;;
*) exit 2;;
esac
"#).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = spin_host::runner::Config {
        env_dir: String::new(),
        advertise_host: "127.0.0.1".into(),
        server: format!("http://{}", listener.local_addr().unwrap()),
        token: "test-worker-token".into(),
        instance_id: "host-test".into(),
        process: "process-agent".into(),
        name: "agent runner".into(),
        tools: d::List::new(),
        max_workloads: 4,
    };
    let docker = spin_core::docker::Docker::new(binary.to_str().unwrap(), "", "").unwrap();
    let start = |id: &str, method: &str, command: &str| WireMessage {
        r#type: p::MESSAGE_REQUEST.into(),
        id: id.into(),
        method: method.into(),
        payload: d::RawJson(Some(
            p::EnabledPayload {
                runtime: d::CapsuleRuntime {
                    driver: "docker".into(),
                    container_id: "test-container".into(),
                    status: "ready".into(),
                    ..Default::default()
                },
                enablement: d::Enablement {
                    name: "acp".into(),
                    transport: "stdio".into(),
                    command: command.into(),
                    ..Default::default()
                },
                request: d::RawJson(Some(
                    d::json::Value::from_json(br#"{"jsonrpc":"2.0","id":0,"method":"initialize"}"#)
                        .unwrap(),
                )),
            }
            .to_value()
            .unwrap(),
        )),
        ..Default::default()
    };
    let input = |id: &str, bytes: &[u8]| WireMessage {
        r#type: p::MESSAGE_STREAM_INPUT.into(),
        id: id.into(),
        data: d::Bytes(Some(bytes.to_vec())),
        ..Default::default()
    };
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let runner = scope.spawn(|| spin_host::runner::run(config, docker, &stop));
        let _stop = Stop(&stop);
        let mut peer = Peer::accept(&listener);
        peer.welcome();
        peer.send(&start("agent-1", p::METHOD_START_ENABLED, "agent-tool"));
        assert!(peer.read().error.is_empty());
        peer.send(&input("agent-1", b"first\n"));
        let data = peer.read();
        assert_eq!(data.r#type, p::MESSAGE_STREAM_DATA);
        assert_eq!(data.data.0.unwrap(), b"first\n");
        drop(peer);
        let mut peer = Peer::accept(&listener);
        assert!(peer.welcome().streams.contains(&"agent-1".into()));
        peer.send(&input("agent-1", b"after reconnect\n"));
        assert_eq!(peer.read().data.0.unwrap(), b"after reconnect\n");
        peer.send(&start("agent-2", p::METHOD_START_ENABLED, "agent-tool"));
        let reply = peer.read();
        assert_eq!(reply.id, "agent-2");
        assert!(reply.error.is_empty());
        let replaced = peer.read();
        assert_eq!(replaced.id, "agent-1");
        assert_eq!(replaced.r#type, p::MESSAGE_STREAM_EXIT);
        peer.send(&input("agent-2", b"replacement\n"));
        assert_eq!(peer.read().data.0.unwrap(), b"replacement\n");
        assert_eq!(
            std::fs::read_to_string(binary.with_extension("calls")).unwrap(),
            "start\ncleanup\nstart\n"
        );
        peer.send(&start("probe", p::METHOD_PROBE_ENABLED, "probe-tool"));
        let response = peer.read();
        assert_eq!(response.id, "probe");
        assert!(response.error.is_empty(), "{}", response.error);
        assert_eq!(
            response
                .payload
                .0
                .unwrap()
                .as_object()
                .unwrap()
                .get("id")
                .unwrap()
                .as_i64(),
            Some(0)
        );
        peer.send(&input("agent-2", b"still alive\n"));
        assert_eq!(peer.read().data.0.unwrap(), b"still alive\n");
        stop.store(true, Ordering::Relaxed);
        runner.join().unwrap().unwrap();
        let calls = std::fs::read_to_string(binary.with_extension("calls")).unwrap();
        assert_eq!(calls.lines().filter(|line| *line == "cleanup").count(), 3);
    });
}

/// Leest de handshake en antwoordt met een HTTP-status in plaats van de upgrade.
fn reject(listener: &TcpListener, status: &str) {
    let start = Instant::now();
    let mut socket = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && start.elapsed() < Duration::from_secs(8) =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => panic!("runner accept: {error}"),
        }
    };
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
        assert!(header.len() <= 32768);
    }
    socket
        .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
        .unwrap();
}
#[test]
fn only_a_rejected_token_stops_the_runner_a_busy_identity_waits_and_a_policy_close_reconnects() {
    let root = std::env::temp_dir().join(format!("spin-rust-rejection-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let _directory = Directory(root.clone());
    let binary = root.join("docker");
    std::fs::write(
        &binary,
        "#!/bin/sh\ncase \"$1\" in version) printf '28.0.1\\n';; ps) exit 0;; *) exit 2;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    for case in ["token", "identity", "policy"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let config = spin_host::runner::Config {
            env_dir: String::new(),
            advertise_host: "127.0.0.1".into(),
            server: format!("http://{}", listener.local_addr().unwrap()),
            token: "test-worker-token".into(),
            instance_id: "host-test".into(),
            process: "second-process".into(),
            name: "rejected runner".into(),
            tools: d::List::new(),
            max_workloads: 1,
        };
        let docker = spin_core::docker::Docker::new(binary.to_str().unwrap(), "", "").unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let runner = scope.spawn(|| spin_host::runner::run(config, docker, &stop));
            let _stop = Stop(&stop);
            if case == "token" {
                // Een geweigerd token is de enige fatale afwijzing.
                reject(&listener, "401 Unauthorized");
                let deadline = Instant::now() + Duration::from_secs(3);
                while !runner.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(runner.is_finished(), "a 401 must stop the runner");
                let error = runner
                    .join()
                    .unwrap()
                    .expect_err("a rejected token terminates the runner");
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                return;
            }
            let mut peer = Peer::accept(&listener);
            assert!(peer.read().is_supported_hello());
            if case == "policy" {
                peer.socket
                    .write_all(&ws::encode(8, &1008u16.to_be_bytes(), None).unwrap())
                    .unwrap();
                // Een beleidssluiting zonder identiteitsreden is een gewone breuk:
                // de runner komt na de eerste backoff (1 s) terug.
                let mut again = Peer::accept(&listener);
                assert!(again.read().is_supported_hello());
            } else {
                peer.send(&WireMessage {
                    r#type: "error".into(),
                    error: "runner identity is already connected from another process".into(),
                    ..Default::default()
                });
                // Een bezette identiteit wacht 15 s zonder te stoppen of eerder
                // terug te komen; de oude claim verloopt op de server vanzelf.
                let quiet = Instant::now() + Duration::from_millis(2500);
                while Instant::now() < quiet {
                    assert!(
                        !runner.is_finished(),
                        "identity in use must not stop the runner"
                    );
                    assert!(
                        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                        "identity in use must not reconnect before the retry delay"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            assert!(!runner.is_finished());
            stop.store(true, Ordering::Relaxed);
            runner.join().unwrap().unwrap();
        });
    }
}
