//! De echte serverbinary: browserauth, cacheheaders, gelijktijdigheid en herstart.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use spin_host::storage::Random;
use spin_store::IdSource;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Server {
    child: Child,
    addr: String,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn start(root: &Path) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_spin-server"))
        .args(["--addr", "127.0.0.1:0", "--data-dir"])
        .arg(root)
        .env_remove("SPIN_MASTER_KEY")
        .env_remove("SPIN_MASTER_KEY_FILE")
        .env_remove("SPIN_PUBLIC_URL")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let addr = line
        .split_whitespace()
        .find_map(|part| part.strip_prefix("addr="))
        .expect("listener marker")
        .to_string();
    Server { child, addr }
}
fn connect(server: &Server) -> TcpStream {
    let socket = TcpStream::connect(&server.addr).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    socket
}
fn send(socket: &mut TcpStream, method: &str, path: &str, headers: &[(&str, &str)], body: &str) {
    write!(socket, "{method} {path} HTTP/1.1\r\nHost: spin.test\r\nConnection: close\r\nContent-Length: {}\r\n", body.len()).unwrap();
    for (name, value) in headers {
        write!(socket, "{name}: {value}\r\n").unwrap();
    }
    write!(socket, "\r\n{body}").unwrap();
}
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}
impl Reply {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map_or("", |(_, v)| v)
    }
}
fn receive(socket: &mut TcpStream) -> Reply {
    let mut raw = String::new();
    socket.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").expect("HTTP headers");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|line| {
            let (k, v) = line.split_once(':').unwrap();
            (k.to_string(), v.trim().to_string())
        })
        .collect();
    Reply {
        status,
        headers,
        body: body.to_string(),
    }
}
fn call(server: &Server, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut socket = connect(server);
    send(&mut socket, method, path, headers, body);
    receive(&mut socket)
}
#[test]
fn browser_setup_assets_restart_and_logout_over_real_sockets() {
    let root: PathBuf =
        std::env::temp_dir().join(Random::open().unwrap().next("spin-http").unwrap());
    let server = start(&root);
    let mut slow = connect(&server);
    slow.write_all(b"GET / HTTP/1.1\r\nHost:").unwrap();
    let status = call(&server, "GET", "/api/auth/status", &[], "");
    assert_eq!(status.status, 200);
    assert!(status.body.contains("\"configured\":false"));
    // Sent the way cloudflared forwards a browser POST: chunked, no length.
    let mut pending = connect(&server);
    let (head, tail) = r#"{"username":"Derek","password":"a-long-password"}"#.split_at(10);
    write!(
        pending,
        "POST /api/auth/setup HTTP/1.1\r\nHost: spin.test\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{head}\r\n{:x}\r\n{tail}\r\n0\r\n\r\n",
        head.len(),
        tail.len()
    )
    .unwrap();
    let before = Instant::now();
    let health = call(&server, "GET", "/healthz", &[], "");
    assert_eq!(health.status, 200);
    assert!(before.elapsed() < Duration::from_secs(2));
    let setup = receive(&mut pending);
    assert_eq!(setup.status, 201, "{}", setup.body);
    let cookie = setup
        .header("Set-Cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    // Authentication is answered before consuming a multi-gigabyte legacy body.
    let mut bulk = connect(&server);
    write!(bulk, "POST /api/restore HTTP/1.1\r\nHost: spin.test\r\nContent-Length: 2147483648\r\nConnection: close\r\n\r\n").unwrap();
    let denied = receive(&mut bulk);
    assert_eq!(denied.status, 401, "{}", denied.body);
    let unauthorized = call(&server, "GET", "/api/state", &[], "");
    assert_eq!(unauthorized.status, 401);
    let state = call(&server, "GET", "/api/state", &[("Cookie", &cookie)], "");
    assert_eq!(state.status, 200);
    assert!(!state.body.contains("password_hash"));
    for name in [
        "Cache-Control",
        "CDN-Cache-Control",
        "Cloudflare-CDN-Cache-Control",
    ] {
        assert!(state.header(name).contains("no-store"));
    }
    let html = call(&server, "GET", "/", &[], "");
    assert_eq!(html.status, 200);
    assert!(!html.body.contains("__SPIN_UI_VERSION__"));
    assert!(html.header("Cache-Control").contains("no-store"));
    let version = include_str!("../../runtime/ui/VERSION").trim();
    let css = call(
        &server,
        "GET",
        &format!("/assets/v{version}/spin.css"),
        &[],
        "",
    );
    assert_eq!(css.status, 200);
    assert!(css.header("Cache-Control").contains("immutable"));
    let old = call(&server, "GET", "/assets/v0/spin.css", &[], "");
    assert_eq!(old.status, 200);
    assert!(
        old.header("Cloudflare-CDN-Cache-Control")
            .contains("no-store")
    );
    drop(server);
    let server = start(&root);
    let restored = call(
        &server,
        "GET",
        "/api/auth/status",
        &[("Cookie", &cookie)],
        "",
    );
    assert_eq!(restored.status, 200);
    assert!(restored.body.contains("\"authenticated\":true"));
    let fresh_cookie = restored.header("Set-Cookie").split(';').next().unwrap();
    assert_ne!(fresh_cookie, cookie);
    let json = spin_domain::json::parse(restored.body.as_bytes()).unwrap();
    let csrf = json
        .as_object()
        .unwrap()
        .get("csrf_token")
        .unwrap()
        .as_str()
        .unwrap();
    let mut stream = connect(&server);
    send(
        &mut stream,
        "GET",
        "/api/state/ws",
        &[
            ("Cookie", fresh_cookie),
            ("Origin", "http://spin.test"),
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ],
        "",
    );
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
        assert!(head.len() < 8192);
    }
    let head = String::from_utf8(head).unwrap();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
    let mut frames =
        spin_core::websocket::Decoder::new(spin_core::websocket::Role::Client, 16 << 20);
    let first = next_frame(&mut stream, &mut frames);
    assert!(matches!(first, spin_core::websocket::Event::Text(_)));
    stream
        .write_all(&spin_core::websocket::encode(9, b"browser-ping", Some([1, 2, 3, 4])).unwrap())
        .unwrap();
    assert_eq!(
        next_frame(&mut stream, &mut frames),
        spin_core::websocket::Event::Pong(b"browser-ping".to_vec())
    );
    let repository = call(
        &server,
        "POST",
        "/api/git/repositories",
        &[("Cookie", fresh_cookie), ("X-Spin-CSRF", csrf)],
        r#"{"operator":"spoofed","name":"websocket-test","remote_url":"https://example.test/spin.git","credential_scope":"public"}"#,
    );
    assert_eq!(repository.status, 201, "{}", repository.body);
    let update = next_frame(&mut stream, &mut frames);
    match update {
        spin_core::websocket::Event::Text(json) => {
            assert!(json.contains("websocket-test"));
            assert!(!json.contains("spoofed"));
        }
        _ => panic!("expected state update"),
    }
    let logout = call(
        &server,
        "POST",
        "/api/auth/logout",
        &[("Cookie", fresh_cookie), ("X-Spin-CSRF", csrf)],
        "",
    );
    assert_eq!(logout.status, 204);
    let closed = next_frame(&mut stream, &mut frames);
    assert_eq!(
        closed,
        spin_core::websocket::Event::Close(1008_u16.to_be_bytes().to_vec())
    );
    assert_eq!(
        logout
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("Set-Cookie"))
            .count(),
        2
    );
    assert_eq!(
        call(
            &server,
            "GET",
            "/api/state",
            &[("Cookie", fresh_cookie)],
            ""
        )
        .status,
        401
    );
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

