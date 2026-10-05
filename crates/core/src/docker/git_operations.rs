//! Git-mutaties gebruiken de bestaande scripts, stdin-geheimen en expliciete procesopruiming.
use super::*;
use d::engine::*;
const ACCEPT: &str = include_str!("scripts/accept-workspace.sh");
const ACCEPT_REMOTE: &str = include_str!("scripts/accept-repository.sh");
const SYNC: &str = include_str!("scripts/sync-workspace.sh");
const MERGE: &str = include_str!("scripts/merge-workspace.sh");
const MERGE_REMOTE: &str = include_str!("scripts/merge-repository.sh");
const GIT_IMAGE: &str = "alpine/git:latest";

pub(super) fn reference(value: &str) -> Result<()> {
    if value.len() > 1024
        || !crate::validation::valid_git_base_ref(value)
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/_-.#".contains(&c))
    {
        return Err(Error::Invalid("invalid Git ref"));
    }
    Ok(())
}
pub(super) fn workspace_directory(path: &str) -> Result<String> {
    if path.len() > 200
        || (!path.is_empty()
            && (matches!(path, "." | "..")
                || path
                    .chars()
                    .any(|c| c.is_control() || " /'\"\\".contains(c))))
    {
        return Err(Error::Invalid("invalid workspace folder"));
    }
    Ok(d::workspace_directory(path)?)
}
fn commit(subject: &str, body: &str) -> Result<()> {
    if subject.trim().is_empty() || subject.trim().len() > 200 || body.trim().len() > 4000 {
        return Err(Error::Invalid("invalid Git commit message length"));
    }
    Ok(())
}
fn secrets(authentication: Option<&GitAuthentication>, default_author: &str) -> Result<String> {
    let mut input = String::new();
    let username = authentication.map_or("", |a| a.username.as_str());
    let password = authentication.map_or("", |a| a.password.as_str());
    let author = authentication.map_or("", |a| a.author_name.trim());
    let email = authentication.map_or("", |a| a.author_email.trim());
    for value in [
        username,
        password,
        if author.is_empty() {
            default_author
        } else {
            author
        },
        if email.is_empty() {
            "spin@local.invalid"
        } else {
            email
        },
    ] {
        if value.len() > 64 << 10 || value.contains('\0') {
            return Err(Error::Invalid("invalid Git credential"));
        }
        for ch in value.chars() {
            let mut buffer = [0; 4];
            try_push_str(
                &mut input,
                if matches!(ch, '\n' | '\r') {
                    " "
                } else {
                    ch.encode_utf8(&mut buffer)
                },
            )?;
        }
        try_push_str(&mut input, "\n")?;
    }
    Ok(input)
}
fn environment(command: &mut Command, pairs: &[(&str, &str)]) -> Result<()> {
    command.arg("-e")?;
    command.arg("GIT_TERMINAL_PROMPT=0")?;
    for (key, value) in pairs {
        command.arg("-e")?;
        command.arg(&text(format_args!("{key}={value}"))?)?;
    }
    Ok(())
}
pub(super) fn digest(value: &str) -> Result<String> {
    let mut encoded = String::new();
    encoded
        .try_reserve_exact(24)
        .map_err(|_| d::Error::OutOfMemory)?;
    for byte in &leancrypto::sha256::Sha256::digest(value.as_bytes())[..12] {
        use core::fmt::Write;
        write!(&mut encoded, "{byte:02x}").map_err(|_| d::Error::OutOfMemory)?;
    }
    Ok(encoded)
}
fn marker<'a>(output: &'a str, label: &str, count: usize) -> Result<Vec<&'a str>> {
    for line in output.lines().rev() {
        let mut fields = Vec::new();
        for field in line.split_whitespace().take(count + 1) {
            d::try_push(&mut fields, field)?;
        }
        if fields.len() == count && fields[0] == label {
            return Ok(fields);
        }
    }
    Err(Error::Invalid("Git operation returned no result marker"))
}
fn head(field: &str) -> Result<String> {
    let value = field
        .strip_prefix("head=")
        .ok_or(Error::Invalid("missing Git head"))?;
    if !(7..=64).contains(&value.len()) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Invalid("invalid Git result head"));
    }
    Ok(try_string(value)?)
}
fn accepted(output: &str) -> Result<WorkspaceAcceptanceResult> {
    let fields = marker(output, "SPIN_ACCEPT", 3)?;
    Ok(WorkspaceAcceptanceResult {
        head: head(fields[2])?,
        committed: fields[1] == "committed=1",
    })
}
impl Docker {
    pub(super) async fn git_workspace(
        &self,
        executor: &mut impl Executor,
        runtime: &d::CapsuleRuntime,
        path: &str,
        script: &str,
        pairs: &[(&str, &str)],
        authentication: Option<&GitAuthentication>,
    ) -> Result<String> {
        live_runtime(runtime)?;
        let directory = workspace_directory(path)?;
        let key = text(format_args!(
            "spin-git-{}",
            digest(&text(format_args!(
                "{}\0{directory}",
                runtime.container_id
            ))?)?
        ))?;
        let pid = text(format_args!("/tmp/{key}.pid"))?;
        let script = text(format_args!(
            "echo $$ > '{pid}'\ntrap 'rm -f {pid}' EXIT\n{script}"
        ))?;
        let mut command = self.command(&["exec", "-i", "-w", &directory])?;
        environment(&mut command, pairs)?;
        for argument in [&runtime.container_id, "sh", "-lc", &script] {
            command.arg(argument)?;
        }
        command.input(
            secrets(
                authentication,
                if script.ends_with(MERGE) {
                    "Spin"
                } else {
                    "Spin Agent"
                },
            )?
            .as_bytes(),
        )?;
        command.output_limit = 8 << 20;
        let lease = self.track_process_cleanup(&key, &runtime.container_id, &pid)?;
        let result = checked(executor.run(command).await?)?;
        lease.complete();
        Ok(result)
    }
    // Eén expliciete call beschrijft container, script, omgeving en stdin.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn git_repository(
        &self,
        executor: &mut impl Executor,
        kind: &str,
        remote: &str,
        cache: &str,
        script: &str,
        pairs: &[(&str, &str)],
        authentication: Option<&GitAuthentication>,
    ) -> Result<String> {
        if !crate::git::valid_remote(remote) {
            return Err(Error::Invalid("invalid repository remote URL"));
        }
        let key = if cache.trim().is_empty() {
            digest(remote)?
        } else {
            try_string(cache.trim())?
        };
        let volume = runtime_name(&text(format_args!("spin-{kind}"))?, &key)?;
        // Deze naam bezit tevens het exclusieve gebruik van de cachevolume.
        let name = text(format_args!("{volume}-process"))?;
        let mount = text(format_args!("type=volume,src={volume},dst=/repo"))?;
        let mut command = self.command(&[
            "run",
            "--rm",
            "-i",
            "--name",
            &name,
            "--label",
            "spin.managed=true",
            "--label",
            &text(format_args!("spin.kind={kind}"))?,
            "--mount",
            &mount,
            "-w",
            "/repo",
        ])?;
        environment(&mut command, &[("SPIN_GIT_REMOTE", remote)])?;
        environment(&mut command, pairs)?;
        for argument in ["--entrypoint", "sh", GIT_IMAGE, "-c", script] {
            command.arg(argument)?;
        }
        command.input(
            secrets(
                authentication,
                if kind == "merge" {
                    "Spin"
                } else {
                    "Spin Agent"
                },
            )?
            .as_bytes(),
        )?;
        command.output_limit = 8 << 20;
        let _lease = self.track_cleanup(&name)?;
        // Ook na een CLI-fout houdt de opruimrij eigendom van een eventuele container.
        let output = executor.run(command).await?;
        if output.code != 0 {
            return checked(output);
        }
        Ok(utf8(&output.bytes, false)?)
    }
    /// Integreert één workspace als één gecontroleerde commit op de Job-branch.
    pub async fn accept_workspace(
        &self,
        executor: &mut impl Executor,
        runtime: &d::CapsuleRuntime,
        request: &WorkspaceAcceptance,
    ) -> Result<WorkspaceAcceptanceResult> {
        commit(&request.commit_subject, &request.commit_body)?;
        reference(&request.remote_ref)?;
        if !request.base_branch.is_empty() {
            reference(&request.base_branch)?;
        }
        accepted(
            &self
                .git_workspace(
                    executor,
                    runtime,
                    &request.path,
                    ACCEPT,
                    &[
                        (
                            "SPIN_ALLOW_CHANGES",
                            if request.allow_changes { "1" } else { "0" },
                        ),
                        ("SPIN_BASE_BRANCH", &request.base_branch),
                        ("SPIN_GIT_REF", &request.remote_ref),
                        ("SPIN_COMMIT_SUBJECT", request.commit_subject.trim()),
                        ("SPIN_COMMIT_BODY", request.commit_body.trim()),
                    ],
                    request.authentication.as_ref(),
                )
                .await?,
        )
    }
    /// Publiceert werk in uitvoering naar de eigen Session-branch; niets naar de Job-branch.
    pub async fn sync_workspace(
        &self,
        executor: &mut impl Executor,
        runtime: &d::CapsuleRuntime,
        request: &WorkspaceSync,
    ) -> Result<WorkspaceSyncResult> {
        reference(&request.session_ref)?;
        let output = self
            .git_workspace(
                executor,
                runtime,
                &request.path,
                SYNC,
                &[("SPIN_SESSION_REF", &request.session_ref)],
                request.authentication.as_ref(),
            )
            .await?;
        let fields = marker(&output, "SPIN_SYNC", 4)?;
        Ok(WorkspaceSyncResult {
            head: head(fields[3])?,
            committed: fields[1] == "committed=1",
            pushed: fields[2] == "pushed=1",
        })
    }
    /// Voert de geconfigureerde merge uit in de workspace, met dezelfde refgrenzen als accept.
    pub async fn merge_workspace(
        &self,
        executor: &mut impl Executor,
        runtime: &d::CapsuleRuntime,
        request: &WorkspaceMerge,
    ) -> Result<WorkspaceMergeResult> {
        reference(&request.source_ref)?;
        reference(&request.target_ref)?;
        commit(&request.commit_subject, &request.commit_body)?;
        let output = self
            .git_workspace(
                executor,
                runtime,
                &request.path,
                MERGE,
                &[
                    ("SPIN_MERGE_SOURCE", &request.source_ref),
                    ("SPIN_MERGE_TARGET", &request.target_ref),
                    ("SPIN_COMMIT_SUBJECT", request.commit_subject.trim()),
                    ("SPIN_COMMIT_BODY", request.commit_body.trim()),
                ],
                request.authentication.as_ref(),
            )
            .await?;
        Ok(WorkspaceMergeResult {
            head: head(marker(&output, "SPIN_MERGE", 2)?[1])?,
        })
    }
    /// Een gestopte capsule kan worden geaccepteerd via haar eerder gepubliceerde Session-ref.
    pub async fn accept_repository(
        &self,
        executor: &mut impl Executor,
        request: &RepositoryAcceptance,
    ) -> Result<WorkspaceAcceptanceResult> {
        reference(&request.session_ref)?;
        reference(&request.job_ref)?;
        if !request.bootstrap_ref.is_empty() {
            reference(&request.bootstrap_ref)?;
        }
        commit(&request.commit_subject, &request.commit_body)?;
        accepted(
            &self
                .git_repository(
                    executor,
                    "accept",
                    &request.remote_url,
                    &request.cache_key,
                    ACCEPT_REMOTE,
                    &[
                        ("SPIN_SESSION_REF", &request.session_ref),
                        ("SPIN_GIT_REF", &request.job_ref),
                        ("SPIN_BOOTSTRAP_REF", &request.bootstrap_ref),
                        (
                            "SPIN_ALLOW_CHANGES",
                            if request.allow_changes { "1" } else { "0" },
                        ),
                        ("SPIN_COMMIT_SUBJECT", request.commit_subject.trim()),
                        ("SPIN_COMMIT_BODY", request.commit_body.trim()),
                    ],
                    request.authentication.as_ref(),
                )
                .await?,
        )
    }
    /// Een actiestap kan branches samenvoegen zonder agentsessie of capsule.
    pub async fn merge_repository(
        &self,
        executor: &mut impl Executor,
        request: &RepositoryMerge,
    ) -> Result<WorkspaceMergeResult> {
        reference(&request.source_ref)?;
        reference(&request.target_ref)?;
        commit(&request.commit_subject, &request.commit_body)?;
        let output = self
            .git_repository(
                executor,
                "merge",
                &request.remote_url,
                &request.cache_key,
                MERGE_REMOTE,
                &[
                    ("SPIN_MERGE_SOURCE", &request.source_ref),
                    ("SPIN_MERGE_TARGET", &request.target_ref),
                    ("SPIN_COMMIT_SUBJECT", request.commit_subject.trim()),
                    ("SPIN_COMMIT_BODY", request.commit_body.trim()),
                ],
                request.authentication.as_ref(),
            )
            .await?;
        Ok(WorkspaceMergeResult {
            head: head(marker(&output, "SPIN_MERGE", 2)?[1])?,
        })
    }
}
