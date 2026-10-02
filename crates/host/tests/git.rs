//! De Rust-commandobouwer en resultaatparser tegen echte Git-repositories, zonder Docker-daemon.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use spin_core::docker::{Docker, Executor, Output};
use spin_domain::{
    self as d,
    engine::{WorkspaceAcceptance, WorkspaceSync},
};
use spin_host::storage::Random;
use spin_store::IdSource;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn block_on<F: core::future::Future>(future: F) -> F::Output {
    let mut future = core::pin::pin!(future);
    match future.as_mut().poll(&mut core::task::Context::from_waker(
        core::task::Waker::noop(),
    )) {
        core::task::Poll::Ready(result) => result,
        core::task::Poll::Pending => panic!("local test executor must complete synchronously"),
    }
}
fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}
struct LocalWorkspace<'a>(&'a Path);
impl Executor for LocalWorkspace<'_> {
    async fn run(
        &mut self,
        request: spin_core::process::Command,
    ) -> spin_core::docker::Result<Output> {
        request.validate()?;
        assert_eq!(request.args[0], "exec");
        let at = request
            .args
            .iter()
            .position(|s| matches!(s.as_str(), "sh" | "git" | "wc"))
            .unwrap();
        let mut command = Command::new(&request.args[at]);
        if request.args[at] == "sh" {
            command.arg("-c").arg(request.args.last().unwrap());
        } else {
            command.args(&request.args[at + 1..]);
        }
        command
            .current_dir(self.0)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for pair in request.args.windows(2).filter(|pair| pair[0] == "-e") {
            let (key, value) = pair[1].split_once('=').unwrap();
            command.env(key, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&request.input)
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let mut bytes = output.stdout;
        bytes.extend_from_slice(&output.stderr);
        Ok(Output {
            code: output.status.code().unwrap_or(-1),
            bytes,
        })
    }
}
fn checkout(directory: &Path, remote: &Path) {
    std::fs::create_dir_all(directory).unwrap();
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(include_str!(
            "../../core/src/docker/scripts/git-workspace.sh"
        ))
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("SPIN_GIT_REMOTE", remote)
        .env("SPIN_GIT_BOOTSTRAP", "main")
        .env("SPIN_GIT_BASE", "jobs/#1/main")
        .env("SPIN_GIT_TARGET", "jobs/#1/main")
        .env("SPIN_GIT_HEAD", "jobs/#1/sessions/one")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"\n\nTest\ntest@spin.invalid\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn sync_resumes_on_a_fresh_workspace_and_accept_folds_into_one_job_commit() {
    let id = Random::open().unwrap().next("spin-git-test").unwrap();
    let root = Directory(std::env::temp_dir().join(&id));
    std::fs::create_dir(&root.0).unwrap();
    git(&root.0, &["init", "--bare", "remote.git"]);
    git(&root.0, &["init", "seed"]);
    let seed = root.0.join("seed");
    git(
        &seed,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@spin.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    git(&seed, &["branch", "-M", "main"]);
    let remote = root.0.join("remote.git");
    git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&seed, &["push", "origin", "main"]);
    let first = root.0.join("first");
    checkout(&first, &remote);
    std::fs::write(first.join("feature.txt"), b"work\n").unwrap();
    let docker = Docker::new("docker-test", "", "").unwrap();
    let capsule = d::CapsuleRuntime {
        driver: "docker".into(),
        container_id: id,
        status: "ready".into(),
        ..Default::default()
    };
    let sync = WorkspaceSync {
        session_ref: "jobs/#1/sessions/one".into(),
        ..Default::default()
    };
    let result =
        block_on(docker.sync_workspace(&mut LocalWorkspace(&first), &capsule, &sync)).unwrap();
    assert!(result.committed && result.pushed);
    let repeat =
        block_on(docker.sync_workspace(&mut LocalWorkspace(&first), &capsule, &sync)).unwrap();
    assert!(!repeat.committed && !repeat.pushed);
    assert_eq!(repeat.head, result.head);
    let second = root.0.join("second");
    checkout(&second, &remote);
    assert_eq!(
        std::fs::read(second.join("feature.txt")).unwrap(),
        b"work\n"
    );
    std::fs::write(second.join("more.txt"), b"more\n").unwrap();
    let base = git(&second, &["config", "spin.baseCommit"]);
    let acceptance = WorkspaceAcceptance {
        allow_changes: true,
        remote_ref: "jobs/#1/main".into(),
        commit_subject: "workflow: accepted".into(),
        commit_body: "all work".into(),
        ..Default::default()
    };
    let accepted =
        block_on(docker.accept_workspace(&mut LocalWorkspace(&second), &capsule, &acceptance))
            .unwrap();
    assert!(accepted.committed);
    assert_eq!(
        git(
            &second,
            &["rev-list", "--count", &format!("{base}..{}", accepted.head)]
        ),
        "1"
    );
    let refs = git(&root.0, &["--git-dir", remote.to_str().unwrap(), "branch"]);
    assert!(!refs.contains("sessions/one"));
    assert_eq!(git(&second, &["rev-parse", "HEAD"]), accepted.head);
    assert!(!docker.needs_cleanup());
}

#[test]
fn inspect_reports_renames_binary_untracked_and_bounded_patches_from_real_git() {
    let root =
        Directory(std::env::temp_dir().join(Random::open().unwrap().next("spin-diff").unwrap()));
    std::fs::create_dir(&root.0).unwrap();
    git(&root.0, &["init", "-b", "main"]);
    std::fs::write(root.0.join("old.txt"), "original\n").unwrap();
    std::fs::write(root.0.join("tracked.txt"), "before\n").unwrap();
    git(&root.0, &["add", "."]);
    git(
        &root.0,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@spin.invalid",
            "commit",
            "-m",
            "seed",
        ],
    );
    git(&root.0, &["mv", "old.txt", "new name.txt"]);
    std::fs::write(root.0.join("tracked.txt"), "after\nmore\n").unwrap();
    std::fs::write(root.0.join("new.txt"), "one\ntwo\n").unwrap();
    std::fs::write(root.0.join("binary.dat"), b"a\0b").unwrap();
    std::fs::write(
        root.0.join("large.txt"),
        "a sufficiently long text line for the patch budget\n".repeat(20000),
    )
    .unwrap();
    let docker = Docker::new("docker", "", "").unwrap();
    let runtime = d::CapsuleRuntime {
        driver: "docker".into(),
        status: "ready".into(),
        container_id: "fixture".into(),
        ..Default::default()
    };
    let changes =
        block_on(docker.inspect_workspace(&mut LocalWorkspace(&root.0), &runtime, "")).unwrap();
    assert_eq!(changes.branch, "main");
    let file = |name: &str| changes.files.iter().find(|f| f.path == name).unwrap();
    assert_eq!(file("tracked.txt").status, " M");
    assert_eq!(
        (file("tracked.txt").added, file("tracked.txt").deleted),
        (2, 1)
    );
    assert!(file("tracked.txt").patch.contains("+more\n"));
    assert_eq!(file("new name.txt").status, "R ");
    assert_eq!(file("new.txt").added, 2);
    assert!(file("binary.dat").binary);
    assert!(file("large.txt").truncated);
    assert!(file("large.txt").patch.len() <= 512 << 10);
    assert!(changes.files.iter().map(|f| f.patch.len()).sum::<usize>() <= 2 << 20);
}