fn next_frame(
    socket: &mut TcpStream,
    decoder: &mut spin_core::websocket::Decoder,
) -> spin_core::websocket::Event {
    loop {
        if let Some(event) = decoder.next_event().unwrap() {
            return event;
        }
        let mut bytes = [0; 8192];
        let count = socket.read(&mut bytes).unwrap();
        assert!(count > 0, "unexpected WebSocket EOF");
        assert_eq!(decoder.push(&bytes[..count]).unwrap(), count);
    }
}

fn upgrade(server: &Server, authorization: &str) -> (TcpStream, spin_core::websocket::Decoder) {
    let mut socket = connect(server);
    send(
        &mut socket,
        "GET",
        "/api/runner/ws",
        &[
            ("Authorization", authorization),
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ],
        "",
    );
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
        assert!(head.len() < 8192);
    }
    assert!(String::from_utf8(head).unwrap().starts_with("HTTP/1.1 101"));
    (
        socket,
        spin_core::websocket::Decoder::new(spin_core::websocket::Role::Client, 24 << 20),
    )
}
fn runner_send(socket: &mut TcpStream, message: &str) {
    socket
        .write_all(
            &spin_core::websocket::encode(1, message.as_bytes(), Some([2, 4, 6, 8])).unwrap(),
        )
        .unwrap();
}
fn runner_receive(
    socket: &mut TcpStream,
    decoder: &mut spin_core::websocket::Decoder,
) -> spin_domain::protocol::WireMessage {
    use spin_domain::Wire;
    let spin_core::websocket::Event::Text(json) = next_frame(socket, decoder) else {
        panic!("expected runner message")
    };
    spin_domain::protocol::WireMessage::from_json(json.as_bytes()).unwrap()
}
#[test]
fn runner_reconnects_replays_and_rotates_over_real_sockets() {
    use spin_domain::{Wire, protocol as p};
    let root = std::env::temp_dir().join(Random::open().unwrap().next("spin-runner-http").unwrap());
    let server = start(&root);
    let setup = call(
        &server,
        "POST",
        "/api/auth/setup",
        &[],
        r#"{"username":"Derek","password":"a-long-password"}"#,
    );
    assert_eq!(setup.status, 201);
    let cookie = setup.header("Set-Cookie").split(';').next().unwrap();
    let auth = spin_domain::json::parse(setup.body.as_bytes()).unwrap();
    let csrf = auth
        .as_object()
        .unwrap()
        .get("csrf_token")
        .unwrap()
        .as_str()
        .unwrap();
    let headers = [("Cookie", cookie), ("X-Spin-CSRF", csrf)];
    let reply = call(&server, "GET", "/api/runners/token", &headers, "");
    assert_eq!(reply.status, 200);
    let token = spin_domain::json::parse(reply.body.as_bytes()).unwrap();
    let token = token
        .as_object()
        .unwrap()
        .get("token")
        .unwrap()
        .as_str()
        .unwrap();
    let bearer = format!("Bearer {token}");
    assert_eq!(
        call(
            &server,
            "GET",
            "/api/state",
            &[("Authorization", &bearer)],
            ""
        )
        .status,
        401
    );
    assert_eq!(
        call(
            &server,
            "POST",
            "/api/runners/token",
            &[("Authorization", &bearer)],
            ""
        )
        .status,
        401
    );
    let (mut first, mut frames) = upgrade(&server, &bearer);
    let hello = r#"{"type":"hello","version":1,"instance_id":"host-a","name":"A","process":"process-1","capabilities":{"engine":{"available":true,"driver":"docker"}},"streams_reported":true,"capsules":{"compositions":["orphan"],"recordings":[]}}"#;
    runner_send(&mut first, hello);
    let welcome = runner_receive(&mut first, &mut frames);
    assert_eq!(welcome.r#type, p::MESSAGE_WELCOME);
    let client = welcome.client.unwrap();
    let cleanup = runner_receive(&mut first, &mut frames);
    assert_eq!(cleanup.method, p::METHOD_REMOVE_CAPSULES);
    let state = call(&server, "GET", "/api/state", &headers, "");
    assert!(state.body.contains("runner/docker"));
    let (mut second, mut frames2) = upgrade(&server, &bearer);
    runner_send(
        &mut second,
        r#"{"type":"hello","version":1,"instance_id":"host-a","name":"A","process":"process-1","capabilities":{"engine":{"available":true,"driver":"docker"}}}"#,
    );
    let second_welcome = runner_receive(&mut second, &mut frames2);
    assert_eq!(second_welcome.client.unwrap().id, client.id);
    assert_eq!(runner_receive(&mut second, &mut frames2).id, cleanup.id);
    assert!(matches!(
        next_frame(&mut first, &mut frames),
        spin_core::websocket::Event::Close(_)
    ));
    runner_send(
        &mut second,
        &p::WireMessage {
            r#type: "response".into(),
            id: cleanup.id,
            ..Default::default()
        }
        .to_json()
        .unwrap(),
    );
    let (mut duplicate, mut frames3) = upgrade(&server, &bearer);
    runner_send(&mut duplicate, &hello.replace("process-1", "process-2"));
    assert!(matches!(
        next_frame(&mut duplicate, &mut frames3),
        spin_core::websocket::Event::Close(_)
    ));
    recording_and_external_session(
        &server,
        &mut second,
        &mut frames2,
        &headers,
        &bearer,
        &client.id,
    );
    let drain = call(
        &server,
        "POST",
        &format!("/api/clients/{}/drain", client.id),
        &headers,
        "",
    );
    assert_eq!(drain.status, 200);
    assert!(drain.body.contains("\"draining\":true"));
    let fresh = call(&server, "POST", "/api/runners/token", &headers, "");
    assert_eq!(fresh.status, 200);
    assert!(!fresh.body.contains(token));
    assert!(matches!(
        next_frame(&mut second, &mut frames2),
        spin_core::websocket::Event::Close(_)
    ));
    assert_eq!(
        call(
            &server,
            "POST",
            "/api/clients/register",
            &[("Authorization", &bearer)],
            r#"{"name":"old token"}"#
        )
        .status,
        401
    );
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

// De fixture houdt browserverzoek en runnerantwoord expliciet naast elkaar.
#[allow(clippy::too_many_arguments)]
fn drive_capsule(
    server: &Server,
    runner: &mut TcpStream,
    frames: &mut spin_core::websocket::Decoder,
    headers: &[(&str, &str)],
    path: &str,
    body: &str,
    method: &str,
    payload: &str,
) -> Reply {
    use spin_domain::Wire;
    let mut browser = connect(server);
    send(
        &mut browser,
        if path.ends_with("/changes") {
            "GET"
        } else {
            "POST"
        },
        path,
        headers,
        body,
    );
    let mut request = runner_receive(runner, frames);
    if request.method == "capsule.accepts" {
        runner_send(
            runner,
            &spin_domain::protocol::WireMessage {
                r#type: "response".into(),
                id: request.id,
                payload: spin_domain::RawJson(Some(
                    spin_domain::json::parse(br#"{"accepts":true,"running":0,"limit":8}"#).unwrap(),
                )),
                ..Default::default()
            }
            .to_json()
            .unwrap(),
        );
        request = runner_receive(runner, frames);
    }
    assert_eq!(request.method, method);
    runner_send(
        runner,
        &spin_domain::protocol::WireMessage {
            r#type: "response".into(),
            id: request.id,
            payload: spin_domain::RawJson(Some(
                spin_domain::json::parse(payload.as_bytes()).unwrap(),
            )),
            ..Default::default()
        }
        .to_json()
        .unwrap(),
    );
    if method == "capsule.seal" {
        let archive = runner_receive(runner, frames);
        assert_eq!(archive.method, "snapshot.archive");
        let snapshot =
            spin_domain::protocol::SnapshotPayload::from_value(archive.payload.0.as_ref().unwrap())
                .unwrap()
                .snapshot;
        runner_send(
            runner,
            &spin_domain::protocol::WireMessage {
                r#type: "response".into(),
                id: archive.id,
                payload: spin_domain::RawJson(Some(
                    spin_domain::protocol::ArchiveResult {
                        r#ref: format!("snapshot:{}", snapshot.digest),
                        digest: "sha256:archive".into(),
                        size: 123,
                    }
                    .to_value()
                    .unwrap(),
                )),
                ..Default::default()
            }
            .to_json()
            .unwrap(),
        );
    }
    receive(&mut browser)
}

fn recording_and_external_session(
    server: &Server,
    runner: &mut TcpStream,
    frames: &mut spin_core::websocket::Decoder,
    headers: &[(&str, &str)],
    bearer: &str,
    client: &str,
) {
    use spin_domain::{self as d, Wire, protocol as p};
    let started = drive_capsule(
        server,
        runner,
        frames,
        headers,
        "/api/recordings",
        r#"{"actor":"spoofed","kind":"tool","name":"agent","enables":[{"name":"acp"},{"name":"git"}]}"#,
        "capsule.start_recording",
        r#"{"driver":"docker","container_id":"test-container","status":"ready"}"#,
    );
    assert_eq!(started.status, 201, "{}", started.body);
    let recording = d::Recording::from_json(started.body.as_bytes()).unwrap();
    assert_eq!(recording.actor, "derek");
    assert_eq!(recording.runtime.as_ref().unwrap().client_id, client);
    let executed = drive_capsule(
        server,
        runner,
        frames,
        headers,
        &format!("/api/recordings/{}/commands", recording.id),
        r#"{"input":"echo test"}"#,
        "capsule.execute",
        r#"{"Output":"test\n","ExitCode":0}"#,
    );
    assert_eq!(executed.status, 200);
    assert_eq!(
        d::Recording::from_json(executed.body.as_bytes())
            .unwrap()
            .commands
            .len(),
        1
    );
    terminal_over_browser_socket(server, runner, frames, headers, &recording.id);
    let sealed = drive_capsule(
        server,
        runner,
        frames,
        headers,
        &format!("/api/recordings/{}/end", recording.id),
        "{}",
        "capsule.seal",
        r#"{"driver":"docker","ref":"test-image","digest":"sha256:test","restorable":true}"#,
    );
    assert_eq!(sealed.status, 201, "{}", sealed.body);
    let artifact = d::Artifact::from_json(sealed.body.as_bytes()).unwrap();
    assert_eq!(artifact.snapshot.client_id, client);
    let repository = call(
        server,
        "POST",
        "/api/git/repositories",
        headers,
        r#"{"name":"session-test","remote_url":"https://example.test/spin.git","credential_scope":"public"}"#,
    );
    assert_eq!(repository.status, 201);
    let repository = d::CreateGitRepositoryResponse::from_json(repository.body.as_bytes())
        .unwrap()
        .repository;
    let mut browsing = connect(server);
    send(
        &mut browsing,
        "GET",
        &format!(
            "/api/git/repositories/{}/code/file?ref=feature%2Ftest%23one&path=docs%2Fhello+world.txt",
            repository.id
        ),
        headers,
        "",
    );
    let browse = runner_receive(runner, frames);
    assert_eq!(browse.method, p::METHOD_BROWSE_REPOSITORY);
    let request =
        p::RepositoryBrowsePayload::from_value(browse.payload.0.as_ref().unwrap()).unwrap();
    assert_eq!(request.browse.r#ref, "feature/test#one");
    assert_eq!(request.browse.path, "docs/hello world.txt");
    runner_send(runner, &p::WireMessage { r#type: p::MESSAGE_RESPONSE.into(), id: browse.id, payload: d::RawJson(Some(d::json::Value::from_json(br#"{"file":{"ref":"feature/test#one","path":"docs/hello world.txt","size":6,"content":"hello\n"}}"#).unwrap())), ..Default::default() }.to_json().unwrap());
    let browse = receive(&mut browsing);
    assert_eq!(browse.status, 200, "{}", browse.body);
    assert_eq!(
        d::engine::WorkspaceFile::from_json(browse.body.as_bytes())
            .unwrap()
            .content,
        "hello\n"
    );
    let created = call(
        server,
        "POST",
        "/api/jobs",
        headers,
        &format!(
            r#"{{"title":"A change","objective":"Do it","git_repository_id":"{}","environment_selector":"tool:agent"}}"#,
            repository.id
        ),
    );
    assert_eq!(created.status, 202, "{}", created.body);
    let created = d::CreateJobResponse::from_json(created.body.as_bytes()).unwrap();
    assert!(created.run_error.is_empty(), "{}", created.run_error);
    for (method, payload) in [
        ("capsule.accepts", r#"{"accepts":true}"#),
        (
            "capsule.materialize",
            r#"{"driver":"docker","container_id":"job-container","status":"ready"}"#,
        ),
    ] {
        let request = runner_receive(runner, frames);
        assert_eq!(request.method, method);
        runner_send(
            runner,
            &d::protocol::WireMessage {
                r#type: "response".into(),
                id: request.id,
                payload: d::RawJson(Some(d::json::parse(payload.as_bytes()).unwrap())),
                ..Default::default()
            }
            .to_json()
            .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let state = call(server, "GET", "/api/state", headers, "");
        let snapshot = d::Snapshot::from_json(state.body.as_bytes()).unwrap();
        if snapshot
            .compositions
            .iter()
            .any(|c| c.session_id == created.session.id && c.runtime.is_some())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "materialization never became durable"
        );
    }

    let changes = drive_capsule(
        server,
        runner,
        frames,
        headers,
        &format!("/api/sessions/{}/changes", created.session.id),
        "",
        "workspace.inspect",
        r#"{"branch":"session","added":1,"files":[{"path":"hello.txt","status":"??","added":1,"patch":"+hello\n"}]}"#,
    );
    assert_eq!(changes.status, 200, "{}", changes.body);
    let changes = d::engine::WorkspaceChanges::from_json(changes.body.as_bytes()).unwrap();
    assert_eq!(changes.files.as_slice()[0].repository, repository.id);
    chat_over_browser_socket(server, runner, frames, headers, &created.session.id);
    let worker = [("Authorization", bearer)];
    let claimed = call(
        server,
        "POST",
        "/api/sessions/claim",
        &worker,
        &format!(r#"{{"client_id":"{client}","tools":["*"]}}"#),
    );
    assert_eq!(claimed.status, 200, "{}", claimed.body);
    let assignment = d::Assignment::from_json(claimed.body.as_bytes()).unwrap();
    assert_eq!(assignment.session.id, created.session.id);
    let activation = &assignment.activation;
    let active_body = format!(
        r#"{{"activation_id":"{}","epoch":{}}}"#,
        activation.id, activation.epoch
    );
    let stale = call(
        server,
        "POST",
        &format!("/api/sessions/{}/start", created.session.id),
        &worker,
        &format!(r#"{{"activation_id":"{}","epoch":0}}"#, activation.id),
    );
    assert_eq!(stale.status, 409);
    assert_eq!(
        call(
            server,
            "POST",
            &format!("/api/sessions/{}/start", created.session.id),
            &worker,
            &active_body
        )
        .status,
        200
    );
    assert_eq!(
        call(
            server,
            "POST",
            &format!("/api/activations/{}/heartbeat", activation.id),
            &worker,
            &active_body
        )
        .status,
        200
    );
    let turn = call(
        server,
        "POST",
        &format!("/api/sessions/{}/turns", created.session.id),
        &worker,
        &format!(
            r#"{{"activation_id":"{}","epoch":{},"input":"Implement","actor":"derek"}}"#,
            activation.id, activation.epoch
        ),
    );
    assert_eq!(turn.status, 201);
    let turn = d::Turn::from_json(turn.body.as_bytes()).unwrap();
    let checkpoint = call(
        server,
        "POST",
        &format!("/api/sessions/{}/checkpoints", created.session.id),
        &worker,
        &format!(
            r#"{{"activation_id":"{}","epoch":{},"turn_id":"{}","kind":"result"}}"#,
            activation.id, activation.epoch, turn.id
        ),
    );
    assert_eq!(checkpoint.status, 201);
    let checkpoint = d::Checkpoint::from_json(checkpoint.body.as_bytes()).unwrap();
    let result = call(
        server,
        "POST",
        &format!("/api/sessions/{}/result", created.session.id),
        &worker,
        &format!(
            r#"{{"activation_id":"{}","epoch":{},"checkpoint_id":"{}","status":"success","summary":"Done"}}"#,
            activation.id, activation.epoch, checkpoint.id
        ),
    );
    assert_eq!(result.status, 201, "{}", result.body);
    assert_eq!(
        call(
            server,
            "POST",
            &format!("/api/activations/{}/heartbeat", activation.id),
            &worker,
            &active_body
        )
        .status,
        409
    );
    let state = call(server, "GET", "/api/state", headers, "");
    let state = d::Snapshot::from_json(state.body.as_bytes()).unwrap();
    assert_eq!(
        state
            .jobs
            .iter()
            .find(|j| j.id == created.job.id)
            .unwrap()
            .status,
        d::JOB_REVIEW
    );
}

fn chat_over_browser_socket(
    server: &Server,
    runner: &mut TcpStream,
    frames: &mut spin_core::websocket::Decoder,
    headers: &[(&str, &str)],
    session: &str,
) {
    use spin_domain::{self as d, Wire, protocol as p};
    let open = || {
        let mut browser = connect(server);
        let mut upgrade = headers.to_vec();
        upgrade.extend_from_slice(&[
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ]);
        send(
            &mut browser,
            "GET",
            &format!("/api/sessions/{session}/acp"),
            &upgrade,
            "",
        );
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            browser.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
            assert!(header.len() < 8192);
        }
        assert!(
            String::from_utf8(header)
                .unwrap()
                .starts_with("HTTP/1.1 101")
        );
        (
            browser,
            spin_core::websocket::Decoder::new(spin_core::websocket::Role::Client, 8 << 20),
        )
    };
    let read =
        |browser: &mut TcpStream, frames: &mut spin_core::websocket::Decoder, wanted: &str| {
            for _ in 0..20 {
                let spin_core::websocket::Event::Text(text) = next_frame(browser, frames) else {
                    panic!("expected chat JSON");
                };
                let event = d::json::Value::from_json(text.as_bytes()).unwrap();
                if event.as_object().unwrap().get("type").unwrap().as_str() == Some(wanted) {
                    return event;
                }
            }
            panic!("chat event not received: {wanted}");
        };
    let (mut browser, mut browser_frames) = open();
    let start = runner_receive(runner, frames);
    assert_eq!(start.method, p::METHOD_START_ENABLED);
    runner_send(
        runner,
        &p::WireMessage {
            r#type: p::MESSAGE_RESPONSE.into(),
            id: start.id.clone(),
            ..Default::default()
        }
        .to_json()
        .unwrap(),
    );
    let reply = |runner: &mut TcpStream, id: &d::json::Value, result: &str| {
        let answer = format!("{{\"id\":{},\"result\":{result}}}\n", id.to_json().unwrap());
        runner_send(
            runner,
            &p::WireMessage {
                r#type: p::MESSAGE_STREAM_DATA.into(),
                id: start.id.clone(),
                data: d::Bytes(Some(answer.into_bytes())),
                ..Default::default()
            }
            .to_json()
            .unwrap(),
        );
    };
    for (method, result) in [
        (
            "initialize",
            r#"{"protocolVersion":1,"agentInfo":{"name":"Fixture"}}"#,
        ),
        ("session/new", r#"{"sessionId":"fixture-session"}"#),
    ] {
        let request = runner_receive(runner, frames);
        assert_eq!(request.r#type, p::MESSAGE_STREAM_INPUT);
        let request = d::json::Value::from_json(request.data.0.as_deref().unwrap()).unwrap();
        assert_eq!(
            request.as_object().unwrap().get("method").unwrap().as_str(),
            Some(method)
        );
        reply(
            runner,
            request.as_object().unwrap().get("id").unwrap(),
            result,
        );
    }
    let ready = read(&mut browser, &mut browser_frames, "ready");
    assert_eq!(
        ready
            .as_object()
            .unwrap()
            .get("agent_name")
            .unwrap()
            .as_str(),
        Some("Fixture")
    );
    runner_send(&mut browser, r#"{"type":"prompt","text":"Maak het af"}"#);
    let request = runner_receive(runner, frames);
    assert_eq!(request.r#type, p::MESSAGE_STREAM_INPUT);
    assert_eq!(request.id, start.id);
    let prompt = d::json::Value::from_json(request.data.0.as_deref().unwrap()).unwrap();
    assert_eq!(
        prompt.as_object().unwrap().get("method").unwrap().as_str(),
        Some("session/prompt")
    );
    read(&mut browser, &mut browser_frames, "user");
    drop(browser);
    let (mut browser, mut browser_frames) = open();
    let ready = read(&mut browser, &mut browser_frames, "ready");
    assert_eq!(
        ready.as_object().unwrap().get("busy"),
        Some(&d::json::Value::Bool(true))
    );
    runner_send(runner, &p::WireMessage { r#type: p::MESSAGE_STREAM_DATA.into(), id: start.id.clone(), data: d::Bytes(Some(b"{\"method\":\"session/update\",\"params\":{\"sessionId\":\"fixture-session\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"Klaar\"}}}}\n".to_vec())), ..Default::default() }.to_json().unwrap());
    reply(
        runner,
        prompt.as_object().unwrap().get("id").unwrap(),
        r#"{"stopReason":"end_turn"}"#,
    );
    assert!(
        read(&mut browser, &mut browser_frames, "update")
            .to_json()
            .unwrap()
            .contains("Klaar")
    );
    read(&mut browser, &mut browser_frames, "turn_end");
    for (method, payload) in [
        (
            p::METHOD_SYNC_WORKSPACE,
            r#"{"Head":"0123456789abcdef","Committed":true,"Pushed":true}"#,
        ),
        (
            p::METHOD_CAPSULE_CHANGES,
            r#"{"files":1,"bytes":4,"entries":[{"path":"root/agent-config","bytes":4}]}"#,
        ),
    ] {
        let request = runner_receive(runner, frames);
        assert_eq!(request.method, method);
        runner_send(
            runner,
            &p::WireMessage {
                r#type: p::MESSAGE_RESPONSE.into(),
                id: request.id,
                payload: d::RawJson(Some(d::json::Value::from_json(payload.as_bytes()).unwrap())),
                ..Default::default()
            }
            .to_json()
            .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = call(server, "GET", "/api/state", headers, "");
        let snapshot = d::Snapshot::from_json(snapshot.body.as_bytes()).unwrap();
        let current = snapshot.sessions.iter().find(|s| s.id == session).unwrap();
        let composition = snapshot
            .compositions
            .iter()
            .find(|c| c.id == current.prepared_composition_id)
            .unwrap();
        if current.synced_head == "0123456789abcdef"
            && composition
                .capsule_changes
                .as_ref()
                .is_some_and(|changes| changes.files == 1)
        {
            break;
        }
        assert!(Instant::now() < deadline, "turn result was not preserved");
    }
    runner_send(
        runner,
        &p::WireMessage {
            r#type: p::MESSAGE_STREAM_EXIT.into(),
            id: start.id,
            error: "fixture finished".into(),
            ..Default::default()
        }
        .to_json()
        .unwrap(),
    );
    assert!(
        read(&mut browser, &mut browser_frames, "error")
            .to_json()
            .unwrap()
            .contains("fixture finished")
    );
    assert!(matches!(
        next_frame(&mut browser, &mut browser_frames),
        spin_core::websocket::Event::Close(_)
    ));
    assert_eq!(
        runner_receive(runner, frames).r#type,
        p::MESSAGE_STREAM_CLOSE
    );
}

fn terminal_over_browser_socket(
    server: &Server,
    runner: &mut TcpStream,
    frames: &mut spin_core::websocket::Decoder,
    headers: &[(&str, &str)],
    recording: &str,
) {
    use spin_domain::{
        self as d, Wire,
        protocol::{self as p, WireMessage},
    };
    let path = format!("/api/recordings/{recording}/terminal");
    let mut browser = connect(server);
    let mut upgrade_headers = headers.to_vec();
    upgrade_headers.extend_from_slice(&[
        ("Upgrade", "websocket"),
        ("Connection", "Upgrade"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ]);
    send(&mut browser, "GET", &path, &upgrade_headers, "");
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        browser.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
        assert!(header.len() < 8192);
    }
    assert!(
        String::from_utf8(header)
            .unwrap()
            .starts_with("HTTP/1.1 101")
    );
    let mut browser_frames =
        spin_core::websocket::Decoder::new(spin_core::websocket::Role::Client, 1 << 20);
    let event = |browser: &mut TcpStream, frames: &mut spin_core::websocket::Decoder| {
        let spin_core::websocket::Event::Text(text) = next_frame(browser, frames) else {
            panic!("expected terminal JSON");
        };
        d::json::Value::from_json(text.as_bytes()).unwrap()
    };
    runner_send(
        &mut browser,
        r#"{"type":"start","command":"sh","rows":24,"cols":80}"#,
    );
    let request = runner_receive(runner, frames);
    assert_eq!(request.method, p::METHOD_START_INTERACTIVE);
    let payload = p::InteractivePayload::from_value(request.payload.0.as_ref().unwrap()).unwrap();
    assert_eq!(payload.recording.actor, "derek");
    assert_eq!(payload.rows, 24);
    let send_message = |runner: &mut TcpStream,
                        kind: &str,
                        data: &[u8],
                        execution: Option<d::engine::Execution>| {
        runner_send(
            runner,
            &WireMessage {
                r#type: kind.into(),
                id: request.id.clone(),
                data: d::Bytes(Some(data.to_vec())),
                execution,
                payload: if kind == p::MESSAGE_RESPONSE {
                    d::RawJson(Some(
                        p::StreamResponse {
                            stream_id: request.id.clone(),
                        }
                        .to_value()
                        .unwrap(),
                    ))
                } else {
                    d::RawJson(None)
                },
                ..Default::default()
            }
            .to_json()
            .unwrap(),
        )
    };
    send_message(runner, p::MESSAGE_RESPONSE, &[], None);
    assert_eq!(
        event(&mut browser, &mut browser_frames)
            .as_object()
            .unwrap()
            .get("type")
            .unwrap()
            .as_str(),
        Some("ready")
    );
    runner_send(&mut browser, r#"{"type":"input","data":"hello\n"}"#);
    let input = runner_receive(runner, frames);
    assert_eq!(input.r#type, p::MESSAGE_STREAM_INPUT);
    assert_eq!(input.id, request.id);
    assert_eq!(input.data.0.unwrap(), b"hello\n");
    runner_send(&mut browser, r#"{"type":"resize","rows":1000,"cols":99}"#);
    let resize = runner_receive(runner, frames);
    assert_eq!(resize.rows, 500);
    assert_eq!(resize.cols, 99);
    send_message(runner, p::MESSAGE_STREAM_DATA, b"terminal output\n", None);
    assert_eq!(
        event(&mut browser, &mut browser_frames)
            .as_object()
            .unwrap()
            .get("data")
            .unwrap()
            .as_str(),
        Some("terminal output\n")
    );
    send_message(
        runner,
        p::MESSAGE_STREAM_EXIT,
        &[],
        Some(d::engine::Execution {
            output: "terminal output".into(),
            exit_code: 7,
        }),
    );
    assert_eq!(
        event(&mut browser, &mut browser_frames)
            .as_object()
            .unwrap()
            .get("exit_code")
            .unwrap()
            .as_i64(),
        Some(7)
    );
    assert!(matches!(
        next_frame(&mut browser, &mut browser_frames),
        spin_core::websocket::Event::Close(_)
    ));
    let state = call(server, "GET", "/api/state", headers, "");
    let state = d::Snapshot::from_json(state.body.as_bytes()).unwrap();
    let recorded = state.recordings.iter().find(|r| r.id == recording).unwrap();
    assert_eq!(recorded.commands.len(), 2);
    assert_eq!(recorded.commands.last().unwrap().exit_code, Some(7));
}

#[test]
fn chunk_upload_reorders_retries_publishes_atomically_and_survives_restart() {
    use spin_domain::{Wire, json::Value};
    let root = std::env::temp_dir().join(Random::open().unwrap().next("spin-uploads").unwrap());
    let server = start(&root);
    let setup = call(
        &server,
        "POST",
        "/api/auth/setup",
        &[],
        r#"{"username":"Derek","password":"a-long-password"}"#,
    );
    assert_eq!(setup.status, 201);
    let cookie = setup.header("Set-Cookie").split(';').next().unwrap();
    let auth = Value::from_json(setup.body.as_bytes()).unwrap();
    let csrf = auth
        .as_object()
        .unwrap()
        .get("csrf_token")
        .unwrap()
        .as_str()
        .unwrap();
    let browser_headers = [("Cookie", cookie), ("X-Spin-CSRF", csrf)];
    let attachment = call(
        &server,
        "POST",
        "/api/uploads",
        &browser_headers,
        r#"{"kind":"attachment","name":"brief.pdf","size":1048581}"#,
    );
    assert_eq!(attachment.status, 201, "{}", attachment.body);
    let attachment = Value::from_json(attachment.body.as_bytes()).unwrap();
    let attachment_path = format!(
        "/api/uploads/{}",
        attachment
            .as_object()
            .unwrap()
            .get("id")
            .unwrap()
            .as_str()
            .unwrap()
    );
    assert_eq!(
        call(
            &server,
            "PUT",
            &attachment_path,
            &[("Cookie", cookie)],
            "bad"
        )
        .status,
        403
    );
    let pdf = format!("%PDF-{}", "a".repeat((1 << 20) - 5));
    for (offset, bytes) in [("0", pdf.as_str()), ("1048576", "tail!")] {
        let response = call(
            &server,
            "PUT",
            &attachment_path,
            &[
                ("Cookie", cookie),
                ("X-Spin-CSRF", csrf),
                ("X-Spin-Upload-Offset", offset),
            ],
            bytes,
        );
        assert_eq!(response.status, 200, "{}", response.body);
    }
    let done = call(
        &server,
        "POST",
        &format!("{attachment_path}/complete"),
        &browser_headers,
        "",
    );
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(
        call(
            &server,
            "POST",
            &format!("{attachment_path}/complete"),
            &browser_headers,
            ""
        )
        .body,
        done.body
    );
    let attachment = spin_domain::JobAttachment::from_json(done.body.as_bytes()).unwrap();
    assert_eq!(attachment.media_type, "application/pdf");
    let get = call(
        &server,
        "GET",
        &format!("/api/job-attachments/{}", attachment.id),
        &browser_headers,
        "",
    );
    assert_eq!(get.status, 200, "{}", get.body);
    assert_eq!(get.body, format!("{pdf}tail!"));
    assert!(
        get.header("Content-Security-Policy")
            .starts_with("sandbox;")
    );
    let token = call(
        &server,
        "GET",
        "/api/runners/token",
        &[("Cookie", cookie)],
        "",
    );
    let token = Value::from_json(token.body.as_bytes()).unwrap();
    let bearer = format!(
        "Bearer {}",
        token
            .as_object()
            .unwrap()
            .get("token")
            .unwrap()
            .as_str()
            .unwrap()
    );
    let headers = [("Authorization", bearer.as_str())];
    let create = |server: &Server| {
        let response = call(
            server,
            "POST",
            "/api/uploads",
            &headers,
            r#"{"kind":"snapshot","size":2097171,"snapshot":{"digest":"sha256:image","ref":"test-image"}}"#,
        );
        assert_eq!(response.status, 201, "{}", response.body);
        let body = Value::from_json(response.body.as_bytes()).unwrap();
        format!(
            "/api/uploads/{}",
            body.as_object()
                .unwrap()
                .get("id")
                .unwrap()
                .as_str()
                .unwrap()
        )
    };
    let path = create(&server);
    assert_eq!(call(&server, "GET", &path, &[], "").status, 401);
    let download = "/api/snapshots/sha256:image?offset=0";
    assert_eq!(call(&server, "GET", download, &headers, "").status, 404);
    let first = "A".repeat(1 << 20);
    let second = "B".repeat(1 << 20);
    let tail = "C".repeat(19);
    for (offset, data, expected) in [
        (1048576, second.as_str(), 0),
        (2097152, tail.as_str(), 0),
        (0, first.as_str(), 2097171),
    ] {
        let response = call(
            &server,
            "PUT",
            &path,
            &[
                ("Authorization", &bearer),
                ("X-Spin-Upload-Offset", &offset.to_string()),
            ],
            data,
        );
        assert_eq!(response.status, 200, "{}", response.body);
        let status = Value::from_json(response.body.as_bytes()).unwrap();
        assert_eq!(
            status.as_object().unwrap().get("offset").unwrap().as_i64(),
            Some(expected)
        );
        if expected == 0 {
            assert_eq!(
                call(&server, "POST", &format!("{path}/complete"), &headers, "").status,
                409
            );
        }
    }
    let retry = call(
        &server,
        "PUT",
        &path,
        &[("Authorization", &bearer), ("X-Spin-Upload-Offset", "0")],
        &"X".repeat(1 << 20),
    );
    assert_eq!(retry.status, 200);
    assert_eq!(call(&server, "GET", download, &headers, "").status, 404);
    let completed = call(&server, "POST", &format!("{path}/complete"), &headers, "");
    assert_eq!(completed.status, 200, "{}", completed.body);
    let digest = format!(
        "sha256:{}",
        spin_security::digest_hex(format!("{first}{second}{tail}").as_bytes()).unwrap()
    );
    assert!(completed.body.contains(&digest));
    let incomplete = create(&server);
    let response = call(
        &server,
        "PUT",
        &incomplete,
        &[("Authorization", &bearer), ("X-Spin-Upload-Offset", "0")],
        &first,
    );
    assert_eq!(response.status, 200);
    drop(server);
    let server = start(&root);
    assert_eq!(call(&server, "GET", &incomplete, &headers, "").status, 404);
    for (offset, expected) in [
        (0, first.as_str()),
        (1048576, second.as_str()),
        (2097152, tail.as_str()),
    ] {
        let response = call(
            &server,
            "GET",
            &format!("/api/snapshots/sha256:image?offset={offset}"),
            &headers,
            "",
        );
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(response.body, expected);
        assert_eq!(response.header("X-Spin-Digest"), digest);
        assert_eq!(response.header("X-Spin-Size"), "2097171");
    }
    assert_eq!(
        call(
            &server,
            "GET",
            "/api/snapshots/sha256:image?offset=2097171",
            &headers,
            ""
        )
        .status,
        416
    );
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}
