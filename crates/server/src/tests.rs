use super::*;
use alloc::vec;
use core::cell::Cell;
use d::{Wire, state::PersistedState, try_string};

#[test]
fn stale_workflow_attempts_cannot_block_another_jobs_lost_capsule() {
    use d::protocol as p;
    for stale in ["deleted-job", "old-attempt", "broken-current"] {
        for run_status in [d::PHASE_RUN_QUEUED, d::PHASE_RUN_RUNNING] {
            let mut state = PersistedState::from_json(br#"{
                "artifacts":{"env":{"id":"env","kind":"tool","name":"agent","scope":"global","profile":"default","enables":[{"name":"acp","command":"agent"},{"name":"git"}]}},
                "git_repositories":{"repo":{"id":"repo","remote_url":"https://example.test/repo.git","default_ref":"main","credential_scope":"public"}},
                "jobs":{
                    "broken":{"id":"broken","current_phase_run_id":"missing-run"},
                    "healthy":{"id":"healthy","owner":"derek","status":"running","git_repository_id":"repo","branch":"jobs/healthy/main","current_phase_run_id":"run","template_snapshot":{"id":"tpl","phases":[{"id":"build","executor":"agent"}]}}
                },
                "sessions":{
                    "stale":{"id":"stale","job_id":"broken","phase_run_id":"missing-run","operator":"derek","status":"queued","created_at":"2026-09-29T12:00:00Z"},
                    "healthy":{"id":"healthy","job_id":"healthy","phase_run_id":"run","operator":"derek","status":"queued","tool":"agent","git_repository_id":"repo","prepared_composition_id":"lost","created_at":"2026-09-28T12:00:00Z"}
                },
                "phase_runs":{"run":{"id":"run","job_id":"healthy","session_id":"healthy","phase_id":"build","status":"queued"}},
                "compositions":{"lost":{"id":"lost","session_id":"healthy","operator":"derek","runtime":{"client_id":"client","container_id":"gone","status":"stopped"}}}
            }"#).unwrap();
            match stale {
                "deleted-job" => {
                    state.jobs.remove("broken");
                }
                "old-attempt" => {
                    state.jobs.get_mut("broken").unwrap().current_phase_run_id = "new-run".into()
                }
                _ => {}
            }
            state.phase_runs.get_mut("run").unwrap().status = run_status.into();
            let fail = Cell::new(false);
            let mut server = Server::new(Store::new(state, Memory(&fail)));
            let now = time();
            let mut random = Random(70000);
            let mut peer = spin_core::runner::Peer::new(d::Client {
                id: "client".into(),
                capabilities: d::ClientCapabilities::from_json(br#"{"engine":{"available":true}}"#)
                    .unwrap(),
                ..Default::default()
            });
            let (generation, _) = peer
                .attach("runner", None, now.time().unwrap().0 / 1_000_000)
                .unwrap();
            server.runners.push(peer);
            let result = server.maintain_capsules(&now, &mut random);
            assert_eq!(
                result.is_err(),
                stale == "broken-current",
                "{stale}: {result:?}"
            );
            assert_eq!(
                server.calls.len(),
                1,
                "the healthy session must reach a runner despite {stale}"
            );
            let new_id = &server
                .store
                .session("healthy")
                .unwrap()
                .prepared_composition_id;
            assert_ne!(new_id, "lost");
            assert_eq!(
                server.store.composition(new_id).unwrap().session_id,
                "healthy"
            );
            let (_, message) = server.runners[0].next(generation).unwrap().unwrap();
            assert_eq!(message.method, p::METHOD_ACCEPTS);
            assert_eq!(
                server.store.session("stale").unwrap().status,
                d::SESSION_QUEUED
            );
            let later =
                Timestamp::from_time(d::Time(now.time().unwrap().0 + 31_000_000_000)).unwrap();
            let result = server.maintain_capsules(&later, &mut random);
            assert_eq!(result.is_err(), stale == "broken-current");
            assert_eq!(
                server.calls.len(),
                1,
                "repeated sweeps must not duplicate the start"
            );
        }
    }
}

#[test]
fn chat_shares_agents_persists_live_turns_and_reports_lost_streams_after_restart() {
    use core::cell::RefCell;
    use d::protocol as p;
    struct Saved<'a>(&'a RefCell<PersistedState>);
    impl Persistence for Saved<'_> {
        fn save(&mut self, state: &PersistedState) -> spin_store::Result {
            *self.0.borrow_mut() = state.try_clone()?;
            Ok(())
        }
    }
    let state = PersistedState::from_json(br#"{
        "users":{"user":{"id":"user","username":"derek"}},
        "auth_sessions":{"auth":{"id":"auth","user_id":"user","token_hash":"token","expires_at":"2100-01-01T00:00:00Z"}},
        "sessions":{"ses":{"id":"ses","job_id":"job","operator":"derek","prepared_composition_id":"cmp"}},
        "compositions":{"cmp":{"id":"cmp","operator":"derek","session_id":"ses","runtime":{"client_id":"client","container_id":"container","status":"ready"},"enabled":[{"name":"acp","command":"agent","transport":"stdio","protocol_version":1}]}}
    }"#).unwrap();
    let saved = RefCell::new(state.try_clone().unwrap());
    let mut server = Server::new(Store::new(state, Saved(&saved)));
    let mut random = Random(10);
    let now = time();
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    let (generation, _) = peer.attach("runner", None, 0).unwrap();
    server.runners.push(peer);
    let path = "/api/sessions/ses/acp";
    let open = |server: &mut Server<_>, random: &mut Random, actor: &str| {
        let Some(Outcome::Chat(link)) = server
            .chat_route(&req("GET", path, &[], b""), actor, "token", &now, random)
            .unwrap()
        else {
            panic!("chat expected")
        };
        link
    };
    let mut chat = open(&mut server, &mut random, "derek");
    let mut viewer = open(&mut server, &mut random, "someone-else");
    assert_eq!(server.agents.len(), 1);
    let (ticket, request) = server.runners[0].next(generation).unwrap().unwrap();
    let stream = request.id.try_clone().unwrap();
    assert_eq!(request.method, p::METHOD_START_ENABLED);
    server.runners[0].acknowledge(ticket);
    server.runners[0].response(&stream).unwrap();
    server
        .agent_runner(
            "client",
            &p::WireMessage {
                r#type: p::MESSAGE_RESPONSE.into(),
                id: stream.try_clone().unwrap(),
                ..Default::default()
            },
            &now,
        )
        .unwrap();
    fn rpc<P: Persistence>(
        server: &mut Server<P>,
        generation: u64,
        now: &Timestamp,
        random: &mut Random,
        expected: &str,
        result: &str,
    ) -> u64 {
        server.maintain_agents(now, random).unwrap();
        let (ticket, message) = server.runners[0].next(generation).unwrap().unwrap();
        assert_eq!(message.r#type, p::MESSAGE_STREAM_INPUT);
        let stream = message.id.try_clone().unwrap();
        let json = Value::from_json(message.data.0.as_ref().unwrap()).unwrap();
        assert_eq!(
            json.as_object().unwrap().get("method").unwrap().as_str(),
            Some(expected)
        );
        let id =
            u64::try_from(i64::from_value(json.as_object().unwrap().get("id").unwrap()).unwrap())
                .unwrap();
        server.runners[0].acknowledge(ticket);
        if !result.is_empty() {
            let answer =
                spin_core::validation::text(format_args!("{{\"id\":{id},\"result\":{result}}}\n"))
                    .unwrap();
            server
                .agent_runner(
                    "client",
                    &p::WireMessage {
                        r#type: p::MESSAGE_STREAM_DATA.into(),
                        id: stream,
                        data: d::Bytes(Some(answer.into_bytes())),
                        ..Default::default()
                    },
                    now,
                )
                .unwrap();
        }
        id
    }
    rpc(
        &mut server,
        generation,
        &now,
        &mut random,
        "initialize",
        r#"{"protocolVersion":1,"agentInfo":{"name":"test-agent"}}"#,
    );
    rpc(
        &mut server,
        generation,
        &now,
        &mut random,
        "session/new",
        r#"{"sessionId":"agent-1"}"#,
    );
    server.maintain_agents(&now, &mut random).unwrap();
    let (sequence, event) = server.chat_next(&chat).unwrap().unwrap();
    assert!(event.contains("\"type\":\"ready\""));
    server.chat_acknowledge(&mut chat, sequence);
    assert!(
        server
            .chat_next(&viewer)
            .unwrap()
            .unwrap()
            .1
            .contains("\"viewer\":true")
    );
    let prompt = Value::from_json(br#"{"type":"prompt","text":"work"}"#).unwrap();
    assert!(server.chat_message(&mut viewer, &prompt, &now).is_err());
    server.chat_message(&mut chat, &prompt, &now).unwrap();
    let prompt_id = rpc(
        &mut server,
        generation,
        &now,
        &mut random,
        "session/prompt",
        "",
    );
    server.maintain_agents(&now, &mut random).unwrap();
    assert_eq!(
        server
            .store
            .composition("cmp")
            .unwrap()
            .agent
            .as_ref()
            .unwrap()
            .prompt_id,
        spin_core::validation::text(format_args!("{prompt_id}")).unwrap()
    );
    drop(chat);
    let mut reconnected = open(&mut server, &mut random, "derek");
    assert!(
        server
            .chat_next(&reconnected)
            .unwrap()
            .unwrap()
            .1
            .contains("\"busy\":true")
    );
    assert!(server.runners[0].next(generation).unwrap().is_none());
    drop(server);
    let recovered = saved.borrow().try_clone().unwrap();
    let mut server = Server::new(Store::new(recovered, Saved(&saved)));
    server.recover(&now).unwrap();
    assert_eq!(server.agents.len(), 1);
    // Buffered completion is consumed even before a browser reconnects.
    let answer = spin_core::validation::text(format_args!(
        "{{\"id\":{prompt_id},\"result\":{{\"stopReason\":\"end_turn\"}}}}\n"
    ))
    .unwrap();
    server
        .agent_runner(
            "client",
            &p::WireMessage {
                r#type: p::MESSAGE_STREAM_DATA.into(),
                id: stream.try_clone().unwrap(),
                data: d::Bytes(Some(answer.into_bytes())),
                ..Default::default()
            },
            &now,
        )
        .unwrap();
    server.maintain_agents(&now, &mut random).unwrap();
    assert!(
        server
            .store
            .composition("cmp")
            .unwrap()
            .agent
            .as_ref()
            .unwrap()
            .prompt_id
            .is_empty()
    );
    while let Some((sequence, _)) = server.chat_next(&reconnected).unwrap() {
        server.chat_acknowledge(&mut reconnected, sequence);
    }
    server
        .agent_runner(
            "client",
            &p::WireMessage {
                r#type: p::MESSAGE_STREAM_EXIT.into(),
                id: stream,
                error: "agent vanished".into(),
                ..Default::default()
            },
            &now,
        )
        .unwrap();
    server.maintain_agents(&now, &mut random).unwrap();
    let (sequence, event) = server.chat_next(&reconnected).unwrap().unwrap();
    assert!(event.contains("agent vanished"));
    assert!(!server.chat_done(&reconnected));
    server.chat_acknowledge(&mut reconnected, sequence);
    assert!(server.chat_done(&reconnected));
    assert!(server.store.composition("cmp").unwrap().agent.is_none());
}
struct Memory<'a>(&'a Cell<bool>);
impl Persistence for Memory<'_> {
    fn save(&mut self, _: &PersistedState) -> spin_store::Result {
        if self.0.get() {
            Err(spin_store::Error::Storage(10))
        } else {
            Ok(())
        }
    }
}
struct Random(u64);
#[test]
fn workflow_mcp_capability_is_session_bound_and_accept_waits_for_git() {
    use d::protocol as p;
    let mut state = PersistedState::from_json(br#"{
      "jobs":{"job":{"id":"job","owner":"derek","status":"running","workflow_status":"running","current_phase_run_id":"run","branch":"jobs/#1/main","template_snapshot":{"id":"tpl","phases":[{"id":"develop","name":"Bouw","allow_changes":true,"accept":{"target":"DONE"},"reject":{"target":"SELF"}}]}}},
      "sessions":{"ses":{"id":"ses","job_id":"job","phase_run_id":"run","operator":"derek","status":"running","prepared_composition_id":"cmp","git_ref":"jobs/#1/sessions/one"}},
      "phase_runs":{"run":{"id":"run","session_id":"ses","job_id":"job","phase_id":"develop","phase_name":"Bouw","status":"running","attempt":1}},
      "compositions":{"cmp":{"id":"cmp","operator":"derek","session_id":"ses","runtime":{"driver":"docker","client_id":"client","container_id":"container","status":"ready"},"git":{"repository_id":"repo","remote_url":"https://example.test/repo.git","credential_scope":"public","bootstrap_ref":"main","mode":"change"}}}
    }"#).unwrap();
    state
        .workflow_tokens
        .insert(
            "ses".into(),
            spin_security::digest_hex(b"workflow-secret").unwrap(),
        )
        .unwrap();
    let fail = Cell::new(false);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let mut random = Random(100);
    let now = time();
    let path = "/api/workflow/mcp/ses";
    let headers = [("Authorization", "Bearer workflow-secret")];
    assert_eq!(
        response(
            &mut server,
            req("POST", path, &[], b"{}"),
            &now,
            &mut random
        )
        .status,
        401
    );
    assert_eq!(
        response(
            &mut server,
            req("POST", "/api/workflow/mcp/other", &headers, b"{}"),
            &now,
            &mut random
        )
        .status,
        401
    );
    assert_eq!(
        response(
            &mut server,
            req(
                "POST",
                path,
                &[
                    ("Authorization", "Bearer workflow-secret"),
                    ("Origin", "http://spin.test")
                ],
                b"{}"
            ),
            &now,
            &mut random
        )
        .status,
        403
    );
    assert_eq!(
        response(
            &mut server,
            req("GET", path, &headers, b""),
            &now,
            &mut random
        )
        .status,
        405
    );
    assert_eq!(
        response(
            &mut server,
            req(
                "POST",
                path,
                &headers,
                br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            ),
            &now,
            &mut random
        )
        .status,
        202
    );
    let initialize = response(&mut server, req("POST", path, &headers, br#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#), &now, &mut random);
    assert!(
        core::str::from_utf8(&initialize.body)
            .unwrap()
            .contains("2025-03-26")
    );
    let tools = response(
        &mut server,
        req(
            "POST",
            path,
            &headers,
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        ),
        &now,
        &mut random,
    );
    assert!(
        core::str::from_utf8(&tools.body)
            .unwrap()
            .contains("\"accept\"")
    );
    let ask = response(&mut server, req("POST", path, &headers, br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ask","arguments":{"questions":[{"question":"Welke kleur?","options":["Blauw","Groen"]}]}}}"#), &now, &mut random);
    assert!(
        core::str::from_utf8(&ask.body)
            .unwrap()
            .contains("\"isError\":false")
    );
    assert_eq!(
        server.store.workflow_for_session("ses").unwrap().run.status,
        d::PHASE_RUN_PENDING
    );
    server
        .store
        .resume_workflow_phase_for_chat("ses", "derek", &now)
        .unwrap();
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    let (generation, _) = peer.attach("runner", None, 0).unwrap();
    server.runners.push(peer);
    let accept = br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"accept","arguments":{"summary":"Klaar"}}}"#;
    for failure in ["push refused", ""] {
        let outcome = server
            .begin(req("POST", path, &headers, accept), &now, &mut random)
            .unwrap();
        let Outcome::Capsule(wait) = outcome else {
            if let Outcome::Response(r) = outcome {
                panic!(
                    "accept must await Git: {}",
                    core::str::from_utf8(&r.body).unwrap()
                );
            }
            panic!("expected capsule");
        };
        assert_eq!(
            server.store.workflow_for_session("ses").unwrap().run.status,
            d::PHASE_RUN_RUNNING
        );
        let (ticket, message) = server.runners[0].next(generation).unwrap().unwrap();
        assert_eq!(message.method, p::METHOD_ACCEPT_WORKSPACE);
        let request = message.try_clone().unwrap();
        let payload =
            p::AcceptWorkspacePayload::from_value(request.payload.0.as_ref().unwrap()).unwrap();
        assert_eq!(payload.acceptance.remote_ref, "jobs/#1/main");
        assert!(payload.acceptance.allow_changes);
        assert!(payload.acceptance.authentication.is_none());
        server.runners[0].acknowledge(ticket);
        server.runners[0].response(&request.id);
        let reply = p::WireMessage {
            id: request.id.try_clone().unwrap(),
            r#type: p::MESSAGE_RESPONSE.into(),
            error: failure.into(),
            payload: d::RawJson(Some(
                Value::from_json(br#"{"Head":"0123456789abcdef","Committed":true}"#).unwrap(),
            )),
            ..Default::default()
        };
        server
            .finish_capsule("client", Some(&request), &reply, &now, &mut random)
            .unwrap();
        let response = server.poll_capsule(&wait, &now).unwrap().unwrap();
        let body = core::str::from_utf8(&response.body).unwrap();
        assert_eq!(response.status, 200, "{body}");
        assert!(
            body.contains(if failure.is_empty() {
                "\"isError\":false"
            } else {
                "\"isError\":true"
            }),
            "{body}"
        );
        assert_eq!(
            server.store.workflow_for_session("ses").unwrap().run.status,
            if failure.is_empty() {
                d::PHASE_RUN_ACCEPTED
            } else {
                d::PHASE_RUN_RUNNING
            }
        );
    }
    assert_eq!(
        server.store.job("job").unwrap().workflow_status,
        d::WORKFLOW_DONE
    );
    let before = server.runners[0].pending_count();
    let repeat = response(
        &mut server,
        req("POST", path, &headers, accept),
        &now,
        &mut random,
    );
    assert!(
        core::str::from_utf8(&repeat.body)
            .unwrap()
            .contains("\"isError\":true")
    );
    assert_eq!(before, server.runners[0].pending_count());
}
impl Entropy for Random {
    fn fill(&mut self, bytes: &mut [u8]) -> spin_security::Result {
        for byte in bytes {
            self.0 += 1;
            *byte = self.0.to_le_bytes()[0];
        }
        Ok(())
    }
}
impl IdSource for Random {
    fn next(&mut self, prefix: &str) -> spin_store::Result<String> {
        self.0 += 1;
        Ok(spin_core::validation::text(format_args!(
            "{prefix}_{}",
            self.0
        ))?)
    }
}
fn req<'a>(
    method: &'a str,
    path: &'a str,
    headers: &'a [(&'a str, &'a str)],
    body: &'a [u8],
) -> Request<'a> {
    Request {
        method,
        path: path.split_once('?').map_or(path, |p| p.0),
        raw_query: path.split_once('?').map_or("", |p| p.1),
        headers,
        body,
        peer: "127.0.0.1",
        secure: false,
    }
}

#[test]
fn terminals_are_owned_bounded_and_disconnect_cancels_only_its_request() {
    use d::protocol as p;
    let state = PersistedState::from_json(br#"{"recordings":{"rec":{"id":"rec","actor":"derek","status":"recording","runtime":{"driver":"docker","client_id":"client","container_id":"container","status":"recording"}}}}"#).unwrap();
    let fail = Cell::new(false);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let mut random = Random(1);
    let path = "/api/recordings/rec/terminal";
    assert!(
        server
            .terminal_route(
                &req("GET", path, &[], b""),
                "someone-else",
                "token",
                &mut random
            )
            .is_err()
    );
    assert!(
        server
            .terminal_route(
                &req(
                    "GET",
                    path,
                    &[("Origin", "https://evil.test"), ("Host", "spin.test")],
                    b""
                ),
                "derek",
                "token",
                &mut random
            )
            .is_err()
    );
    let mut links = alloc::vec::Vec::new();
    for _ in 0..8 {
        let Some(Outcome::Terminal(link)) = server
            .terminal_route(&req("GET", path, &[], b""), "derek", "token", &mut random)
            .unwrap()
        else {
            panic!("terminal expected");
        };
        links.push(link);
    }
    assert!(
        server
            .terminal_route(&req("GET", path, &[], b""), "derek", "token", &mut random)
            .is_err()
    );
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    let (generation, _) = peer.attach("process", None, 0).unwrap();
    server.runners.push(peer);
    let first = links.remove(0);
    server
        .terminal_message(
            &first,
            &Value::from_json(br#"{"type":"start","command":"sh"}"#).unwrap(),
            &mut random,
        )
        .unwrap();
    let (ticket, call) = server.runners[0].next(generation).unwrap().unwrap();
    let id = call.id.try_clone().unwrap();
    assert_eq!(call.method, p::METHOD_START_INTERACTIVE);
    server.runners[0].acknowledge(ticket);
    server.terminal_disconnect(first).unwrap();
    let (_, cancelled) = server.runners[0].next(generation).unwrap().unwrap();
    assert_eq!(cancelled.id, id);
    assert_eq!(cancelled.r#type, p::MESSAGE_CANCEL);
    assert_eq!(server.terminals.len(), 7);
    assert!(links.iter().all(|link| !server.terminal_done(link)));
    assert_eq!(server.runners[0].pending_count(), 0);
}
fn response<P: Persistence>(
    server: &mut Server<P>,
    request: Request<'_>,
    now: &Timestamp,
    random: &mut Random,
) -> Response {
    match server.begin(request, now, random) {
        Ok(Outcome::Response(response)) => response,
        Ok(Outcome::Password(mut work)) => {
            while !work.step(2048) {}
            match server.finish_password(work, now, random) {
                Ok(r) => r,
                Err(e) => e.response().unwrap(),
            }
        }
        Ok(Outcome::State(_)) => panic!("unexpected state stream"),
        Ok(Outcome::Runner(_)) => panic!("unexpected runner stream"),
        Ok(Outcome::Capsule(_)) => panic!("unexpected capsule task"),
        Ok(Outcome::Terminal(_)) => panic!("unexpected terminal"),
        Ok(Outcome::Chat(_)) => panic!("unexpected chat"),
        Ok(Outcome::Upload(_)) => panic!("unexpected upload"),
        Ok(Outcome::Network(_)) => panic!("unexpected provider request"),
        Ok(Outcome::Operation(_)) => panic!("unexpected management operation"),
        Ok(Outcome::Download(_)) => panic!("unexpected backup stream"),
        Err(e) => e.response().unwrap(),
    }
}
fn cookie(response: &Response) -> &str {
    response
        .headers
        .iter()
        .find(|(k, _)| k == "Set-Cookie")
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
}
fn csrf(response: &Response) -> String {
    let value = d::json::parse(&response.body).unwrap();
    try_string(
        value
            .as_object()
            .unwrap()
            .get("csrf_token")
            .unwrap()
            .as_str()
            .unwrap(),
    )
    .unwrap()
}
fn time() -> Timestamp {
    Timestamp::from_json(br#""2026-09-30T12:00:00Z""#).unwrap()
}
#[test]
fn browser_can_close_a_job_with_an_empty_post_body() {
    let fail = Cell::new(false);
    let now = time();
    let mut random = Random(91000);
    let state = PersistedState::from_json(br#"{
        "jobs":{"job":{"id":"job","owner":"derek","status":"active","session_ids":["ses"],"current_phase_run_id":"missing-run"}},
        "sessions":{"ses":{"id":"ses","job_id":"job","operator":"derek","phase_run_id":"missing-run","status":"queued"}}
    }"#).unwrap();
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let setup = response(
        &mut server,
        req(
            "POST",
            "/api/auth/setup",
            &[],
            br#"{"username":"Derek","display_name":"Derek","password":"a-long-password"}"#,
        ),
        &now,
        &mut random,
    );
    assert_eq!(setup.status, 201);
    let csrf = csrf(&setup);
    let headers = [
        ("Cookie", cookie(&setup)),
        ("X-Spin-CSRF", csrf.as_str()),
        ("Host", "bollenloods.getspin.app"),
        ("Origin", "https://bollenloods.getspin.app"),
    ];
    for _ in 0..2 {
        let Outcome::Operation(wait) = server
            .begin(
                req("POST", "/api/jobs/job/close", &headers, b""),
                &now,
                &mut random,
            )
            .unwrap()
        else {
            panic!("expected close operation")
        };
        server.maintain_operations(&now, &mut random).unwrap();
        let result = server.poll_operation(&wait).unwrap().unwrap();
        assert_eq!(
            result.status,
            200,
            "{}",
            core::str::from_utf8(&result.body).unwrap()
        );
        assert_eq!(server.store.job("job").unwrap().status, d::JOB_CANCELLED);
    }
}
#[test]
fn setup_cookie_csrf_restart_and_logout_match_browser_contract() {
    let fail = Cell::new(false);
    let now = time();
    let mut random = Random(0);
    let mut server = Server::new(Store::new(PersistedState::default(), Memory(&fail)));
    let status = response(
        &mut server,
        req("GET", "/api/auth/status", &[], &[]),
        &now,
        &mut random,
    );
    assert_eq!(status.status, 200);
    assert!(
        core::str::from_utf8(&status.body)
            .unwrap()
            .contains("\"configured\":false")
    );
    let setup = br#"{"username":"Derek","display_name":"Derek","password":"a-long-password"}"#;
    let invalid = response(
        &mut server,
        req(
            "POST",
            "/api/auth/setup",
            &[("Host", "spin.test"), ("Origin", "https://evil.test")],
            setup,
        ),
        &now,
        &mut random,
    );
    assert_eq!(invalid.status, 403);
    let setup = response(
        &mut server,
        req("POST", "/api/auth/setup", &[], setup),
        &now,
        &mut random,
    );
    assert_eq!(
        setup.status,
        201,
        "{}",
        core::str::from_utf8(&setup.body).unwrap()
    );
    let hash = &server
        .store
        .user_by_username("derek")
        .unwrap()
        .password_hash;
    assert!(spin_security::verify_password("a-long-password", hash).unwrap());
    let auth_cookie = cookie(&setup);
    let csrf = csrf(&setup);
    let state = response(
        &mut server,
        req("GET", "/api/state", &[("Cookie", auth_cookie)], &[]),
        &now,
        &mut random,
    );
    assert_eq!(state.status, 200);
    let encoded = core::str::from_utf8(&state.body).unwrap();
    assert!(!encoded.contains("password_hash"));
    assert!(encoded.contains("\"recommendations\":[]"));
    for key in [
        "Cache-Control",
        "CDN-Cache-Control",
        "Cloudflare-CDN-Cache-Control",
    ] {
        assert!(
            state
                .headers
                .iter()
                .any(|(k, v)| k == key && v.contains("no-store"))
        );
    }
    let no_csrf = response(
        &mut server,
        req("POST", "/api/auth/logout", &[("Cookie", auth_cookie)], &[]),
        &now,
        &mut random,
    );
    assert_eq!(no_csrf.status, 403);
    let evil = response(
        &mut server,
        req(
            "POST",
            "/api/auth/logout",
            &[
                ("Cookie", auth_cookie),
                ("X-Spin-CSRF", &csrf),
                ("Host", "spin.test"),
                ("Origin", "https://evil.test"),
            ],
            &[],
        ),
        &now,
        &mut random,
    );
    assert_eq!(evil.status, 403);
    let mut server = Server::new(server.store);
    let refreshed = response(
        &mut server,
        req("GET", "/api/auth/status", &[("Cookie", auth_cookie)], &[]),
        &now,
        &mut random,
    );
    assert_eq!(refreshed.status, 200);
    let fresh_cookie = cookie(&refreshed);
    assert_ne!(auth_cookie, fresh_cookie);
    assert_eq!(
        response(
            &mut server,
            req("GET", "/api/state", &[("Cookie", auth_cookie)], &[]),
            &now,
            &mut random
        )
        .status,
        401
    );
    let fresh_csrf = super::tests::csrf(&refreshed);
    let logout = response(
        &mut server,
        req(
            "POST",
            "/api/auth/logout",
            &[("Cookie", fresh_cookie), ("X-Spin-CSRF", &fresh_csrf)],
            &[],
        ),
        &now,
        &mut random,
    );
    assert_eq!(logout.status, 204);
    assert_eq!(
        response(
            &mut server,
            req("GET", "/api/state", &[("Cookie", fresh_cookie)], &[]),
            &now,
            &mut random
        )
        .status,
        401
    );
}
#[test]
fn login_is_bounded_rate_limited_and_password_changes_fence_pending_work() {
    let fail = Cell::new(false);
    let now = time();
    let mut random = Random(0);
    let key = spin_security::derive_password(b"a-long-password", &[0; 16], 20).unwrap();
    let hash = spin_core::validation::text(format_args!(
        "pbkdf2-sha256$20${}${}",
        spin_security::encode_base64(&[0; 16], false).unwrap(),
        spin_security::encode_base64(&key, false).unwrap()
    ))
    .unwrap();
    let mut store = Store::new(PersistedState::default(), Memory(&fail));
    store
        .create_initial_user(
            d::User {
                username: try_string("derek").unwrap(),
                password_hash: hash,
                ..Default::default()
            },
            spin_store::Context {
                now: &now,
                id: "usr_1",
            },
        )
        .unwrap();
    let mut server = Server::new(store);
    let login = br#"{"username":"derek","password":"a-long-password"}"#;
    let Ok(Outcome::Password(mut pending)) = server.begin(
        req("POST", "/api/auth/login", &[], login),
        &now,
        &mut random,
    ) else {
        panic!("expected password work");
    };
    assert!(!pending.step(1));
    server
        .store
        .reset_user_password("usr_1", "usr_1", "changed-after-login-started")
        .unwrap();
    pending.step(100);
    assert_eq!(
        server
            .finish_password(pending, &now, &mut random)
            .err()
            .unwrap()
            .response()
            .unwrap()
            .status,
        401
    );
    // Een geldige korte fixturehash maakt alleen deze rate-limittest goedkoop.
    let hash = spin_core::validation::text(format_args!(
        "pbkdf2-sha256$20${}${}",
        spin_security::encode_base64(&[0; 16], false).unwrap(),
        spin_security::encode_base64(&key, false).unwrap()
    ))
    .unwrap();
    server
        .store
        .reset_user_password("usr_1", "usr_1", &hash)
        .unwrap();
    for _ in 0..4 {
        let bad = response(
            &mut server,
            req(
                "POST",
                "/api/auth/login",
                &[],
                br#"{"username":"derek","password":"wrong"}"#,
            ),
            &now,
            &mut random,
        );
        assert_eq!(bad.status, 401);
    }
    let blocked = response(
        &mut server,
        req("POST", "/api/auth/login", &[], login),
        &now,
        &mut random,
    );
    assert_eq!(blocked.status, 429);
    assert!(blocked.headers.iter().any(|(k, _)| k == "Retry-After"));
    let later = Timestamp::from_time(d::Time(now.time().unwrap().0 + 61_000_000_000)).unwrap();
    let accepted = response(
        &mut server,
        req("POST", "/api/auth/login", &[], login),
        &later,
        &mut random,
    );
    assert_eq!(accepted.status, 200);
    let huge = vec![0; http::MAX_BODY + 1];
    assert_eq!(
        response(
            &mut server,
            req("POST", "/api/auth/login", &[], &huge),
            &now,
            &mut random
        )
        .status,
        413
    );
}

#[test]
fn runner_registration_replay_generation_and_token_revocation() {
    use d::protocol as p;
    let fail = Cell::new(false);
    let mut server = Server::new(Store::new(PersistedState::default(), Memory(&fail)));
    let mut random = Random(8000);
    let now = time();
    server.ensure_worker_token("seed", &mut random).unwrap();
    server
        .ensure_worker_token("old-deployment-seed", &mut random)
        .unwrap();
    assert_eq!(server.store.worker_token(), "seed");
    assert_eq!(
        response(
            &mut server,
            req(
                "GET",
                "/api/state",
                &[("Authorization", "Bearer seed")],
                b""
            ),
            &now,
            &mut random
        )
        .status,
        401
    );
    let open = |server: &mut Server<_>, random: &mut Random| match server
        .begin(
            req(
                "GET",
                "/api/runner/ws",
                &[("Authorization", "Bearer seed")],
                b"",
            ),
            &now,
            random,
        )
        .unwrap()
    {
        Outcome::Runner(link) => link,
        _ => panic!("expected runner"),
    };
    let hello = || {
        p::WireMessage::from_json(br#"{"type":"hello","version":1,"instance_id":"host-a","name":"A","process":"process-1","capabilities":{"engine":{"available":true,"driver":"docker"}},"streams_reported":true,"capsules":{"compositions":["orphan"],"recordings":[]}}"#).unwrap()
    };
    let mut first = open(&mut server, &mut random);
    let client = match server
        .runner_message(&mut first, hello(), &now, 100, &mut random)
        .unwrap()
    {
        RunnerEvent::Attached { client, .. } => client,
        _ => panic!("not attached"),
    };
    assert!(server.runner_info().unwrap().available);
    let (ticket, welcome) = server.runner_next(&first).unwrap().unwrap();
    assert_eq!(welcome.r#type, p::MESSAGE_WELCOME);
    assert!(server.runner_acknowledge(&first, ticket));
    let (old_ticket, cleanup) = server.runner_next(&first).unwrap().unwrap();
    assert_eq!(cleanup.method, p::METHOD_REMOVE_CAPSULES);
    let request_id = cleanup.id.try_clone().unwrap();
    // Een gelijktijdig tweede proces mag de geregistreerde naam niet vervalsen.
    let mut duplicate = open(&mut server, &mut random);
    let mut other = hello();
    other.process = try_string("process-2").unwrap();
    other.name = try_string("spoofed").unwrap();
    assert!(
        server
            .runner_message(&mut duplicate, other, &now, 101, &mut random)
            .is_err()
    );
    assert_eq!(server.store.client(&client).unwrap().name, "A");
    let mut second = open(&mut server, &mut random);
    let mut reconnect = hello();
    reconnect.capsules = None;
    reconnect.capabilities.engine.driver = try_string("new-driver").unwrap();
    server
        .runner_message(&mut second, reconnect, &now, 102, &mut random)
        .unwrap();
    assert!(!server.runner_acknowledge(&first, old_ticket));
    assert!(server.runner_touch(&first, 1000).is_err());
    assert!(
        server
            .runner_message(
                &mut first,
                p::WireMessage {
                    r#type: try_string(p::MESSAGE_RESPONSE).unwrap(),
                    id: request_id.try_clone().unwrap(),
                    ..Default::default()
                },
                &now,
                103,
                &mut random
            )
            .is_err()
    );
    server.runner_disconnect(first, &now).unwrap();
    assert_eq!(server.store.client(&client).unwrap().status, "online");
    assert_eq!(server.runner_info().unwrap().driver, "runner/new-driver");
    let (ticket, welcome) = server.runner_next(&second).unwrap().unwrap();
    assert_eq!(welcome.r#type, p::MESSAGE_WELCOME);
    server.runner_acknowledge(&second, ticket);
    let (ticket, replay) = server.runner_next(&second).unwrap().unwrap();
    assert_eq!(replay.id, request_id);
    server.runner_acknowledge(&second, ticket);
    assert!(matches!(
        server
            .runner_message(
                &mut second,
                p::WireMessage {
                    r#type: try_string(p::MESSAGE_RESPONSE).unwrap(),
                    id: request_id,
                    ..Default::default()
                },
                &now,
                104,
                &mut random
            )
            .unwrap(),
        RunnerEvent::Response { .. }
    ));
    fail.set(true);
    assert!(server.store.replace_worker_token("fresh").is_err());
    assert!(server.validate_runner(&second, &now).is_ok());
    fail.set(false);
    server.store.replace_worker_token("fresh").unwrap();
    assert!(server.validate_runner(&second, &now).is_err());
    server.runner_disconnect(second, &now).unwrap();
    assert_eq!(server.store.client(&client).unwrap().status, "offline");
    assert!(!server.runner_info().unwrap().available);
}

#[test]
fn capsule_start_wait_cancel_and_failed_commit_are_recoverable() {
    use d::protocol as p;
    let fail = Cell::new(false);
    let mut server = Server::new(Store::new(PersistedState::default(), Memory(&fail)));
    let mut random = Random(16000);
    let now = time();
    server.ensure_worker_token("seed", &mut random).unwrap();
    let start = |server: &mut Server<_>, random: &mut Random| {
        let request = req(
            "POST",
            "/api/recordings",
            &[],
            br#"{"kind":"tool","name":"tool"}"#,
        );
        let Some(Outcome::Capsule(wait)) = server
            .capsule_route(&request, "derek", &now, random)
            .unwrap()
        else {
            panic!("expected async start")
        };
        wait
    };
    let wait = start(&mut server, &mut random);
    assert!(server.poll_capsule(&wait, &now).unwrap().is_none());
    let later = Timestamp::from_time(d::Time(now.time().unwrap().0 + 4_000_000_000)).unwrap();
    let progress = server.poll_capsule(&wait, &later).unwrap().unwrap();
    assert_eq!(progress.status, 202);
    let progress = d::StartStatus::from_json(&progress.body).unwrap();
    assert_eq!(progress.status, "running");
    let path = spin_core::validation::text(format_args!(
        "/api/recordings/{}/cancel",
        progress.recording_id
    ))
    .unwrap();
    let Some(Outcome::Response(cancelled)) = server
        .capsule_route(
            &req("POST", &path, &[], b"{}"),
            "derek",
            &later,
            &mut random,
        )
        .unwrap()
    else {
        panic!("expected cancellation")
    };
    assert_eq!(cancelled.status, 200);
    server.maintain_capsules(&later, &mut random).unwrap();
    assert!(server.store.starting_recordings().unwrap().is_empty());
    let wait = start(&mut server, &mut random);
    let Outcome::Runner(mut link) = server
        .begin(
            req(
                "GET",
                "/api/runner/ws",
                &[("Authorization", "Bearer seed")],
                b"",
            ),
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!("runner")
    };
    server.runner_message(&mut link, p::WireMessage::from_json(br#"{"version":1,"type":"hello","instance_id":"a","process":"one","name":"A","capabilities":{"engine":{"available":true}}}"#).unwrap(), &now, 1, &mut random).unwrap();
    let (ticket, _) = server.runner_next(&link).unwrap().unwrap();
    server.runner_acknowledge(&link, ticket);
    server.maintain_capsules(&later, &mut random).unwrap();
    let (ticket, accepts) = server.runner_next(&link).unwrap().unwrap();
    assert_eq!(accepts.method, p::METHOD_ACCEPTS);
    let accept_id = accepts.id.try_clone().unwrap();
    server.runner_acknowledge(&link, ticket);
    server
        .runner_message(
            &mut link,
            p::WireMessage {
                r#type: "response".into(),
                id: accept_id,
                payload: d::RawJson(Some(d::json::parse(br#"{"accepts":true}"#).unwrap())),
                ..Default::default()
            },
            &later,
            2,
            &mut random,
        )
        .unwrap();
    let (ticket, command) = server.runner_next(&link).unwrap().unwrap();
    assert_eq!(command.method, p::METHOD_START_RECORDING);
    let command_id = command.id.try_clone().unwrap();
    server.runner_acknowledge(&link, ticket);
    fail.set(true);
    server
        .runner_message(
            &mut link,
            p::WireMessage {
                r#type: "response".into(),
                id: command_id,
                payload: d::RawJson(Some(
                    d::json::parse(
                        br#"{"driver":"docker","container_id":"container","status":"ready"}"#,
                    )
                    .unwrap(),
                )),
                ..Default::default()
            },
            &later,
            3,
            &mut random,
        )
        .unwrap();
    assert_eq!(
        server.poll_capsule(&wait, &later).unwrap().unwrap().status,
        500
    );
    assert_eq!(server.store.starting_recordings().unwrap().len(), 1);
    fail.set(false);
    server.maintain_capsules(&later, &mut random).unwrap();
    let (_, command) = server.runner_next(&link).unwrap().unwrap();
    assert_eq!(command.method, p::METHOD_ACCEPTS);
}

#[test]
fn materialization_places_rotated_logins_and_cleans_up_failed_preparation() {
    use d::protocol as p;
    let fail = Cell::new(false);
    let state = PersistedState::from_json(br#"{"artifacts":{"layer":{"id":"layer","kind":"tool","name":"agent","scope":"global","profile":"default","tracked_paths":["/root/config"],"enables":[{"name":"acp"}]}}}"#).unwrap();
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let mut random = Random(20000);
    let now = time();
    server.ensure_worker_token("seed", &mut random).unwrap();
    let Outcome::Runner(mut link) = server
        .begin(
            req(
                "GET",
                "/api/runner/ws",
                &[("Authorization", "Bearer seed")],
                b"",
            ),
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!("runner")
    };
    server.runner_message(&mut link, p::WireMessage::from_json(br#"{"version":1,"type":"hello","instance_id":"a","process":"one","name":"A","capabilities":{"engine":{"available":true}}}"#).unwrap(), &now, 1, &mut random).unwrap();
    let (ticket, _) = server.runner_next(&link).unwrap().unwrap();
    server.runner_acknowledge(&link, ticket);
    let start = |server: &mut Server<_>, random: &mut Random| {
        let Some(Outcome::Capsule(wait)) = server
            .capsule_route(
                &req("POST", "/api/use", &[], br#"{"selector":"tool:agent"}"#),
                "derek",
                &now,
                random,
            )
            .unwrap()
        else {
            panic!("materialize")
        };
        wait
    };
    let exchange = |server: &mut Server<_>,
                    link: &mut RunnerLink,
                    random: &mut Random,
                    method: &str,
                    payload: &str,
                    error: &str| {
        let (ticket, request) = server.runner_next(link).unwrap().unwrap();
        assert_eq!(request.method, method);
        let request = request.try_clone().unwrap();
        server.runner_acknowledge(link, ticket);
        server
            .runner_message(
                link,
                p::WireMessage {
                    r#type: "response".into(),
                    id: request.id.try_clone().unwrap(),
                    payload: d::RawJson(Some(d::json::parse(payload.as_bytes()).unwrap())),
                    error: error.into(),
                    ..Default::default()
                },
                &now,
                2,
                random,
            )
            .unwrap();
        request
    };
    let wait = start(&mut server, &mut random);
    exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_ACCEPTS,
        r#"{"accepts":true}"#,
        "",
    );
    exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_MATERIALIZE,
        r#"{"driver":"docker","container_id":"container-1","status":"ready"}"#,
        "",
    );
    assert!(server.poll_capsule(&wait, &now).unwrap().is_none());
    exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_READ_TRACKED,
        r#"{"/root/config":"Zmlyc3Q="}"#,
        "",
    );
    exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_WATCH_TRACKED,
        "null",
        "",
    );
    let response = server.poll_capsule(&wait, &now).unwrap().unwrap();
    assert_eq!(response.status, 201);
    let composition = d::Composition::from_json(&response.body).unwrap();
    let (_, login_id) = composition.logins.iter().next().unwrap();
    server.runner_message(&mut link, p::WireMessage { r#type: "event".into(), method: p::METHOD_TRACKED_CHANGED.into(), payload: d::RawJson(Some(d::json::parse(br#"{"runtime":{"container_id":"container-1"},"paths":["/root/config"],"files":{"/root/config":"cm90YXRlZA==","/root/not-tracked":"aWdub3JlZA=="}}"#).unwrap())), ..Default::default() }, &now, 3, &mut random).unwrap();
    assert_eq!(
        server
            .store
            .login(login_id)
            .unwrap()
            .files
            .get("/root/config")
            .unwrap()
            .0
            .as_deref(),
        Some(b"rotated".as_slice())
    );
    assert_eq!(server.store.login(login_id).unwrap().files.len(), 1);
    let second = start(&mut server, &mut random);
    exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_ACCEPTS,
        r#"{"accepts":true}"#,
        "",
    );
    let built = exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_MATERIALIZE,
        r#"{"driver":"docker","container_id":"container-2","status":"ready"}"#,
        "",
    );
    let built = p::MaterializePayload::from_value(built.payload.0.as_ref().unwrap()).unwrap();
    let write = exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_WRITE_TRACKED,
        "null",
        "write refused",
    );
    let write = p::TrackedFilesPayload::from_value(write.payload.0.as_ref().unwrap()).unwrap();
    assert_eq!(
        write.files.get("/root/config").unwrap().0.as_deref(),
        Some(b"rotated".as_slice())
    );
    assert!(server.poll_capsule(&second, &now).unwrap().is_none());
    assert!(
        server
            .store
            .composition(&built.composition.id)
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .stop_pending
    );
    exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_STOP,
        "null",
        "",
    );
    assert_eq!(
        server.poll_capsule(&second, &now).unwrap().unwrap().status,
        500
    );
    assert_eq!(
        server
            .store
            .composition(&built.composition.id)
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .status,
        "stopped"
    );
}

#[test]
fn offline_stop_survives_restart_captures_logins_and_releases_only_after_ack() {
    use core::cell::RefCell;
    use d::protocol as p;
    struct Saved<'a>(&'a RefCell<PersistedState>);
    impl Persistence for Saved<'_> {
        fn save(&mut self, state: &PersistedState) -> spin_store::Result {
            *self.0.borrow_mut() = state.try_clone()?;
            Ok(())
        }
    }
    let state = PersistedState::from_json(br#"{
        "artifacts":{"layer":{"id":"layer","kind":"credential","name":"agent","tracked_paths":["/root/token"]}},
        "logins":{"login":{"id":"login","key":"/credential:agent","owner":"derek","files":{"/root/token":"b2xk"}}},
        "compositions":{"cmp":{"id":"cmp","operator":"derek","layers":["layer"],"logins":{"/credential:agent":"login"},"runtime":{"client_id":"client","container_id":"container","status":"ready"}}}
    }"#).unwrap();
    let saved = RefCell::new(state.try_clone().unwrap());
    let mut server = Server::new(Store::new(state, Saved(&saved)));
    let now = time();
    let mut random = Random(90000);
    let Some(Outcome::Response(response)) = server
        .capsule_route(
            &req("POST", "/api/compositions/cmp/stop", &[], b""),
            "derek",
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!("offline stop should be accepted");
    };
    assert_eq!(response.status, 202);
    assert!(
        server
            .store
            .composition("cmp")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .stop_pending
    );
    assert!(
        !server
            .store
            .logins_free("/credential:agent", true, "derek")
            .unwrap()
    );
    // Reconstruct from the durable state: there are no surviving operation handles.
    let state = saved.borrow().try_clone().unwrap();
    let mut server = Server::new(Store::new(state, Saved(&saved)));
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    let (generation, _) = peer.attach("runner", None, 0).unwrap();
    server.runners.push(peer);
    server.maintain_capsules(&now, &mut random).unwrap();
    for (method, payload) in [
        (p::METHOD_READ_TRACKED, r#"{"/root/token":"cm90YXRlZA=="}"#),
        (p::METHOD_CAPSULE_CHANGES, r#"{"files":2,"bytes":40}"#),
        (p::METHOD_STOP, "null"),
    ] {
        assert!(
            !server
                .store
                .logins_free("/credential:agent", true, "derek")
                .unwrap()
        );
        let (ticket, message) = server.runners[0].next(generation).unwrap().unwrap();
        let message = message.try_clone().unwrap();
        assert_eq!(message.method, method);
        server.runners[0].acknowledge(ticket);
        server.runners[0].response(&message.id);
        server
            .finish_capsule(
                "client",
                Some(&message),
                &p::WireMessage {
                    id: message.id.try_clone().unwrap(),
                    payload: d::RawJson(Some(Value::from_json(payload.as_bytes()).unwrap())),
                    ..Default::default()
                },
                &now,
                &mut random,
            )
            .unwrap();
    }
    let composition = server.store.composition("cmp").unwrap();
    assert_eq!(composition.runtime.as_ref().unwrap().status, "stopped");
    assert!(!composition.runtime.as_ref().unwrap().stop_pending);
    assert_eq!(composition.capsule_changes.as_ref().unwrap().files, 2);
    assert_eq!(
        server
            .store
            .login("login")
            .unwrap()
            .files
            .get("/root/token")
            .unwrap()
            .0
            .as_deref(),
        Some(b"rotated".as_slice())
    );
    assert!(
        server
            .store
            .logins_free("/credential:agent", true, "derek")
            .unwrap()
    );
}

#[test]
fn human_accept_publishes_before_answering_and_failure_keeps_decision_open() {
    use d::protocol as p;
    let state = PersistedState::from_json(br#"{
      "jobs":{"job":{"id":"job","owner":"derek","status":"running","workflow_status":"running","current_phase_run_id":"run","branch":"jobs/#1/main","template_snapshot":{"id":"tpl","phases":[{"id":"develop","name":"Bouw","allow_changes":true,"accept":{"target":"DONE"},"reject":{"target":"SELF"}}]}}},
      "sessions":{"ses":{"id":"ses","job_id":"job","phase_run_id":"run","operator":"derek","status":"running","prepared_composition_id":"cmp","git_ref":"jobs/#1/sessions/one"}},
      "phase_runs":{"run":{"id":"run","session_id":"ses","job_id":"job","phase_id":"develop","phase_name":"Bouw","status":"running","attempt":1}},
      "compositions":{"cmp":{"id":"cmp","operator":"derek","session_id":"ses","runtime":{"driver":"docker","client_id":"client","container_id":"container","status":"ready"},"git":{"repository_id":"repo","remote_url":"https://example.test/repo.git","credential_scope":"public","bootstrap_ref":"main","mode":"change"}}}
    }"#).unwrap();
    let fail = Cell::new(false);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let now = time();
    let mut random = Random(50000);
    let question = server
        .store
        .complete_workflow_phase(
            "ses",
            "accept",
            "Review this",
            true,
            spin_store::Mutation {
                now: &now,
                ids: &mut random,
            },
        )
        .unwrap()
        .question
        .unwrap();
    let path = spin_core::validation::text(format_args!(
        "/api/workflow/questions/{}/answer",
        question.id
    ))
    .unwrap();
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    let (generation, _) = peer.attach("runner", None, 0).unwrap();
    server.runners.push(peer);
    for failure in ["push refused", ""] {
        let Some(Outcome::Capsule(wait)) = server
            .human_accept_route(
                &req(
                    "POST",
                    &path,
                    &[],
                    br#"{"action":"accept","reason":"Looks good"}"#,
                ),
                "derek",
                &now,
                &mut random,
            )
            .unwrap()
        else {
            panic!("accept must wait for publication");
        };
        assert!(
            server
                .human_accept_route(
                    &req(
                        "POST",
                        &path,
                        &[],
                        br#"{"action":"reject","reason":"Changed my mind"}"#
                    ),
                    "derek",
                    &now,
                    &mut random
                )
                .is_err()
        );
        assert_eq!(
            server.store.workflow_for_session("ses").unwrap().run.status,
            d::PHASE_RUN_PENDING
        );
        let (ticket, message) = server.runners[0].next(generation).unwrap().unwrap();
        let message = message.try_clone().unwrap();
        let payload =
            p::AcceptWorkspacePayload::from_value(message.payload.0.as_ref().unwrap()).unwrap();
        assert!(
            payload
                .acceptance
                .commit_body
                .contains("Spin-Accepted-By: user:derek")
        );
        server.runners[0].acknowledge(ticket);
        server.runners[0].response(&message.id);
        server
            .finish_capsule(
                "client",
                Some(&message),
                &p::WireMessage {
                    id: message.id.try_clone().unwrap(),
                    error: failure.into(),
                    payload: d::RawJson(Some(
                        Value::from_json(br#"{"Head":"0123456789abcdef","Committed":true}"#)
                            .unwrap(),
                    )),
                    ..Default::default()
                },
                &now,
                &mut random,
            )
            .unwrap();
        let response = server.poll_capsule(&wait, &now).unwrap().unwrap();
        let view = server.store.workflow_for_session("ses").unwrap();
        let question = view.questions.iter().find(|q| q.id == question.id).unwrap();
        if failure.is_empty() {
            assert_eq!(response.status, 200);
            assert_eq!(question.status, "answered");
            assert_eq!(question.answered_by, "derek");
            assert_eq!(view.run.status, d::PHASE_RUN_ACCEPTED);
            assert_eq!(
                d::WorkflowAdvance::from_json(&response.body)
                    .unwrap()
                    .job
                    .workflow_status,
                d::WORKFLOW_DONE
            );
        } else {
            assert_eq!(response.status, 502);
            assert_eq!(question.status, "open");
            assert_eq!(view.run.status, d::PHASE_RUN_PENDING);
        }
    }
}

#[test]
fn expose_waits_for_app_ack_and_merge_waits_for_remote_commit() {
    use d::protocol as p;
    for expose in [true, false] {
        let mut state = PersistedState::from_json(br#"{
          "jobs":{"job":{"id":"job","owner":"derek","status":"running","workflow_status":"running","current_phase_run_id":"run","branch":"jobs/#1/main","base_ref":"main","git_repository_id":"repo","git_remote_url":"https://example.test/repo.git","git_credential_scope":"public","template_snapshot":{"id":"tpl","phases":[{"id":"check","name":"Test","executor":"expose","accept":{"target":"DONE"},"reject":{"target":"SELF"}}]}}},
          "sessions":{"ses":{"id":"ses","job_id":"job","phase_run_id":"run","operator":"derek","status":"queued","executor":"expose","git_repository_id":"repo","prepared_composition_id":"cmp"}},
          "phase_runs":{"run":{"id":"run","session_id":"ses","job_id":"job","phase_id":"check","phase_name":"Test","status":"queued","attempt":1}},
          "git_repositories":{"repo":{"id":"repo","name":"Repo","remote_url":"https://example.test/repo.git","default_ref":"main","credential_scope":"public","services":[{"name":"web","run":"npm start","ports":[3000]}]}},
          "compositions":{"cmp":{"id":"cmp","operator":"derek","session_id":"ses","runtime":{"driver":"docker","client_id":"client","container_id":"container","status":"ready"}}}
        }"#).unwrap();
        if !expose {
            state.sessions.get_mut("ses").unwrap().executor = "action".into();
            let phase = state
                .jobs
                .get_mut("job")
                .unwrap()
                .template_snapshot
                .as_mut()
                .unwrap()
                .phases
                .as_mut_slice()
                .get_mut(0)
                .unwrap();
            phase.executor = "action".into();
            phase.action = Some(d::WorkflowAction::from_json(br#"{"type":"git.merge"}"#).unwrap());
        }
        let failed = Cell::new(false);
        let mut server = Server::new(Store::new(state, Memory(&failed)));
        let now = time();
        let mut random = Random(300);
        let mut peer = spin_core::runner::Peer::new(d::Client {
            id: "client".into(),
            capabilities: d::ClientCapabilities::from_json(br#"{"engine":{"available":true}}"#)
                .unwrap(),
            ..Default::default()
        });
        let (generation, _) = peer
            .attach("runner", None, now.time().unwrap().0 / 1_000_000)
            .unwrap();
        server.runners.push(peer);
        let session = server.store.session("ses").unwrap().try_clone().unwrap();
        server
            .schedule_session(&session, &now, &mut random)
            .unwrap();
        server
            .schedule_session(&session, &now, &mut random)
            .unwrap();
        assert_eq!(
            server.calls.len(),
            1,
            "repeated sweep must not repeat an external action"
        );
        assert_eq!(
            server.store.workflow_for_session("ses").unwrap().run.status,
            d::PHASE_RUN_RUNNING
        );
        let (ticket, request) = server.runners[0].next(generation).unwrap().unwrap();
        let request = request.try_clone().unwrap();
        assert_eq!(
            request.method,
            if expose {
                p::METHOD_START_APP
            } else {
                p::METHOD_MERGE_REPOSITORY
            }
        );
        server.runners[0].acknowledge(ticket);
        server.runners[0].response(&request.id);
        let payload = if expose {
            br#"{"services":[{"service":"web","status":"running","host":"runner.test","ports":{"3000":45123},"reachable":true}]}"#.as_slice()
        } else {
            br#"{"Head":"0123456789012345678901234567890123456789"}"#.as_slice()
        };
        server
            .finish_capsule(
                "client",
                Some(&request),
                &p::WireMessage {
                    id: request.id.try_clone().unwrap(),
                    payload: d::RawJson(Some(Value::from_json(payload).unwrap())),
                    ..Default::default()
                },
                &now,
                &mut random,
            )
            .unwrap();
        let view = server.store.workflow_for_session("ses").unwrap();
        if expose {
            assert_eq!(view.run.status, d::PHASE_RUN_PENDING);
            assert!(
                view.questions
                    .iter()
                    .any(|q| q.question.contains("runner.test:45123"))
            );
        } else {
            assert_eq!(view.job.workflow_status, d::WORKFLOW_DONE);
            assert_eq!(
                view.run.action_result.unwrap().external_id,
                "0123456789012345678901234567890123456789"
            );
        }
    }
}

#[test]
fn oauth_binds_state_to_browser_uses_pkce_and_never_exposes_provider_tokens() {
    let state=PersistedState::from_json(br#"{
      "users":{"user":{"id":"user","username":"derek","role":"admin"}},
      "auth_sessions":{"auth":{"id":"auth","user_id":"user","token_hash":"browser","expires_at":"2100-01-01T00:00:00Z"}}
    }"#).unwrap();
    let fail = Cell::new(false);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    server.set_public_url("https://spin.test").unwrap();
    server
        .set_oauth_environment("github", "client-id", "client-secret")
        .unwrap();
    let user = d::User {
        id: "user".into(),
        username: "derek".into(),
        role: "admin".into(),
        ..Default::default()
    };
    let now = time();
    let mut random = Random(600);
    let Some(Outcome::Response(response)) = server
        .oauth_route(
            &req("GET", "/api/git/oauth/github/start", &[], b""),
            &user,
            "browser",
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!("redirect expected")
    };
    let location = response
        .headers
        .iter()
        .find(|(k, _)| k == "Location")
        .unwrap()
        .1
        .as_str();
    assert!(location.starts_with("https://github.com/login/oauth/authorize?"));
    assert!(!location.contains("client-secret"));
    assert!(location.contains("code_challenge_method=S256"));
    let mut callback = req("GET", "/api/git/oauth/github/callback", &[], b"");
    let state = location
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let query = alloc::format!("state={state}&code=the-code");
    callback.raw_query = &query;
    assert!(
        server
            .oauth_route(&callback, &user, "another-browser", &now, &mut random)
            .is_err()
    );
    let Some(Outcome::Network(wait)) = server
        .oauth_route(&callback, &user, "browser", &now, &mut random)
        .unwrap()
    else {
        panic!("network wait expected")
    };
    assert!(
        server
            .oauth_route(&callback, &user, "browser", &now, &mut random)
            .is_err()
    );
    let request = server.take_network_request().unwrap();
    assert_eq!(request.url, "https://github.com/login/oauth/access_token");
    let body = core::str::from_utf8(&request.body).unwrap();
    assert!(body.contains("client_secret=client-secret"));
    assert!(body.contains("code_verifier="));
    server.finish_network(&request.id,Ok(NetworkResponse {status:200,body:br#"{"access_token":"PRIVATE-ACCESS","refresh_token":"PRIVATE-REFRESH","token_type":"bearer","expires_in":3600}"#.to_vec()}),&now,&mut random).unwrap();
    assert!(server.poll_network(&wait).unwrap().is_none());
    let request = server.take_network_request().unwrap();
    assert_eq!(request.url, "https://api.github.com/user");
    assert!(
        request
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer PRIVATE-ACCESS")
    );
    server
        .finish_network(
            &request.id,
            Ok(NetworkResponse {
                status: 200,
                body: br#"{"id":42,"login":"derek","name":"Derek"}"#.to_vec(),
            }),
            &now,
            &mut random,
        )
        .unwrap();
    let response = server.poll_network(&wait).unwrap().unwrap();
    assert_eq!(response.status, 302);
    assert_eq!(
        response
            .headers
            .iter()
            .find(|(k, _)| k == "Location")
            .unwrap()
            .1,
        "/?git_oauth=connected#git"
    );
    let account = server
        .store
        .resolve_git_workspace_account(
            &d::GitWorkspace {
                remote_url: "https://github.com/derek/repo.git".into(),
                provider: "github".into(),
                credential_scope: "user".into(),
                ..Default::default()
            },
            "derek",
        )
        .unwrap();
    assert_eq!(account.access_token, "PRIVATE-ACCESS");
    assert_eq!(account.email, "derek@users.noreply.github.com");
    let public = server.state_for(&user).unwrap().to_json().unwrap();
    assert!(!public.contains("PRIVATE-ACCESS"));
    assert!(!public.contains("PRIVATE-REFRESH"));
    assert!(!public.contains("client-secret"));
}

#[test]
fn pull_request_action_reuses_existing_pr_and_creates_only_after_empty_lookup() {
    for existing in [false, true] {
        let state=PersistedState::from_json(br#"{
          "jobs":{"job":{"id":"job","owner":"derek","title":"Feature","objective":"Build it","status":"running","workflow_status":"running","current_phase_run_id":"run","branch":"jobs/#1/main","base_ref":"main","git_repository_id":"repo","git_remote_url":"https://github.com/example/repo.git","git_provider":"github","git_credential_scope":"user","template_snapshot":{"id":"tpl","phases":[{"id":"publish","executor":"action","action":{"type":"git.pull_request.create"},"accept":{"target":"DONE"},"reject":{"target":"SELF"}}]}}},
          "sessions":{"ses":{"id":"ses","job_id":"job","phase_run_id":"run","operator":"derek","status":"queued","executor":"action"}},
          "phase_runs":{"run":{"id":"run","session_id":"ses","job_id":"job","phase_id":"publish","status":"queued","attempt":1}},
          "git_accounts":{"account":{"id":"account","operator":"derek","provider":"github","host":"github.com","credential_scope":"user","access_token":"TOKEN","updated_at":"2026-09-29T12:00:00Z"}}
        }"#).unwrap();
        let fail = Cell::new(false);
        let mut server = Server::new(Store::new(state, Memory(&fail)));
        let now = time();
        let mut random = Random(800);
        let session = server.store.session("ses").unwrap().try_clone().unwrap();
        server
            .schedule_session(&session, &now, &mut random)
            .unwrap();
        server
            .schedule_session(&session, &now, &mut random)
            .unwrap();
        assert_eq!(server.network.len(), 1);
        let request = server.take_network_request().unwrap();
        assert_eq!(request.method, "GET");
        assert!(request.url.contains("head=example%3Ajobs%2F%231%2Fmain"));
        assert!(
            request
                .headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer TOKEN")
        );
        let pull = br#"{"number":7,"html_url":"https://github.com/example/repo/pull/7"}"#;
        let body = if existing {
            alloc::format!("[{}]", core::str::from_utf8(pull).unwrap()).into_bytes()
        } else {
            b"[]".to_vec()
        };
        server
            .finish_network(
                &request.id,
                Ok(NetworkResponse { status: 200, body }),
                &now,
                &mut random,
            )
            .unwrap();
        if !existing {
            assert_eq!(
                server.store.workflow_for_session("ses").unwrap().run.status,
                d::PHASE_RUN_RUNNING
            );
            let request = server.take_network_request().unwrap();
            assert_eq!(request.method, "POST");
            assert_eq!(
                request.url,
                "https://api.github.com/repos/example/repo/pulls"
            );
            let payload = Value::from_json(&request.body).unwrap();
            assert_eq!(
                payload.as_object().unwrap().get("head").unwrap().as_str(),
                Some("jobs/#1/main")
            );
            server
                .finish_network(
                    &request.id,
                    Ok(NetworkResponse {
                        status: 201,
                        body: pull.to_vec(),
                    }),
                    &now,
                    &mut random,
                )
                .unwrap();
        }
        assert!(server.network.is_empty());
        let view = server.store.workflow_for_session("ses").unwrap();
        assert_eq!(view.job.workflow_status, d::WORKFLOW_DONE);
        assert_eq!(
            view.run.action_result.unwrap().url,
            "https://github.com/example/repo/pull/7"
        );
    }
}

#[test]
fn oauth_refresh_does_not_overwrite_a_newer_manual_connection() {
    for reconnect in [false, true] {
        let state=PersistedState::from_json(br#"{"git_accounts":{"account":{"id":"account","operator":"derek","provider":"github","host":"github.com","provider_id":"42","login":"derek","credential_scope":"user","access_token":"old-access","refresh_token":"old-refresh","updated_at":"2026-09-29T12:00:00Z","expires_at":"2026-09-29T13:00:00Z"}}}"#).unwrap();
        let fail = Cell::new(false);
        let mut server = Server::new(Store::new(state, Memory(&fail)));
        server
            .set_oauth_environment("github", "client", "secret")
            .unwrap();
        let now = time();
        let mut random = Random(900);
        server.maintain_network(&now, &mut random).unwrap();
        server.maintain_network(&now, &mut random).unwrap();
        assert_eq!(server.network.len(), 1);
        let request = server.take_network_request().unwrap();
        assert!(
            core::str::from_utf8(&request.body)
                .unwrap()
                .contains("grant_type=refresh_token")
        );
        if reconnect {
            let mut account = server
                .store
                .git_account("account", "derek")
                .unwrap()
                .try_clone()
                .unwrap();
            account.access_token = "manual-access".into();
            account.refresh_token = "manual-refresh".into();
            server
                .store
                .save_git_account(
                    account,
                    spin_store::Context {
                        now: &now,
                        id: "account",
                    },
                )
                .unwrap();
        }
        server.finish_network(&request.id,Ok(NetworkResponse{status:200,body:br#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#.to_vec()}),&now,&mut random).unwrap();
        assert!(server.network.is_empty());
        let account = server.store.git_account("account", "derek").unwrap();
        assert_eq!(
            account.access_token,
            if reconnect {
                "manual-access"
            } else {
                "new-access"
            }
        );
        assert_eq!(
            account.refresh_token,
            if reconnect {
                "manual-refresh"
            } else {
                "new-refresh"
            }
        );
    }
}

#[test]
fn job_delete_waits_for_stop_ack_and_close_keeps_offline_login_reserved() {
    use d::protocol as p;
    let state = PersistedState::from_json(br#"{
      "jobs":{"job":{"id":"job","owner":"derek","status":"active","session_ids":["ses"]}},
      "sessions":{"ses":{"id":"ses","job_id":"job","operator":"derek","prepared_composition_id":"cmp"}},
      "compositions":{"cmp":{"id":"cmp","operator":"derek","session_id":"ses","logins":{"derek/token":"login"},"runtime":{"client_id":"runner","container_id":"box","status":"ready"}}},
      "logins":{"login":{"id":"login","key":"derek/token","number":1}}
    }"#).unwrap();
    let fail = Cell::new(false);
    let now = time();
    let mut random = Random(500);
    let mut server = Server::new(Store::new(state.try_clone().unwrap(), Memory(&fail)));
    assert!(
        server
            .operation_route(
                &req("DELETE", "/api/jobs/job", &[], b""),
                "",
                &now,
                &mut random
            )
            .is_err()
    );
    let Some(Outcome::Operation(wait)) = server
        .operation_route(
            &req("DELETE", "/api/jobs/job", &[], b""),
            "derek",
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!("operation expected")
    };
    server.maintain_operations(&now, &mut random).unwrap();
    assert!(server.poll_operation(&wait).unwrap().is_none());
    assert!(server.store.job("job").is_ok());
    assert!(server.store.delete_login("login").is_err());
    assert!(
        server
            .store
            .composition("cmp")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .stop_pending
    );
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "runner".into(),
        ..Default::default()
    });
    let (generation, _) = peer.attach("instance", None, 0).unwrap();
    server.runners.push(peer);
    server.maintain_operations(&now, &mut random).unwrap();
    for method in [
        p::METHOD_STOP_APP,
        p::METHOD_CAPSULE_CHANGES,
        p::METHOD_STOP,
    ] {
        let (ticket, message) = server.runners[0].next(generation).unwrap().unwrap();
        let message = message.try_clone().unwrap();
        assert_eq!(message.method, method);
        server.runners[0].acknowledge(ticket);
        server.runners[0].response(&message.id).unwrap();
        assert!(server.store.job("job").is_ok());
        assert!(server.store.delete_login("login").is_err());
        server
            .finish_capsule(
                "runner",
                Some(&message),
                &p::WireMessage {
                    id: message.id.try_clone().unwrap(),
                    payload: d::RawJson(Some(d::LayerContents::default().to_value().unwrap())),
                    ..Default::default()
                },
                &now,
                &mut random,
            )
            .unwrap();
    }
    server.maintain_operations(&now, &mut random).unwrap();
    assert_eq!(server.poll_operation(&wait).unwrap().unwrap().status, 200);
    assert!(server.store.job("job").is_err());
    assert!(server.store.delete_login("login").is_ok());

    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let Some(Outcome::Operation(wait)) = server
        .operation_route(
            &req("POST", "/api/jobs/job/close", &[], b""),
            "derek",
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!("operation expected")
    };
    server.maintain_operations(&now, &mut random).unwrap();
    assert_eq!(server.poll_operation(&wait).unwrap().unwrap().status, 200);
    assert_eq!(server.store.job("job").unwrap().status, d::JOB_CANCELLED);
    assert!(server.store.delete_login("login").is_err());
    assert!(
        server
            .store
            .composition("cmp")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .stop_pending
    );
}

#[test]
fn management_preserves_tree_contract_and_hides_credentials() {
    let state = PersistedState::from_json(br#"{
      "artifacts":{"art":{"id":"art","kind":"credential","name":"token","subject":"derek","created_by":"derek","scope":"user","enables":[{"name":"acp","command":"old"}]},"child":{"id":"child","kind":"tool","name":"child","parent_artifact_ids":["art"]}},
      "logins":{"login":{"id":"login","key":"derek/token","number":1,"files":{"/root/token":"c2VjcmV0"}}}
    }"#).unwrap();
    let fail = Cell::new(false);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let owner = d::User {
        username: "derek".into(),
        role: d::USER_MEMBER.into(),
        ..Default::default()
    };
    let stranger = d::User {
        username: "other".into(),
        role: d::USER_MEMBER.into(),
        ..Default::default()
    };
    let request = req(
        "PUT",
        "/api/artifacts/art/enablements/acp",
        &[],
        br#"{"command":"new-agent"}"#,
    );
    assert_eq!(
        server
            .management_route(&request, &stranger)
            .unwrap()
            .unwrap()
            .status,
        200
    );
    assert_eq!(
        server
            .management_route(&request, &owner)
            .unwrap()
            .unwrap()
            .status,
        200
    );
    let renamed = server
        .management_route(
            &req("PUT", "/api/logins/login/name", &[], br#"{"name":"Work"}"#),
            &owner,
        )
        .unwrap()
        .unwrap();
    let body = core::str::from_utf8(&renamed.body).unwrap();
    assert!(body.contains("Work"));
    assert!(!body.contains("c2VjcmV0"));
    assert!(!body.contains("/root/token"));
    let tree = server
        .management_route(&req("GET", "/api/artifacts/art/tree", &[], b""), &owner)
        .unwrap()
        .unwrap();
    let tree = Value::from_json(&tree.body).unwrap();
    let members = tree.as_object().unwrap().get("members").unwrap();
    let members = d::List::<Value>::from_value(members).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(
        members.as_slice()[0]
            .as_object()
            .unwrap()
            .get("id")
            .unwrap()
            .as_str(),
        Some("child")
    );
}

#[test]
fn backup_tickets_are_admin_browser_bound_single_use_and_cancel_releases_storage() {
    use spin_store::backup::{Reply, Request as Q};
    struct Backup<'a>(&'a Cell<bool>);
    impl Persistence for Backup<'_> {
        fn save(&mut self, _: &PersistedState) -> spin_store::Result {
            assert!(!self.0.get());
            Ok(())
        }
        fn backup(&mut self, request: Q) -> spin_store::Result<Reply> {
            Ok(match request {
                Q::Begin => {
                    assert!(!self.0.replace(true));
                    Reply::Ready {
                        size: 200_000,
                        key: "fixture-key".into(),
                    }
                }
                Q::Read { offset, length } => {
                    assert!(self.0.get());
                    assert!(offset + length as u64 <= 200_000);
                    Reply::Bytes(vec![42; length])
                }
                Q::End => {
                    self.0.set(false);
                    Reply::Done
                }
            })
        }
    }
    let busy = Cell::new(false);
    let mut server = Server::new(Store::new(PersistedState::default(), Backup(&busy)));
    let now = time();
    let mut random = Random(3000);
    let admin = d::User {
        role: d::USER_ADMIN.into(),
        ..Default::default()
    };
    let member = d::User {
        role: d::USER_MEMBER.into(),
        ..Default::default()
    };
    let ticket = req("POST", "/api/backup-ticket", &[], b"{}");
    assert!(
        server
            .backup_route(&ticket, &member, "owner", &now, &mut random)
            .is_err()
    );
    let Some(Outcome::Response(ticket)) = server
        .backup_route(&ticket, &admin, "owner", &now, &mut random)
        .unwrap()
    else {
        panic!("ticket expected")
    };
    let value = Value::from_json(&ticket.body).unwrap();
    let url = value
        .as_object()
        .unwrap()
        .get("url")
        .unwrap()
        .as_str()
        .unwrap();
    let (path, query) = url.split_once('?').unwrap();
    let mut download = req("GET", path, &[], b"");
    download.raw_query = query;
    assert!(
        server
            .backup_route(&download, &admin, "other-browser", &now, &mut random)
            .is_err()
    );
    assert!(!busy.get());
    let Some(Outcome::Download(first)) = server
        .backup_route(&download, &admin, "owner", &now, &mut random)
        .unwrap()
    else {
        panic!("download expected")
    };
    assert!(busy.get());
    assert!(server.backup_active());
    assert!(server.backup_chunk(&first.wait, &now).unwrap().is_some());
    assert!(
        server
            .backup_chunk(&first.wait, &now)
            .unwrap()
            .unwrap()
            .len()
            <= 65536
    );
    server.finish_backup(&first.wait).unwrap();
    assert!(!busy.get());
    assert!(!server.backup_active());
    assert!(
        server
            .backup_route(&download, &admin, "owner", &now, &mut random)
            .is_err()
    );
    let Some(Outcome::Download(second)) = server
        .backup_route(
            &req("POST", "/api/backup", &[], b""),
            &admin,
            "owner",
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!("download expected")
    };
    server.finish_backup(&first.wait).unwrap();
    assert!(busy.get(), "old stream must not unfreeze a new backup");
    let mut received = 0;
    while let Some(bytes) = server.backup_chunk(&second.wait, &now).unwrap() {
        received += bytes.len();
    }
    let length = second
        .response
        .headers
        .iter()
        .find(|(name, _)| name == "Content-Length")
        .unwrap()
        .1
        .parse::<usize>()
        .unwrap();
    assert_eq!(received, length);
    server.finish_backup(&second.wait).unwrap();
    assert!(!busy.get());
}

#[test]
fn restore_publishes_only_after_storage_commit_and_status_survives_logout() {
    use spin_store::backup::{PortableState, RestoreReply as R, RestoreRequest as Q};
    struct Restore<'a> {
        fail: &'a Cell<bool>,
        installed: &'a Cell<bool>,
        portable: PortableState,
    }
    impl Persistence for Restore<'_> {
        fn save(&mut self, _: &PersistedState) -> spin_store::Result {
            Ok(())
        }
        fn stage_restore(&mut self, q: Q) -> spin_store::Result<R> {
            Ok(match q {
                Q::Begin(_) | Q::Replica { .. } => R::Progress(0, 100),
                Q::Abort => R::Done,
                Q::Step => R::Prepared(PortableState {
                    json: self.portable.json.clone(),
                    master_key: self.portable.master_key.clone(),
                }),
            })
        }
        fn install_restore(&mut self, state: &PersistedState) -> spin_store::Result {
            assert_eq!(state.worker_token, "restored-worker");
            assert!(state.auth_sessions.is_empty());
            if self.fail.get() {
                return Err(spin_store::Error::Storage(10));
            }
            self.installed.set(true);
            Ok(())
        }
    }
    let source=PersistedState::from_json(br#"{"users":{"u":{"id":"u","username":"restored","role":"admin","password_hash":"hash"}},"worker_token":"restored-worker","auth_sessions":{"s":{"user_id":"u"}}}"#).unwrap();
    let fail = Cell::new(true);
    let installed = Cell::new(false);
    let mut random = Random(4100);
    let now = time();
    let portable =
        spin_store::backup::encrypt(&source, &spin_security::Cipher::new([13; 32]), &mut random)
            .unwrap();
    let original = PersistedState {
        worker_token: "original-worker".into(),
        ..Default::default()
    };
    let mut server = Server::new(Store::new(
        original,
        Restore {
            fail: &fail,
            installed: &installed,
            portable,
        },
    ));
    assert_eq!(
        server
            .start_restore("unguessable-first", -1, &now)
            .unwrap()
            .status,
        202
    );
    assert!(server.backup_active());
    assert!(server.maintain_restores(&now, &mut random).is_err());
    assert!(!installed.get());
    assert_eq!(server.store.worker_token(), "original-worker");
    assert!(!server.backup_active());
    let result = server
        .restore_status(
            &req("GET", "/api/restores/unguessable-first", &[], b""),
            &now,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        Value::from_json(&result.body)
            .unwrap()
            .as_object()
            .unwrap()
            .get("status")
            .unwrap()
            .as_str(),
        Some("error")
    );
    fail.set(false);
    server
        .start_restore("unguessable-second", -2, &now)
        .unwrap();
    server.maintain_restores(&now, &mut random).unwrap();
    assert!(installed.get());
    assert_eq!(server.store.worker_token(), "restored-worker");
    assert!(!server.backup_active());
    let Outcome::Response(result) = server
        .begin(
            req("GET", "/api/restores/unguessable-second", &[], b""),
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!("status response")
    };
    assert_eq!(
        Value::from_json(&result.body)
            .unwrap()
            .as_object()
            .unwrap()
            .get("status")
            .unwrap()
            .as_str(),
        Some("complete")
    );
}

#[test]
fn watcher_selection_is_reconciled_after_ack_and_removed_paths_stop_the_watcher() {
    use d::protocol as p;
    let state=PersistedState::from_json(br#"{"artifacts":{"a":{"id":"a","kind":"credential","name":"agent","subject":"derek","tracked_paths":["/root/.agent/"]}},"compositions":{"c":{"id":"c","operator":"derek","layers":["a"],"runtime":{"status":"ready","client_id":"client","container_id":"container"}}}}"#).unwrap();
    let fail = Cell::new(false);
    let now = time();
    let mut random = Random(8000);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    let (generation, _) = peer
        .attach("runner", None, now.time().unwrap().0 / 1_000_000)
        .unwrap();
    server.runners.push(peer);
    for remove in [false, true, false] {
        if remove {
            server.store.set_artifact_tracked("a", &[], &[]).unwrap();
        } else {
            server
                .store
                .set_artifact_tracked("a", &[String::from("/root/.agent/")], &[])
                .unwrap();
        }
        server.maintain_watches(&now, &mut random).unwrap();
        let (ticket, request) = server.runners[0].next(generation).unwrap().unwrap();
        let request = request.try_clone().unwrap();
        assert_eq!(request.method, p::METHOD_WATCH_TRACKED);
        let payload =
            p::TrackedFilesPayload::from_value(request.payload.0.as_ref().unwrap()).unwrap();
        assert_eq!(payload.paths.is_empty(), remove);
        server.runners[0].acknowledge(ticket);
        server.runners[0].response(&request.id).unwrap();
        server
            .finish_capsule(
                "client",
                Some(&request),
                &p::WireMessage {
                    r#type: p::MESSAGE_RESPONSE.into(),
                    id: request.id.clone(),
                    ..Default::default()
                },
                &now,
                &mut random,
            )
            .unwrap();
        assert!(server.watch_stamps.get("c").is_some());
        server.maintain_watches(&now, &mut random).unwrap();
        assert!(
            server.runners[0].next(generation).unwrap().is_none(),
            "ACK suppresses repeated watcher replacement"
        );
    }
    let failed = p::TrackedFilesPayload {
        runtime: server
            .store
            .composition("c")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .try_clone()
            .unwrap(),
        ..Default::default()
    }
    .to_value()
    .unwrap();
    server
        .tracked_watcher_stopped("foreign-client", &failed)
        .unwrap();
    assert!(server.watch_stamps.get("c").is_some());
    server.tracked_watcher_stopped("client", &failed).unwrap();
    assert!(server.watch_stamps.get("c").is_none());
    server.maintain_watches(&now, &mut random).unwrap();
    assert_eq!(
        server.runners[0]
            .next(generation)
            .unwrap()
            .unwrap()
            .1
            .method,
        p::METHOD_WATCH_TRACKED
    );
}

#[test]
fn manifest_api_reads_detached_go_listings_and_workspace_files_enforce_session_access() {
    use spin_store::{BlobInfo, BlobReply as R, BlobRequest as Q};
    struct Manifest;
    const LISTING: &[u8] = br#"[{"path":"/root/.agent/config","bytes":8,"source":"layer"}]"#;
    impl Persistence for Manifest {
        fn save(&mut self, _: &PersistedState) -> spin_store::Result {
            Ok(())
        }
        fn blob(&mut self, q: Q<'_>) -> spin_store::Result<R> {
            let info = |reference: &str| BlobInfo {
                reference: reference.into(),
                digest: "hash".into(),
                kind: "layer-manifest".into(),
                size: LISTING.len() as i64,
            };
            Ok(match q {
                Q::Info(reference) => {
                    assert_eq!(reference, "manifest:artifact:a");
                    R::Info(info(reference))
                }
                Q::Chunk { reference, offset } => {
                    assert_eq!(offset, 0);
                    R::Chunk(LISTING.to_vec(), info(reference))
                }
                _ => panic!("unexpected manifest write"),
            })
        }
    }
    let state=PersistedState::from_json(br#"{"artifacts":{"a":{"id":"a","snapshot":{"contents":{"files":1}}}},"jobs":{"j":{"id":"j","owner":"owner"}},"sessions":{"s":{"id":"s","job_id":"j","operator":"runner","prepared_composition_id":"c"}},"compositions":{"c":{"id":"c","operator":"runner","git":{"path":"legacy"},"runtime":{"status":"ready","client_id":"client","container_id":"container"}}}}"#).unwrap();
    let mut server = Server::new(Store::new(state, Manifest));
    let mut random = Random(9000);
    let now = time();
    let user = d::User {
        username: "owner".into(),
        ..Default::default()
    };
    let response = server
        .management_route(&req("GET", "/api/artifacts/a/contents", &[], b""), &user)
        .unwrap()
        .unwrap();
    let listing = Value::from_json(&response.body).unwrap();
    assert_eq!(
        listing
            .as_object()
            .unwrap()
            .get("entries")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut request = req("GET", "/api/sessions/s/file", &[], b"");
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    peer.attach("runner", None, now.time().unwrap().0 / 1_000_000)
        .unwrap();
    server.runners.push(peer);
    request.raw_query = "folder=legacy&path=src/main.rs";
    assert!(
        server
            .session_file_route(&request, "stranger", &now, &mut random)
            .is_err()
    );
    assert!(matches!(
        server
            .session_file_route(&request, "owner", &now, &mut random)
            .unwrap(),
        Some(Outcome::Capsule(_))
    ));
    request.raw_query = "folder=legacy&path=../secret";
    assert!(
        server
            .session_file_route(&request, "owner", &now, &mut random)
            .is_err()
    );
}

#[test]
fn workflow_retry_requires_git_and_stop_ack_before_replacing_the_capsule() {
    use d::protocol as p;
    let state=PersistedState::from_json(br#"{
      "jobs":{"job":{"id":"job","owner":"derek","session_ids":["ses"],"status":"running","workflow_status":"running","current_phase_run_id":"run","branch":"jobs/#1/main","template_snapshot":{"id":"tpl","phases":[{"id":"develop","name":"Bouw","allow_changes":true,"accept":{"target":"DONE"},"reject":{"target":"SELF"}}]}}},
      "sessions":{"ses":{"id":"ses","job_id":"job","phase_run_id":"run","operator":"derek","status":"running","prepared_composition_id":"cmp","git_ref":"jobs/#1/sessions/one"}},
      "phase_runs":{"run":{"id":"run","session_id":"ses","job_id":"job","phase_id":"develop","phase_name":"Bouw","status":"running","attempt":1}},
      "compositions":{"cmp":{"id":"cmp","operator":"derek","session_id":"ses","runtime":{"driver":"docker","client_id":"client","container_id":"container","status":"ready"},"git":{"repository_id":"repo","remote_url":"https://example.test/repo.git","credential_scope":"public","bootstrap_ref":"main","mode":"change"}}}
    }"#).unwrap();
    let fail = Cell::new(false);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let now = time();
    let mut random = Random(9500);
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    let (generation, _) = peer
        .attach("runner", None, now.time().unwrap().0 / 1_000_000)
        .unwrap();
    server.runners.push(peer);
    let start = |server: &mut Server<_>, random: &mut Random| {
        let Some(Outcome::Operation(wait)) = server
            .operation_route(
                &req(
                    "POST",
                    "/api/sessions/ses/retry",
                    &[],
                    br#"{"note":"try another approach"}"#,
                ),
                "derek",
                &now,
                random,
            )
            .unwrap()
        else {
            panic!("retry operation")
        };
        wait
    };
    let answer =
        |server: &mut Server<_>, random: &mut Random, method: &str, payload: &str, error: &str| {
            let (ticket, message) = server.runners[0].next(generation).unwrap().unwrap();
            let message = message.try_clone().unwrap();
            assert_eq!(message.method, method);
            server.runners[0].acknowledge(ticket);
            server.runners[0].response(&message.id);
            server
                .finish_capsule(
                    "client",
                    Some(&message),
                    &p::WireMessage {
                        id: message.id.clone(),
                        payload: d::RawJson(Some(Value::from_json(payload.as_bytes()).unwrap())),
                        error: error.into(),
                        ..Default::default()
                    },
                    &now,
                    random,
                )
                .unwrap();
        };
    let first = start(&mut server, &mut random);
    server.maintain_operations(&now, &mut random).unwrap();
    answer(
        &mut server,
        &mut random,
        p::METHOD_SYNC_WORKSPACE,
        "null",
        "push denied",
    );
    server.maintain_operations(&now, &mut random).unwrap();
    assert_eq!(server.poll_operation(&first).unwrap().unwrap().status, 502);
    assert_eq!(
        server.store.session("ses").unwrap().prepared_composition_id,
        "cmp"
    );
    assert_eq!(
        server
            .store
            .composition("cmp")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .status,
        "ready"
    );
    assert_eq!(
        server
            .store
            .workflow_for_session("ses")
            .unwrap()
            .run
            .restarts,
        0
    );
    let second = start(&mut server, &mut random);
    server.maintain_operations(&now, &mut random).unwrap();
    answer(
        &mut server,
        &mut random,
        p::METHOD_SYNC_WORKSPACE,
        r#"{"Head":"saved-head","Pushed":true}"#,
        "",
    );
    answer(
        &mut server,
        &mut random,
        p::METHOD_CAPSULE_CHANGES,
        r#"{"files":1,"bytes":8}"#,
        "",
    );
    server.maintain_operations(&now, &mut random).unwrap();
    for (method, payload) in [
        (p::METHOD_STOP_APP, "null"),
        (p::METHOD_CAPSULE_CHANGES, r#"{"files":1,"bytes":8}"#),
        (p::METHOD_STOP, "null"),
    ] {
        assert_eq!(
            server
                .store
                .workflow_for_session("ses")
                .unwrap()
                .run
                .restarts,
            0
        );
        answer(&mut server, &mut random, method, payload, "");
    }
    server.maintain_operations(&now, &mut random).unwrap();
    assert_eq!(server.poll_operation(&second).unwrap().unwrap().status, 202);
    let view = server.store.workflow_for_session("ses").unwrap();
    assert_eq!(view.run.restarts, 1);
    assert_eq!(view.run.status, d::PHASE_RUN_QUEUED);
    assert_eq!(
        server.store.session("ses").unwrap().synced_head,
        "saved-head"
    );
    assert!(
        server
            .store
            .session("ses")
            .unwrap()
            .prepared_composition_id
            .is_empty()
    );
}

#[test]
fn login_capture_and_swap_wait_for_runner_ack_and_fence_uncertain_credentials() {
    use d::protocol as p;
    let state = PersistedState::from_json(br#"{
      "artifacts":{"a":{"id":"a","kind":"credential","name":"agent","subject":"derek","tracked_paths":["/root/.agent/"]}},
      "compositions":{"c":{"id":"c","operator":"derek","for_login":true,"for_login_private":true,"layers":["a"],"runtime":{"status":"ready","client_id":"client","container_id":"container"}}}
    }"#).unwrap();
    let fail = Cell::new(false);
    let now = time();
    let mut random = Random(11000);
    let user = d::User {
        username: "derek".into(),
        ..Default::default()
    };
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    let (generation, _) = peer
        .attach("runner", None, now.time().unwrap().0 / 1_000_000)
        .unwrap();
    server.runners.push(peer);
    let begin = |server: &mut Server<_>, random: &mut Random, method: &str, path: &str| {
        let Some(Outcome::Operation(wait)) = server
            .login_operation_route(&req(method, path, &[], b""), &user, &now, random)
            .unwrap()
        else {
            panic!("login operation");
        };
        wait
    };
    let answer =
        |server: &mut Server<_>, random: &mut Random, method: &str, payload: &str, error: &str| {
            let (ticket, request) = server.runners[0].next(generation).unwrap().unwrap();
            let request = request.try_clone().unwrap();
            assert_eq!(request.method, method);
            server.runners[0].acknowledge(ticket);
            server.runners[0].response(&request.id);
            server
                .finish_capsule(
                    "client",
                    Some(&request),
                    &p::WireMessage {
                        id: request.id.clone(),
                        payload: d::RawJson(Some(Value::from_json(payload.as_bytes()).unwrap())),
                        error: error.into(),
                        ..Default::default()
                    },
                    &now,
                    random,
                )
                .unwrap();
        };
    let saved = begin(
        &mut server,
        &mut random,
        "POST",
        "/api/compositions/c/login",
    );
    server.maintain_operations(&now, &mut random).unwrap();
    assert!(server.store.login_summaries().unwrap().is_empty());
    answer(
        &mut server,
        &mut random,
        p::METHOD_READ_TRACKED,
        r#"{"/root/.agent/token":"c2VjcmV0","/outside":"bm8="}"#,
        "",
    );
    let login = server.store.login_summaries().unwrap()[0].id.clone();
    assert_eq!(server.store.login(&login).unwrap().owner, "derek");
    assert_eq!(server.store.login(&login).unwrap().files.len(), 1);
    server.maintain_operations(&now, &mut random).unwrap();
    server.maintain_operations(&now, &mut random).unwrap();
    assert!(server.poll_operation(&saved).unwrap().is_none());
    answer(
        &mut server,
        &mut random,
        p::METHOD_READ_TRACKED,
        r#"{"/root/.agent/token":"c2VjcmV0"}"#,
        "",
    );
    answer(
        &mut server,
        &mut random,
        p::METHOD_CAPSULE_CHANGES,
        "{}",
        "",
    );
    assert!(
        !server.store.login_summaries().unwrap()[0]
            .composition_id
            .is_empty()
    );
    answer(&mut server, &mut random, p::METHOD_STOP, "null", "");
    server.maintain_operations(&now, &mut random).unwrap();
    let response = server.poll_operation(&saved).unwrap().unwrap();
    assert_eq!(response.status, 201);
    let summaries = d::List::<d::LoginSummary>::from_json(&response.body).unwrap();
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].composition_id.is_empty());
    assert!(
        !core::str::from_utf8(&response.body)
            .unwrap()
            .contains("c2VjcmV0")
    );
    let runtime = d::CapsuleRuntime {
        status: "ready".into(),
        client_id: "client".into(),
        container_id: "container".into(),
        ..Default::default()
    };
    server
        .store
        .set_composition_runtime("c", "derek", runtime.try_clone().unwrap())
        .unwrap();
    server
        .store
        .create_login(
            "",
            "derek/credential:agent",
            &d::WireMap::<d::Bytes>::from_json(br#"{"/root/.agent/token":"bmV3"}"#).unwrap(),
            "",
            spin_store::Context {
                now: &now,
                id: "replacement",
            },
        )
        .unwrap();
    let path = alloc::format!("/api/logins/{login}/disabled");
    let parked = begin(&mut server, &mut random, "PUT", &path);
    server.maintain_operations(&now, &mut random).unwrap();
    answer(
        &mut server,
        &mut random,
        p::METHOD_READ_TRACKED,
        r#"{"/root/.agent/token":"cmVmcmVzaGVk"}"#,
        "",
    );
    assert_eq!(
        server
            .store
            .composition("c")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .status,
        "installing_login"
    );
    assert!(server.poll_operation(&parked).unwrap().is_none());
    // A stale watcher event while replacing files must not overwrite the reserved account.
    server
        .tracked_changed(
            "client",
            &p::TrackedFilesPayload {
                runtime: runtime.try_clone().unwrap(),
                paths: d::List::from_json(br#"["/root/.agent/"]"#).unwrap(),
                files: d::WireMap::from_json(br#"{"/root/.agent/token":"b2xk"}"#).unwrap(),
                ..Default::default()
            }
            .to_value()
            .unwrap(),
            &now,
        )
        .unwrap();
    assert_eq!(
        server
            .store
            .login("replacement")
            .unwrap()
            .files
            .get("/root/.agent/token")
            .unwrap()
            .0
            .as_deref(),
        Some(b"new".as_slice())
    );
    answer(
        &mut server,
        &mut random,
        p::METHOD_WRITE_TRACKED,
        "null",
        "write interrupted",
    );
    server.maintain_operations(&now, &mut random).unwrap();
    // No credential read follows an uncertain write; only filesystem metadata and stop.
    answer(
        &mut server,
        &mut random,
        p::METHOD_CAPSULE_CHANGES,
        "{}",
        "",
    );
    assert!(server.store.delete_login("replacement").is_err());
    answer(&mut server, &mut random, p::METHOD_STOP, "null", "");
    server.maintain_operations(&now, &mut random).unwrap();
    let response = server.poll_operation(&parked).unwrap().unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
        Value::from_json(&response.body)
            .unwrap()
            .as_object()
            .unwrap()
            .get("swapped"),
        Some(&Value::uint(0))
    );
    assert_eq!(
        server
            .store
            .login(&login)
            .unwrap()
            .files
            .get("/root/.agent/token")
            .unwrap()
            .0
            .as_deref(),
        Some(b"refreshed".as_slice())
    );
    server.store.delete_login("replacement").unwrap();
    let deleting = begin(
        &mut server,
        &mut random,
        "DELETE",
        &alloc::format!("/api/logins/{login}"),
    );
    server.maintain_operations(&now, &mut random).unwrap();
    assert_eq!(
        server.poll_operation(&deleting).unwrap().unwrap().status,
        204
    );
    assert!(server.store.login(&login).is_err());
}

#[test]
fn agent_options_use_a_temporary_capsule_and_cleanup_after_the_handshake() {
    use d::protocol as p;
    let fail = Cell::new(false);
    let state=PersistedState::from_json(br#"{"artifacts":{"layer":{"id":"layer","kind":"tool","name":"agent","scope":"global","profile":"default","enables":[{"name":"acp","command":"agent","protocol_version":1}]}}}"#).unwrap();
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let now = time();
    let mut random = Random(15000);
    server.ensure_worker_token("seed", &mut random).unwrap();
    let Outcome::Runner(mut link) = server
        .begin(
            req(
                "GET",
                "/api/runner/ws",
                &[("Authorization", "Bearer seed")],
                b"",
            ),
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!("runner");
    };
    server.runner_message(&mut link, p::WireMessage::from_json(br#"{"version":1,"type":"hello","instance_id":"a","process":"one","name":"A","capabilities":{"engine":{"available":true}}}"#).unwrap(), &now, 1, &mut random).unwrap();
    let (ticket, _) = server.runner_next(&link).unwrap().unwrap();
    server.runner_acknowledge(&link, ticket);
    assert_eq!(
        server
            .options_route(
                &req("POST", "/api/artifacts/layer/acp/options", &[], b""),
                "derek",
                &now,
                &mut random
            )
            .unwrap()
            .unwrap()
            .status,
        202
    );
    assert!(
        server
            .store
            .artifact("layer")
            .unwrap()
            .agent_options
            .as_ref()
            .unwrap()
            .fetching
    );
    server.maintain_options(&now, &mut random).unwrap();
    let composition = server.store.snapshot().unwrap().compositions[0].id.clone();
    assert_eq!(
        server
            .store
            .composition(&composition)
            .unwrap()
            .probe_artifact_id,
        "layer"
    );
    let exchange = |server: &mut Server<_>,
                    link: &mut RunnerLink,
                    random: &mut Random,
                    method: &str,
                    payload: &str| {
        let (ticket, message) = server.runner_next(link).unwrap().unwrap();
        let message = message.try_clone().unwrap();
        assert_eq!(message.method, method);
        server.runner_acknowledge(link, ticket);
        server
            .runner_message(
                link,
                p::WireMessage {
                    r#type: p::MESSAGE_RESPONSE.into(),
                    id: message.id.clone(),
                    payload: d::RawJson(Some(Value::from_json(payload.as_bytes()).unwrap())),
                    ..Default::default()
                },
                &now,
                2,
                random,
            )
            .unwrap();
        message
    };
    exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_ACCEPTS,
        r#"{"accepts":true}"#,
    );
    exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_MATERIALIZE,
        r#"{"driver":"docker","container_id":"probe-container","status":"ready"}"#,
    );
    server.maintain_agents(&now, &mut random).unwrap();
    let stream = exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_START_ENABLED,
        "null",
    )
    .id;
    for (method, result) in [
        (
            "initialize",
            r#"{"protocolVersion":1,"agentInfo":{"name":"probe-agent"}}"#,
        ),
        (
            "session/new",
            r#"{"sessionId":"probe-session","models":{"currentModelId":"model-a","availableModels":[{"modelId":"model-a","name":"Model A"}]}}"#,
        ),
    ] {
        server.maintain_agents(&now, &mut random).unwrap();
        let (ticket, message) = server.runner_next(&link).unwrap().unwrap();
        assert_eq!(message.r#type, p::MESSAGE_STREAM_INPUT);
        let json = Value::from_json(message.data.0.as_deref().unwrap()).unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(object.get("method").unwrap().as_str(), Some(method));
        let id = object.get("id").unwrap().as_i64().unwrap();
        server.runner_acknowledge(&link, ticket);
        server
            .runner_message(
                &mut link,
                p::WireMessage {
                    r#type: p::MESSAGE_STREAM_DATA.into(),
                    id: stream.clone(),
                    data: d::Bytes(Some(
                        alloc::format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}\n")
                            .into_bytes(),
                    )),
                    ..Default::default()
                },
                &now,
                3,
                &mut random,
            )
            .unwrap();
    }
    server.maintain_agents(&now, &mut random).unwrap();
    let options = server
        .store
        .artifact("layer")
        .unwrap()
        .agent_options
        .as_ref()
        .unwrap();
    assert!(!options.fetching);
    assert_eq!(options.agent_name, "probe-agent");
    assert_eq!(options.models.len(), 1);
    server.maintain_agents(&now, &mut random).unwrap();
    let (ticket, close) = server.runner_next(&link).unwrap().unwrap();
    assert_eq!(close.r#type, p::MESSAGE_STREAM_CLOSE);
    server.runner_acknowledge(&link, ticket);
    exchange(
        &mut server,
        &mut link,
        &mut random,
        p::METHOD_CAPSULE_CHANGES,
        "{}",
    );
    assert!(!server.options.is_empty());
    exchange(&mut server, &mut link, &mut random, p::METHOD_STOP, "null");
    server.maintain_agents(&now, &mut random).unwrap();
    assert!(server.options.is_empty());
    assert_eq!(
        server
            .store
            .composition(&composition)
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .status,
        "stopped"
    );
}

#[test]
fn artifact_deletion_waits_for_capsules_and_records_durable_blob_cleanup() {
    use d::protocol as p;
    let state=PersistedState::from_json(br#"{
      "artifacts":{"a":{"id":"a","kind":"tool","name":"base","created_by":"derek","snapshot":{"driver":"docker","digest":"base","ref":"spin/base","client_id":"client"}},"b":{"id":"b","kind":"credential","name":"child","created_by":"colleague","parent_artifact_ids":["a"],"snapshot":{"driver":"docker","digest":"child","ref":"spin/child","client_id":"client"}}},
      "compositions":{"c":{"id":"c","operator":"colleague","layers":["a","b"],"runtime":{"status":"ready","client_id":"client","container_id":"container"}}},
      "recordings":{"r":{"id":"r","actor":"colleague","status":"recording","parent_artifact_ids":["b"],"runtime":{"status":"ready","client_id":"client","container_id":"recording"}}}
    }"#).unwrap();
    let fail = Cell::new(false);
    let now = time();
    let mut random = Random(16000);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    let (generation, _) = peer.attach("runner", None, 0).unwrap();
    server.runners.push(peer);
    let request = req("DELETE", "/api/artifacts/a", &[], b"");
    assert!(
        server
            .artifact_operation_route(
                &request,
                &d::User {
                    username: "stranger".into(),
                    ..Default::default()
                },
                &now,
                &mut random
            )
            .is_err()
    );
    assert!(
        !server
            .store
            .composition("c")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .stop_pending
    );
    let Some(Outcome::Operation(wait)) = server
        .artifact_operation_route(
            &request,
            &d::User {
                username: "derek".into(),
                ..Default::default()
            },
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!("deletion");
    };
    let answer = |server: &mut Server<_>, random: &mut Random, method: &str, payload: &str| {
        let (ticket, request) = server.runners[0].next(generation).unwrap().unwrap();
        let request = request.try_clone().unwrap();
        assert_eq!(request.method, method);
        server.runners[0].acknowledge(ticket);
        server.runners[0].response(&request.id);
        server
            .finish_capsule(
                "client",
                Some(&request),
                &p::WireMessage {
                    id: request.id.clone(),
                    payload: d::RawJson(Some(Value::from_json(payload.as_bytes()).unwrap())),
                    ..Default::default()
                },
                &now,
                random,
            )
            .unwrap();
        request
    };
    server.maintain_operations(&now, &mut random).unwrap();
    answer(&mut server, &mut random, p::METHOD_CAPSULE_CHANGES, "{}");
    assert!(server.store.artifact("a").is_ok());
    answer(&mut server, &mut random, p::METHOD_STOP, "null");
    server.maintain_operations(&now, &mut random).unwrap();
    answer(&mut server, &mut random, p::METHOD_CANCEL_RECORDING, "null");
    for image in ["spin/child", "spin/base"] {
        server.maintain_operations(&now, &mut random).unwrap();
        let request = answer(&mut server, &mut random, p::METHOD_REMOVE_SNAPSHOT, "null");
        assert_eq!(
            p::SnapshotPayload::from_value(request.payload.0.as_ref().unwrap())
                .unwrap()
                .snapshot
                .r#ref,
            image
        );
        assert!(server.store.artifact("a").is_ok());
    }
    server.maintain_operations(&now, &mut random).unwrap();
    assert_eq!(server.poll_operation(&wait).unwrap().unwrap().status, 200);
    assert!(server.store.artifact("a").is_err());
    assert!(server.store.artifact("b").is_err());
    assert!(server.store.composition("c").is_err());
    // This test persistence rejects blob I/O: graph deletion still has a successful response.
    assert!(server.store.collect_blob_garbage().is_err());
}

#[test]
fn live_attachments_wait_for_ack_and_reconcile_after_capsule_replacement() {
    use d::protocol as p;
    use spin_store::{BlobInfo, BlobReply as R, BlobRequest as Q};
    struct Bytes;
    const FILE: &[u8] = b"%PDF-1.7 native fixture";
    impl Persistence for Bytes {
        fn save(&mut self, _: &PersistedState) -> spin_store::Result {
            Ok(())
        }
        fn blob(&mut self, request: Q<'_>) -> spin_store::Result<R> {
            let info = || BlobInfo {
                reference: "attachment:att".into(),
                digest: "digest".into(),
                kind: "job-attachment".into(),
                size: FILE.len() as i64,
            };
            match request {
                Q::Info("attachment:att") => Ok(R::Info(info())),
                Q::Chunk {
                    reference: "attachment:att",
                    offset: 0,
                } => Ok(R::Chunk(FILE.to_vec(), info())),
                _ => Err(spin_store::Error::NotFound),
            }
        }
    }
    let mut state = PersistedState::from_json(br#"{
        "jobs":{"j":{"id":"j","owner":"derek"}},
        "sessions":{"s":{"id":"s","job_id":"j","operator":"derek","prepared_composition_id":"c"}},
        "compositions":{"c":{"id":"c","session_id":"s","operator":"derek","runtime":{"status":"ready","client_id":"client","container_id":"container"}}},
        "job_attachments":{"att":{"id":"att","job_id":"j","capsule_path":"/spin/job-attachments/att-proof.pdf"}}
    }"#).unwrap();
    let attachment = state.job_attachments.get_mut("att").unwrap();
    attachment.size = FILE.len() as i64;
    attachment.sha256 = spin_security::digest_hex(FILE).unwrap();
    let now = time();
    let mut random = Random(32000);
    let mut server = Server::new(Store::new(state, Bytes));
    let mut peer = spin_core::runner::Peer::new(d::Client {
        id: "client".into(),
        ..Default::default()
    });
    let (generation, _) = peer
        .attach("runner", None, now.time().unwrap().0 / 1_000_000)
        .unwrap();
    server.runners.push(peer);
    server.maintain_attachments(&now, &mut random).unwrap();
    assert!(server.attachment_stamps.is_empty());
    let (ticket, request) = server.runners[0].next(generation).unwrap().unwrap();
    let request = request.try_clone().unwrap();
    assert_eq!(request.method, p::METHOD_INJECT_ATTACHMENTS);
    let payload =
        p::InjectAttachmentsPayload::from_value(request.payload.0.as_ref().unwrap()).unwrap();
    assert_eq!(payload.attachments[0].data.0.as_ref().unwrap(), FILE);
    assert_eq!(
        payload.attachments[0].target_path,
        "/spin/job-attachments/att-proof.pdf"
    );
    server.runners[0].acknowledge(ticket);
    server.runners[0].response(&request.id);
    server
        .finish_capsule(
            "client",
            Some(&request),
            &p::WireMessage {
                id: request.id.clone(),
                ..Default::default()
            },
            &now,
            &mut random,
        )
        .unwrap();
    server.maintain_attachments(&now, &mut random).unwrap();
    assert!(server.runners[0].next(generation).unwrap().is_none());
    let mut capsule = server
        .store
        .composition("c")
        .unwrap()
        .runtime
        .as_ref()
        .unwrap()
        .try_clone()
        .unwrap();
    capsule.container_id = "replacement".into();
    server
        .store
        .set_composition_runtime("c", "derek", capsule)
        .unwrap();
    server.maintain_attachments(&now, &mut random).unwrap();
    assert_eq!(
        server.runners[0]
            .next(generation)
            .unwrap()
            .unwrap()
            .1
            .method,
        p::METHOD_INJECT_ATTACHMENTS
    );
}

#[test]
fn manual_sessions_only_start_when_requested_and_repeated_start_is_one_operation() {
    let state = PersistedState::from_json(br#"{
        "artifacts":{"env":{"id":"env","kind":"tool","name":"agent","scope":"global","profile":"default","enables":[{"name":"acp"},{"name":"git"}]}},
        "git_repositories":{"repo":{"id":"repo","remote_url":"https://example.test/repo.git","default_ref":"main","credential_scope":"public"}},
        "jobs":{"j":{"id":"j","owner":"derek","status":"active","git_repository_id":"repo","branch":"jobs/j/main"}}
    }"#).unwrap();
    let fail = Cell::new(false);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let now = time();
    let mut random = Random(33000);
    let body = br#"{"environment_selector":"tool:agent","objective_delta":"Inspect the logs","operator":"untrusted","run":false}"#;
    let Some(Outcome::Response(response)) = server
        .capsule_route(
            &req("POST", "/api/jobs/j/sessions", &[], body),
            "derek",
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(response.status, 201);
    let created = d::CreateJobSessionResponse::from_json(&response.body).unwrap();
    assert_eq!(created.session.operator, "derek");
    assert!(created.composition.is_none());
    server.maintain_capsules(&now, &mut random).unwrap();
    assert!(
        server.calls.is_empty(),
        "run=false must not become an automatic workflow launch"
    );
    let path =
        spin_core::validation::text(format_args!("/api/sessions/{}/capsule", created.session.id))
            .unwrap();
    for _ in 0..2 {
        let Some(Outcome::Response(response)) = server
            .capsule_route(&req("POST", &path, &[], b""), "derek", &now, &mut random)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(response.status, 202);
        assert_eq!(server.calls.len(), 1);
    }
    let stopping = spin_core::validation::text(format_args!("{path}/stop")).unwrap();
    assert!(matches!(
        server.capsule_route(
            &req("POST", &stopping, &[], b""),
            "derek",
            &now,
            &mut random
        ),
        Err(Error::Http(409, _))
    ));
    let body = br#"{"environment_selector":"tool:agent","objective_delta":"Run another investigation","run":true}"#;
    let Some(Outcome::Capsule(wait)) = server
        .capsule_route(
            &req("POST", "/api/jobs/j/sessions", &[], body),
            "derek",
            &now,
            &mut random,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(server.store.snapshot().unwrap().sessions.len(), 2);
    assert_eq!(server.calls.len(), 2);
    assert!(server.poll_capsule(&wait, &now).unwrap().is_none());
}

#[test]
fn storage_reports_replica_lag_and_pruning_waits_for_object_deletion() {
    use alloc::{borrow::ToOwned, vec::Vec};
    use core::cell::RefCell;
    use spin_store::{BlobReply as R, BlobRequest as Q, StorageUsage};
    struct Storage<'a> {
        mode: &'a Cell<u8>,
        fail: &'a Cell<bool>,
        deleted: &'a RefCell<Vec<String>>,
    }
    impl Persistence for Storage<'_> {
        fn save(&mut self, _: &PersistedState) -> spin_store::Result {
            Ok(())
        }
        fn storage_usage(&mut self) -> spin_store::Result<Option<StorageUsage>> {
            let replication = match self.mode.get() {
                0 => Value::Null,
                1 => Value::from_json(br#"{"complete":false}"#)?,
                _ => {
                    Value::from_json(br#"{"complete":true,"last_sync_at":"2026-09-30T12:00:00Z"}"#)?
                }
            };
            Ok(Some(StorageUsage {
                database_bytes: 4096,
                object_bytes: 100,
                objects: 1,
                replication,
            }))
        }
        fn blob(&mut self, request: Q<'_>) -> spin_store::Result<R> {
            let Q::Delete(reference) = request else {
                return Err(spin_store::Error::NotFound);
            };
            if self.fail.get() {
                return Err(spin_store::Error::Storage(10));
            }
            self.deleted.borrow_mut().push(reference.into());
            Ok(R::Done)
        }
    }
    let state = PersistedState::from_json(br#"{"artifacts":{
        "old":{"id":"old","kind":"tool","name":"agent","superseded_by":"new","snapshot":{"digest":"sha256:old","ref":"old-image"}},
        "new":{"id":"new","kind":"tool","name":"agent","snapshot":{"digest":"sha256:new","ref":"new-image"}}
    }}"#).unwrap();
    let mode = Cell::new(0);
    let fail = Cell::new(true);
    let deleted = RefCell::new(Vec::new());
    let mut server = Server::new(Store::new(
        state,
        Storage {
            mode: &mode,
            fail: &fail,
            deleted: &deleted,
        },
    ));
    let now = time();
    let level = |server: &Server<_>| {
        server
            .storage_report
            .as_object()
            .unwrap()
            .get("health")
            .unwrap()
            .as_object()
            .unwrap()
            .get("level")
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    };
    server.refresh_storage(&now).unwrap();
    assert_eq!(level(&server), "warning");
    mode.set(1);
    server.refresh_storage(&now).unwrap();
    assert_eq!(level(&server), "error");
    mode.set(2);
    server.refresh_storage(&now).unwrap();
    assert_eq!(level(&server), "ok");
    server
        .refresh_storage(&Timestamp::from_json(br#""2026-09-30T12:05:01Z""#).unwrap())
        .unwrap();
    assert_eq!(level(&server), "error");
    assert!(server.prune_snapshot(&now).is_err());
    assert!(
        server
            .store
            .artifact("old")
            .unwrap()
            .snapshot_pruned_at
            .is_none()
    );
    fail.set(false);
    server.prune_snapshot(&now).unwrap();
    assert!(
        server
            .store
            .artifact("old")
            .unwrap()
            .snapshot_pruned_at
            .is_some()
    );
    assert_eq!(
        deleted.borrow().as_slice(),
        [String::from("snapshot:sha256:old")]
    );
    server.prune_snapshot(&now).unwrap();
    assert_eq!(deleted.borrow().len(), 1);
}

#[test]
fn recording_parent_change_survives_restart_and_waits_for_old_container_removal() {
    use core::cell::RefCell;
    use d::protocol as p;
    struct Saved<'a>(&'a RefCell<PersistedState>);
    impl Persistence for Saved<'_> {
        fn save(&mut self, state: &PersistedState) -> spin_store::Result {
            *self.0.borrow_mut() = state.try_clone()?;
            Ok(())
        }
    }
    let state = PersistedState::from_json(br#"{
        "artifacts":{"parent":{"id":"parent","kind":"tool","name":"base","profile":"default","scope":"global","snapshot":{"ref":"base-image"}}},
        "recordings":{"r":{"id":"r","actor":"derek","kind":"tool","name":"child","scope":"global","status":"recording","runtime":{"client_id":"client","container_id":"old","status":"recording"}}}
    }"#).unwrap();
    let saved = RefCell::new(state.try_clone().unwrap());
    let now = time();
    let mut random = Random(34000);
    let mut server = Server::new(Store::new(state, Saved(&saved)));
    let attach = |server: &mut Server<_>| {
        let mut peer = spin_core::runner::Peer::new(d::Client {
            id: "client".into(),
            ..Default::default()
        });
        let (generation, _) = peer
            .attach("runner", None, now.time().unwrap().0 / 1_000_000)
            .unwrap();
        server.runners.push(peer);
        generation
    };
    let generation = attach(&mut server);
    assert!(matches!(
        server
            .capsule_route(
                &req(
                    "POST",
                    "/api/recordings/r/parents",
                    &[],
                    br#"{"kind":"tool","name":"base"}"#
                ),
                "derek",
                &now,
                &mut random
            )
            .unwrap(),
        Some(Outcome::Capsule(_))
    ));
    assert_eq!(
        server.runners[0]
            .next(generation)
            .unwrap()
            .unwrap()
            .1
            .method,
        p::METHOD_CANCEL_RECORDING
    );
    drop(server);
    let recovered = saved.borrow().try_clone().unwrap();
    let mut server = Server::new(Store::new(recovered, Saved(&saved)));
    let generation = attach(&mut server);
    assert!(matches!(
        server.capsule_route(
            &req(
                "POST",
                "/api/recordings/r/commands",
                &[],
                br#"{"input":"touch /unsafe"}"#
            ),
            "derek",
            &now,
            &mut random
        ),
        Err(Error::Http(409, _))
    ));
    server.maintain_capsules(&now, &mut random).unwrap();
    for (method, result) in [
        (p::METHOD_CANCEL_RECORDING, "null"),
        (
            p::METHOD_START_RECORDING,
            r#"{"client_id":"client","container_id":"new","status":"recording"}"#,
        ),
    ] {
        let (ticket, request) = server.runners[0].next(generation).unwrap().unwrap();
        let request = request.try_clone().unwrap();
        assert_eq!(request.method, method);
        if method == p::METHOD_START_RECORDING {
            let payload =
                p::StartRecordingPayload::from_value(request.payload.0.as_ref().unwrap()).unwrap();
            assert!(payload.recording.runtime.is_none());
            assert_eq!(payload.parents[0].id, "parent");
        }
        server.runners[0].acknowledge(ticket);
        server.runners[0].response(&request.id);
        server
            .finish_capsule(
                "client",
                Some(&request),
                &p::WireMessage {
                    id: request.id.clone(),
                    payload: d::RawJson(Some(Value::from_json(result.as_bytes()).unwrap())),
                    ..Default::default()
                },
                &now,
                &mut random,
            )
            .unwrap();
    }
    let recording = server.store.recording("r").unwrap();
    assert_eq!(
        recording.parent_artifact_ids.as_slice(),
        [String::from("parent")]
    );
    assert_eq!(recording.runtime.as_ref().unwrap().container_id, "new");
    assert!(!recording.runtime.as_ref().unwrap().stop_pending);
}

#[test]
fn restart_fences_interrupted_login_writes_and_temporary_agent_probes() {
    let state = PersistedState::from_json(br#"{
        "artifacts":{"a":{"id":"a","kind":"credential","name":"agent","tracked_paths":["/root/token"],"agent_options":{"fetching":true}}},
        "logins":{"login":{"id":"login","key":"/credential:agent","files":{"/root/token":"bmV3"}}},
        "compositions":{
            "c":{"id":"c","operator":"derek","layers":["a"],"logins":{"/credential:agent":"login"},"runtime":{"status":"installing_login","client_id":"offline","container_id":"c"}},
            "probe":{"id":"probe","probe_artifact_id":"a","operator":"derek","runtime":{"status":"ready","client_id":"offline","container_id":"probe"}}
        }
    }"#).unwrap();
    let fail = Cell::new(false);
    let now = time();
    let mut random = Random(35000);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    server.recover(&now).unwrap();
    let options = server
        .store
        .artifact("a")
        .unwrap()
        .agent_options
        .as_ref()
        .unwrap();
    assert!(!options.fetching);
    assert!(!options.error.is_empty());
    assert!(
        server
            .store
            .composition("probe")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .stop_pending
    );
    server.retry_pending_stops(&now, &mut random).unwrap();
    assert!(
        server
            .store
            .composition("c")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .stop_pending
    );
    assert!(
        server.store.delete_login("login").is_err(),
        "uncertain credentials stay reserved until stop ACK"
    );
    assert_eq!(
        server
            .store
            .login("login")
            .unwrap()
            .files
            .get("/root/token")
            .unwrap()
            .0
            .as_deref(),
        Some(b"new".as_slice())
    );
}

#[test]
fn accepted_step_releases_its_capsule_while_the_current_step_keeps_its_own() {
    let state = PersistedState::from_json(br#"{
      "jobs":{"job":{"id":"job","owner":"derek","status":"active","workflow_status":"busy","current_phase_run_id":"run2","branch":"jobs/#1/main","template_snapshot":{"id":"tpl","phases":[{"id":"develop","name":"Bouw","allow_changes":true,"accept":{"target":"review"},"reject":{"target":"SELF"}},{"id":"review","name":"Review","accept":{"target":"DONE"},"reject":{"target":"develop"}}]}}},
      "sessions":{
        "ses":{"id":"ses","job_id":"job","phase_run_id":"run","operator":"derek","status":"completed","prepared_composition_id":"cmp","git_ref":"jobs/#1/sessions/one"},
        "ses2":{"id":"ses2","job_id":"job","phase_run_id":"run2","operator":"derek","status":"running","prepared_composition_id":"cmp2","git_ref":"jobs/#1/sessions/two"}},
      "phase_runs":{
        "run":{"id":"run","session_id":"ses","job_id":"job","phase_id":"develop","phase_name":"Bouw","status":"accepted","attempt":1},
        "run2":{"id":"run2","session_id":"ses2","job_id":"job","phase_id":"review","phase_name":"Review","status":"running","attempt":1}},
      "compositions":{
        "cmp":{"id":"cmp","operator":"derek","session_id":"ses","runtime":{"driver":"docker","client_id":"client","container_id":"old","status":"ready"}},
        "cmp2":{"id":"cmp2","operator":"derek","session_id":"ses2","runtime":{"driver":"docker","client_id":"client","container_id":"new","status":"ready"}}}
    }"#).unwrap();
    let fail = Cell::new(false);
    let mut server = Server::new(Store::new(state, Memory(&fail)));
    let now = time();
    let mut random = Random(60000);
    server.sweep_idle_capsules(&now, &mut random).unwrap();
    let stopping = |id: &str| {
        server
            .store
            .composition(id)
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .stop_pending
    };
    assert!(stopping("cmp"), "the accepted step's capsule holds a login; it must stop");
    assert!(!stopping("cmp2"), "the current step keeps its capsule");
}
