use super::*;
use alloc::collections::VecDeque;
use d::{
    Wire,
    engine::{GitAuthentication, TrackedSelection},
};

#[derive(Default)]
struct Fake {
    commands: Vec<Command>,
    replies: VecDeque<Output>,
}
impl Fake {
    fn reply(&mut self, code: i32, output: &[u8]) {
        self.replies.push_back(Output {
            code,
            bytes: output.to_vec(),
        });
    }
}
impl Executor for Fake {
    async fn run(&mut self, command: Command) -> Result<Output> {
        command.validate().unwrap();
        self.commands.push(command);
        Ok(self
            .replies
            .pop_front()
            .expect("unexpected process invocation"))
    }
}
fn run<F: core::future::Future>(future: F) -> F::Output {
    let mut future = core::pin::pin!(future);
    match future.as_mut().poll(&mut core::task::Context::from_waker(
        core::task::Waker::noop(),
    )) {
        core::task::Poll::Ready(value) => value,
        core::task::Poll::Pending => panic!("fake executor must complete synchronously"),
    }
}
fn docker() -> Docker {
    Docker::new("/fake/docker", "", "").unwrap()
}
fn runtime() -> d::CapsuleRuntime {
    d::CapsuleRuntime::from_json(br#"{"driver":"docker","container_id":"c-1","status":"ready"}"#)
        .unwrap()
}
#[test]
fn git_publication_keeps_credentials_off_argv_and_failed_processes_owned() {
    let docker = docker();
    let mut fake = Fake::default();
    let mut request = d::engine::WorkspaceAcceptance {
        remote_ref: "jobs/#12/main".into(),
        base_branch: "main".into(),
        commit_subject: "workflow: accepted".into(),
        allow_changes: true,
        authentication: Some(GitAuthentication {
            password: "private-token".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    fake.reply(
        0,
        b"pushed\nSPIN_ACCEPT committed=1 head=0123456789abcdef\n",
    );
    let result = run(docker.accept_workspace(&mut fake, &runtime(), &request)).unwrap();
    assert!(result.committed);
    assert_eq!(result.head, "0123456789abcdef");
    assert!(!docker.needs_cleanup());
    let command = &fake.commands[0];
    assert!(
        command
            .args
            .iter()
            .all(|arg| !arg.contains("private-token"))
    );
    assert!(
        core::str::from_utf8(&command.input)
            .unwrap()
            .contains("private-token")
    );
    assert!(
        command
            .args
            .iter()
            .any(|arg| arg == "SPIN_GIT_REF=jobs/#12/main")
    );
    request.remote_ref = "main; echo injection".into();
    assert!(run(docker.accept_workspace(&mut fake, &runtime(), &request)).is_err());
    assert_eq!(fake.commands.len(), 1);
    fake.reply(43, b"Job branch advanced");
    let request = d::engine::WorkspaceSync {
        session_ref: "jobs/#12/sessions/one".into(),
        ..Default::default()
    };
    assert!(run(docker.sync_workspace(&mut fake, &runtime(), &request)).is_err());
    assert!(docker.needs_cleanup());
    // Dezelfde workspace mag geen tweede schrijver krijgen voordat cleanup klaar is.
    assert!(run(docker.sync_workspace(&mut fake, &runtime(), &request)).is_err());
    assert_eq!(fake.commands.len(), 2);
    fake.reply(0, b"");
    run(docker.clean_one(&mut fake)).unwrap();
    assert!(!docker.needs_cleanup());
}
#[test]
fn start_reuses_capsule_and_daemon_failure_does_not_create_another() {
    let recording = d::Recording::from_json(br#"{"id":"rec-1"}"#).unwrap();
    let mut fake = Fake::default();
    fake.reply(1, b"Error: No such container: spin-rec-rec-1");
    fake.reply(0, b"container-one\n");
    fake.reply(0, b"container-one\n");
    let first = run(docker().start_recording(&mut fake, &recording, &[])).unwrap();
    assert_eq!(first.container_id, "container-one");
    assert!(
        fake.commands[1]
            .args
            .windows(2)
            .any(|a| a == ["--label", "spin.recording_id=rec-1"])
    );
    fake.reply(0, b"container-one\n");
    fake.reply(0, b"container-one\n");
    let second = run(docker().start_recording(&mut fake, &recording, &[])).unwrap();
    assert_eq!(first, second);
    fake.reply(1, b"Cannot connect to the Docker daemon");
    assert!(run(docker().start_recording(&mut fake, &recording, &[])).is_err());
    assert_eq!(
        fake.commands.iter().filter(|c| c.args[0] == "run").count(),
        1
    );
}
#[test]
fn execution_keeps_exit_status_and_replaces_invalid_utf8() {
    let recording = d::Recording {
        runtime: Some(runtime()),
        ..Default::default()
    };
    let mut fake = Fake::default();
    fake.reply(7, b"  hello \xff\n");
    let execution = run(docker().execute(&mut fake, &recording, "echo x; exit 7")).unwrap();
    assert_eq!(execution.exit_code, 7);
    assert_eq!(execution.output, "hello \u{fffd}");
    assert_eq!(fake.commands[0].args.last().unwrap(), "echo x; exit 7");
    assert!(fake.commands[0].merge_stderr);
}
#[test]
fn labels_keep_empty_last_field_and_cleanup_is_idempotent() {
    let mut fake = Fake::default();
    fake.reply(
        0,
        b"composition\tcmp-1\t\nrecording\t\trec-1\ncomposition-build\tbuild-1\t\n",
    );
    let live = run(docker().live_capsules(&mut fake)).unwrap();
    assert_eq!(&*live.compositions, &["cmp-1"]);
    assert_eq!(&*live.recordings, &["rec-1"]);
    fake.reply(0, b"container-one\n");
    fake.reply(1, b"No such container: container-one");
    fake.reply(0, b"container-two\n");
    fake.reply(0, b"container-two\n");
    assert_eq!(
        run(docker().remove_capsules(&mut fake, &live.compositions, &live.recordings)).unwrap(),
        2
    );
    for command in fake.commands.iter().filter(|c| c.args[0] == "ps") {
        assert!(command.args.iter().any(|a| a == "label=spin.managed=true"));
    }
}
#[test]
fn tracked_files_distinguish_skipped_empty_binary_and_reject_bad_paths_before_spawn() {
    let mut fake = Fake::default();
    fake.reply(0, b"SPIN_SKIP /root/skipped\nSPIN_FILE /root/empty \nSPIN_FILE /root/binary AP8=\nSPIN_SKIP /root/binary\n");
    let selection =
        TrackedSelection::from_json(br#"{"paths":["/root/"],"excludes":["/root/cache/"]}"#)
            .unwrap();
    let files = run(docker().read_tracked_files(&mut fake, &runtime(), &selection)).unwrap();
    assert_eq!(files.get("/root/skipped").unwrap().0, None);
    assert_eq!(
        files.get("/root/empty").unwrap().0.as_deref(),
        Some(b"".as_slice())
    );
    assert_eq!(
        files.get("/root/binary").unwrap().0.as_deref(),
        Some(b"\0\xff".as_slice())
    );
    fake.reply(0, b"");
    run(docker().write_tracked_files(&mut fake, &runtime(), &files)).unwrap();
    assert!(contains(&fake.commands[1].input, b"/root/binary AP8=\n"));
    assert!(!fake.commands[1].args.iter().any(|a| a.contains("AP8=")));
    let invalid = TrackedSelection::from_json(br#"{"paths":["/root/../../tmp/"]}"#).unwrap();
    assert!(run(docker().read_tracked_files(&mut fake, &runtime(), &invalid)).is_err());
    assert_eq!(fake.commands.len(), 2);
}
#[test]
fn checkout_credentials_only_cross_stdin_and_reference_checkout_is_read_only() {
    let composition = d::Composition::from_json(br#"{"session_id":"ses-1","workspaces":[{"path":"docs","mode":"reference","remote_url":"https://example.test/docs.git","base_ref":"main","credential_scope":"user"}]}"#).unwrap();
    let snapshot = d::CapsuleSnapshot::from_json(
        br#"{"driver":"docker","ref":"spin/artifact:git","restorable":true}"#,
    )
    .unwrap();
    let auth = GitAuthentication::from_json(br#"{"Username":"alice","Password":"secret-token","AuthorName":"Alice","AuthorEmail":"alice@example.test"}"#).unwrap();
    let mut fake = Fake::default();
    fake.reply(0, b"spin-work-ses-1\n");
    fake.reply(0, b"");
    assert_eq!(
        run(docker().prepare_git_workspaces(&mut fake, &composition, &snapshot, Some(&auth)))
            .unwrap(),
        "spin-work-ses-1"
    );
    let command = &fake.commands[1];
    assert!(contains(&command.input, b"secret-token\n"));
    assert!(!command.args.iter().any(|a| a.contains("secret-token")));
    assert!(
        !command
            .environment
            .iter()
            .any(|(_, value)| value.contains("secret-token"))
    );
    let script = command.args.last().unwrap();
    assert!(script.contains("git remote set-url --push origin no_push"));
    assert!(script.contains("GIT_CONFIG_VALUE_0="));
    assert!(!script.contains("gitCredentialEnvironmentScript"));
}
#[test]
fn watcher_uses_literal_argv_and_process_fields_cannot_bypass_budgets() {
    let selection = TrackedSelection::from_json(br#"{"paths":["/root/$(id)/file"]}"#).unwrap();
    let command = docker()
        .watch_tracked_files(&runtime(), &selection)
        .unwrap()
        .unwrap();
    assert_eq!(command.args.last().unwrap(), "/root/$(id)");
    assert!(!command.args[6].contains("$(id)"));
    let mut command = Command::new("sh").unwrap();
    command.args.push("\0".into());
    assert!(command.validate().is_err());
    assert_eq!(
        runtime_name("spin-rec", "ÄBC//Name").unwrap(),
        "spin-rec--bc--name"
    );
}
#[test]
fn cancelled_build_keeps_cleanup_owned_until_docker_confirms_removal() {
    let docker = docker();
    let lease = docker.track_cleanup("spin-compose-build-test").unwrap();
    assert!(!docker.needs_cleanup());
    drop(lease);
    assert!(docker.needs_cleanup());
    assert!(docker.track_cleanup("spin-compose-build-test").is_err());
    let mut fake = Fake::default();
    fake.reply(1, b"daemon unavailable");
    assert!(run(docker.clean_one(&mut fake)).is_err());
    assert!(docker.needs_cleanup());
    fake.reply(0, b"spin-compose-build-test\n");
    run(docker.clean_one(&mut fake)).unwrap();
    assert!(!docker.needs_cleanup());
    docker
        .track_cleanup("spin-compose-build-test")
        .unwrap()
        .complete();
    assert!(!docker.needs_cleanup());
}

#[test]
fn enabled_environment_stdio_and_container_process_ownership() {
    let docker = docker();
    let enabled =
        d::Enablement::from_json(br#"{"name":"acp","transport":"stdio","command":"codex-acp"}"#)
            .unwrap();
    let command = docker
        .enabled_command(&runtime(), &enabled, "/tmp/spin-enabled-test.pid")
        .unwrap();
    assert!(!command.merge_stderr);
    let script = command.args.last().unwrap();
    assert!(script.contains(". /etc/spin/enabled/acp.env"));
    assert!(script.contains("export SHELL="));
    assert!(script.ends_with("echo $$ > /tmp/spin-enabled-test.pid; exec codex-acp"));
    assert!(
        docker
            .enabled_command(&runtime(), &enabled, "/tmp/$(id)")
            .is_err()
    );
    let invalid =
        d::Enablement::from_json(br#"{"name":"../acp","transport":"stdio","command":"codex-acp"}"#)
            .unwrap();
    assert!(
        docker
            .enabled_command(&runtime(), &invalid, "/tmp/spin-enabled-test.pid")
            .is_err()
    );
    let lease = docker
        .track_process_cleanup("container\0acp", "container", "/tmp/spin-enabled-test.pid")
        .unwrap();
    assert!(docker.cleanup_pending("container\0acp"));
    drop(lease);
    let mut fake = Fake::default();
    fake.reply(1, b"daemon unavailable");
    assert!(run(docker.clean_one(&mut fake)).is_err());
    assert!(docker.cleanup_pending("container\0acp"));
    fake.reply(0, b"");
    run(docker.clean_one(&mut fake)).unwrap();
    assert!(!docker.cleanup_pending("container\0acp"));
    assert_eq!(&fake.commands[0].args[..2], &["exec", "container"]);
    assert!(
        fake.commands[0]
            .args
            .last()
            .unwrap()
            .contains("kill \"$enabled_pid\"")
    );
}

#[test]
fn browse_preserves_file_bytes_and_rejects_traversal_before_spawning() {
    use d::engine::RepositoryBrowse;
    let docker = docker();
    let mut fake = Fake::default();
    let mut request = RepositoryBrowse {
        remote_url: "https://example.test/repo.git".into(),
        cache_key: "repo".into(),
        mode: "file".into(),
        r#ref: "main".into(),
        path: "docs/a b.txt".into(),
        authentication: Some(GitAuthentication {
            password: "secret".into(),
            ..Default::default()
        }),
    };
    fake.reply(0, b"warning\nSPIN_SIZE 4\n hi\n");
    let file = run(docker.browse_repository(&mut fake, &request))
        .unwrap()
        .file
        .unwrap();
    assert_eq!(file.content, " hi\n");
    assert!(!file.truncated);
    assert!(
        !fake.commands[0]
            .args
            .iter()
            .any(|arg| arg.contains("secret"))
    );
    assert!(
        core::str::from_utf8(&fake.commands[0].input)
            .unwrap()
            .contains("secret")
    );
    request.path = "../token".into();
    assert!(run(docker.browse_repository(&mut fake, &request)).is_err());
    assert_eq!(fake.commands.len(), 1);
    fake.reply(0, b"");
    run(docker.clean_one(&mut fake)).unwrap();
    request.path = "binary".into();
    fake.reply(0, b"SPIN_SIZE 8\na\0b");
    let file = run(docker.browse_repository(&mut fake, &request))
        .unwrap()
        .file
        .unwrap();
    assert!(file.binary && file.truncated && file.content.is_empty());
}

#[test]
fn workspace_inspection_rejects_escape_before_any_process() {
    let docker = docker();
    let mut fake = Fake::default();
    for path in ["../other", "/root", ".", "x/y", "space here"] {
        assert!(run(docker.inspect_workspace(&mut fake, &runtime(), path)).is_err());
    }
    assert!(fake.commands.is_empty());
}
