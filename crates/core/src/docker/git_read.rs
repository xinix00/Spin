//! Repositoryverkenning gebruikt dezelfde begrensde clone en scripts als de Go-runner.
use super::*;
use d::{List, engine::*};
const BROWSE: &str = include_str!("scripts/browse-repository.sh");
const FILE_LIMIT: usize = 512 << 10;
fn file_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.starts_with(['/', '-'])
        && !path.contains(['\0', '\r', '\n'])
        && path.split('/').all(|s| !matches!(s, "" | "." | ".."))
}
enum Target<'a> {
    Capsule(&'a d::CapsuleRuntime, &'a str),
    Volume(&'a str),
}
impl Docker {
    async fn workspace_read(
        &self,
        executor: &mut impl Executor,
        target: &Target<'_>,
        args: &[&str],
    ) -> Result<Output> {
        let mut lease = None;
        let mut command = match target {
            Target::Capsule(runtime, path) => {
                live_runtime(runtime)?;
                let directory = super::git_operations::workspace_directory(path)?;
                self.command(&["exec", "-w", &directory, &runtime.container_id])?
            }
            Target::Volume(volume) => {
                let name = text(format_args!("{volume}-read"))?;
                lease = Some(self.track_cleanup(&name)?);
                self.command(&[
                    "run",
                    "--rm",
                    "--name",
                    &name,
                    "--label",
                    "spin.managed=true",
                    "--label",
                    "spin.kind=compare",
                    "--mount",
                    &text(format_args!("type=volume,src={volume},dst=/repo"))?,
                    "-w",
                    "/repo",
                    "--entrypoint",
                    args.first()
                        .copied()
                        .ok_or(Error::Invalid("empty Git command"))?,
                    "alpine/git:latest",
                ])?
            }
        };
        for argument in if matches!(target, Target::Volume(_)) {
            &args[1..]
        } else {
            args
        } {
            command.arg(argument)?;
        }
        let output = executor.run(command).await;
        if output.as_ref().is_ok_and(|o| o.code == 0)
            && let Some(lease) = lease
        {
            lease.complete();
        }
        output
    }
    /// Leest de werkboom, inclusief nieuwe bestanden en begrensde patches.
    pub async fn inspect_workspace(
        &self,
        executor: &mut impl Executor,
        runtime: &d::CapsuleRuntime,
        path: &str,
    ) -> Result<WorkspaceChanges> {
        self.inspect_workspace_commits(executor, Target::Capsule(runtime, path), "HEAD", "")
            .await
    }
    /// Resolveert een Job- of Sessionvergelijking voordat de patches worden gelezen.
    pub async fn inspect_workspace_range(
        &self,
        executor: &mut impl Executor,
        runtime: &d::CapsuleRuntime,
        comparison: &WorkspaceComparison,
    ) -> Result<WorkspaceChanges> {
        super::git_operations::reference(&comparison.base_ref)?;
        super::git_operations::reference(&comparison.head_ref)?;
        let pattern = comparison.commit_message_match.trim();
        if pattern.len() > 256 || pattern.contains(['\0', '\r', '\n']) {
            return Err(Error::Invalid("invalid Git commit match"));
        }
        if !comparison.merge_commit.is_empty()
            && (!(7..=64).contains(&comparison.merge_commit.len())
                || !comparison
                    .merge_commit
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()))
        {
            return Err(Error::Invalid("invalid Git merge commit"));
        }
        let output = self
            .git_workspace(
                executor,
                runtime,
                &comparison.path,
                include_str!("scripts/compare-workspace.sh"),
                &[
                    ("SPIN_COMPARE_BASE", &comparison.base_ref),
                    ("SPIN_COMPARE_HEAD", &comparison.head_ref),
                    ("SPIN_COMPARE_COMMIT_MATCH", pattern),
                    ("SPIN_COMPARE_MERGE", &comparison.merge_commit),
                ],
                comparison.authentication.as_ref(),
            )
            .await?;
        let (base, head) = comparison_result(&output)?;
        if head.is_empty() && !pattern.is_empty() {
            return Ok(WorkspaceChanges {
                files: List::new(),
                ..Default::default()
            });
        }
        self.inspect_workspace_commits(
            executor,
            Target::Capsule(runtime, &comparison.path),
            base,
            head,
        )
        .await
    }
    async fn inspect_workspace_commits(
        &self,
        executor: &mut impl Executor,
        target: Target<'_>,
        base: &str,
        head: &str,
    ) -> Result<WorkspaceChanges> {
        let mut result = WorkspaceChanges {
            files: List::new(),
            ..Default::default()
        };
        let branch = self
            .workspace_read(executor, &target, &["git", "branch", "--show-current"])
            .await?;
        if branch.code == 0 {
            result.branch = utf8(&branch.bytes, true)?;
        }
        if base != "HEAD" || !head.is_empty() {
            let mut args = arguments(&["git", "diff", "--name-status", "-z", base])?;
            if !head.is_empty() {
                args.push(head);
            }
            let output = read_text(self.workspace_read(executor, &target, &args).await?)?;
            let mut fields = output.split('\0');
            while let (Some(status), Some(mut name)) = (fields.next(), fields.next()) {
                if status.is_empty() || name.is_empty() {
                    continue;
                }
                if status.starts_with(['R', 'C']) {
                    name = fields
                        .next()
                        .ok_or(Error::Invalid("incomplete Git rename"))?;
                }
                let code = status
                    .get(..1)
                    .filter(|s| s.is_ascii())
                    .ok_or(Error::Invalid("invalid Git status"))?;
                changed_file(&mut result, name, &text(format_args!("{code} "))?)?;
            }
        }
        if head.is_empty() {
            let output = read_text(
                self.workspace_read(
                    executor,
                    &target,
                    &[
                        "git",
                        "status",
                        "--porcelain=v1",
                        "--untracked-files=all",
                        "-z",
                    ],
                )
                .await?,
            )?;
            let mut fields = output.split('\0');
            while let Some(field) = fields.next() {
                if field.len() < 4 {
                    continue;
                }
                let status = field.get(..2).ok_or(Error::Invalid("invalid Git status"))?;
                let name = field
                    .get(3..)
                    .ok_or(Error::Invalid("invalid Git status path"))?;
                changed_file(&mut result, name, status)?;
                if status.starts_with(['R', 'C']) {
                    fields.next();
                }
            }
        }
        let mut args = arguments(&["git", "diff", "--numstat", "-z", base])?;
        if !head.is_empty() {
            args.push(head);
        }
        let output = self.workspace_read(executor, &target, &args).await?;
        if output.code == 0 {
            let output = utf8(&output.bytes, false)?;
            let mut fields = output.split('\0');
            while let Some(field) = fields.next() {
                let mut parts = field.splitn(3, '\t');
                let (Some(added), Some(deleted), Some(mut name)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                if name.is_empty() {
                    fields.next();
                    name = fields
                        .next()
                        .ok_or(Error::Invalid("incomplete Git numstat rename"))?;
                }
                let file = changed_file_if_missing(&mut result, name)?;
                file.added = added.parse().unwrap_or(0);
                file.deleted = deleted.parse().unwrap_or(0);
                let added = file.added;
                result.added = result.added.saturating_add(added);
                // Borrow again after updating the aggregate.
                let deleted: i64 = deleted.parse().unwrap_or(0);
                result.deleted = result.deleted.saturating_add(deleted);
            }
        }
        if !head.is_empty() {
            let output = self
                .workspace_read(executor, &target, &["git", "rev-parse", head])
                .await?;
            if output.code == 0 {
                result.head = utf8(&output.bytes, true)?;
            }
        }
        let mut remaining = 2 << 20;
        for file in result.files.as_mut_slice() {
            file.head = result.head.try_clone()?;
            if file.status == "??" && file.added == 0 && file.deleted == 0 {
                let output = self
                    .workspace_read(executor, &target, &["wc", "-l", "--", &file.path])
                    .await?;
                if output.code == 0 {
                    file.added = utf8(&output.bytes, true)?
                        .split_whitespace()
                        .next()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0);
                    result.added = result.added.saturating_add(file.added);
                }
            }
            if remaining == 0 {
                file.truncated = true;
                continue;
            }
            let mut args = arguments(&[
                "git",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--unified=3",
            ])?;
            if file.status == "??" {
                args.extend_from_slice(&["--no-index", "--", "/dev/null", &file.path]);
            } else {
                args.push(base);
                if !head.is_empty() {
                    args.push(head);
                }
                args.extend_from_slice(&["--", &file.path]);
            }
            let output = self.workspace_read(executor, &target, &args).await?;
            if output.code != 0 && !(file.status == "??" && output.code == 1) {
                continue;
            }
            let mut patch = utf8(&output.bytes, false)?;
            file.binary = patch.contains("Binary files ") || patch.contains("GIT binary patch");
            let limit = remaining.min(FILE_LIMIT);
            if patch.len() > limit {
                let mut end = limit;
                while !patch.is_char_boundary(end) {
                    end -= 1;
                }
                if let Some(newline) = patch[..end].rfind('\n') {
                    end = newline + 1;
                }
                patch.truncate(end);
                file.truncated = true;
            }
            remaining -= patch.len();
            file.patch = patch;
        }
        Ok(result)
    }
    /// Vergelijkt duurzame Job-branches zonder een levende Job-capsule.
    pub async fn compare_repository(
        &self,
        executor: &mut impl Executor,
        request: &RepositoryComparison,
    ) -> Result<WorkspaceChanges> {
        let comparison = &request.comparison;
        super::git_operations::reference(&comparison.base_ref)?;
        super::git_operations::reference(&comparison.head_ref)?;
        if comparison.commit_message_match.len() > 256
            || comparison.commit_message_match.contains(['\0', '\r', '\n'])
            || (!comparison.merge_commit.is_empty()
                && (!(7..=64).contains(&comparison.merge_commit.len())
                    || !comparison
                        .merge_commit
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit())))
        {
            return Err(Error::Invalid("invalid repository comparison"));
        }
        let key = if request.cache_key.trim().is_empty() {
            super::git_operations::digest(&request.remote_url)?
        } else {
            try_string(request.cache_key.trim())?
        };
        let volume = runtime_name("spin-compare", &key)?;
        let output = self
            .git_repository(
                executor,
                "compare",
                &request.remote_url,
                &key,
                include_str!("scripts/compare-repository.sh"),
                &[
                    ("SPIN_COMPARE_BASE", &comparison.base_ref),
                    ("SPIN_COMPARE_HEAD", &comparison.head_ref),
                    (
                        "SPIN_COMPARE_COMMIT_MATCH",
                        comparison.commit_message_match.trim(),
                    ),
                    ("SPIN_COMPARE_MERGE", &comparison.merge_commit),
                ],
                comparison.authentication.as_ref(),
            )
            .await?;
        let (base, head) = comparison_result(&output)?;
        if head.is_empty() {
            return Ok(WorkspaceChanges {
                files: List::new(),
                ..Default::default()
            });
        }
        self.inspect_workspace_commits(executor, Target::Volume(&volume), base, head)
            .await
    }
    /// Leest branches, bestandsnamen of een begrensd bestand uit een runnerclone.
    pub async fn browse_repository(
        &self,
        executor: &mut impl Executor,
        request: &RepositoryBrowse,
    ) -> Result<RepositoryBrowseResult> {
        if !matches!(request.mode.as_str(), "refs" | "tree" | "file") {
            return Err(Error::Invalid("invalid repository browse mode"));
        }
        if request.mode != "refs" {
            super::git_operations::reference(&request.r#ref)?;
        }
        if request.mode == "file" && !file_path(&request.path) {
            return Err(Error::Invalid("invalid repository file path"));
        }
        let output = self
            .git_repository(
                executor,
                "browse",
                &request.remote_url,
                &request.cache_key,
                BROWSE,
                &[
                    ("SPIN_MODE", &request.mode),
                    ("SPIN_REF", &request.r#ref),
                    ("SPIN_PATH", &request.path),
                    ("SPIN_LIMIT", &text(format_args!("{FILE_LIMIT}"))?),
                ],
                request.authentication.as_ref(),
            )
            .await?;
        parse_browse(request, &output)
    }
}
fn comparison_result(output: &str) -> Result<(&str, &str)> {
    for line in output.lines().rev() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("SPIN_COMPARE") {
            continue;
        }
        let base = fields
            .next()
            .and_then(|s| s.strip_prefix("base="))
            .ok_or(Error::Invalid("missing comparison base"))?;
        let head = fields
            .next()
            .and_then(|s| s.strip_prefix("head="))
            .ok_or(Error::Invalid("missing comparison head"))?;
        for value in [base, head] {
            if !value.is_empty()
                && (!(7..=64).contains(&value.len())
                    || !value.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Err(Error::Invalid("invalid comparison commit"));
            }
        }
        if base.is_empty() {
            return Err(Error::Invalid("empty comparison base"));
        }
        return Ok((base, head));
    }
    Err(Error::Invalid("missing comparison marker"))
}
fn changed_file<'a>(
    result: &'a mut WorkspaceChanges,
    name: &str,
    status: &str,
) -> Result<&'a mut WorkspaceFileChange> {
    if let Some(index) = result.files.iter().position(|f| f.path == name) {
        let file = &mut result.files.as_mut_slice()[index];
        file.status = try_string(status)?;
        return Ok(file);
    }
    if result.files.len() >= 2000 {
        return Err(Error::Invalid("too many changed files"));
    }
    result.files.push(WorkspaceFileChange {
        path: try_string(name)?,
        status: try_string(status)?,
        ..Default::default()
    })?;
    result
        .files
        .as_mut_slice()
        .last_mut()
        .ok_or(Error::Invalid("missing changed file"))
}
fn changed_file_if_missing<'a>(
    result: &'a mut WorkspaceChanges,
    name: &str,
) -> Result<&'a mut WorkspaceFileChange> {
    if let Some(index) = result.files.iter().position(|f| f.path == name) {
        return Ok(&mut result.files.as_mut_slice()[index]);
    }
    changed_file(result, name, "M ")
}
fn parse_browse(request: &RepositoryBrowse, output: &str) -> Result<RepositoryBrowseResult> {
    let mut result = RepositoryBrowseResult::default();
    match request.mode.as_str() {
        "refs" => {
            for line in output.lines() {
                let Some((stamp, name)) = line.trim().split_once('\t') else {
                    continue;
                };
                if name.is_empty() || name == "HEAD" {
                    continue;
                }
                if result.refs.len() >= 300 {
                    return Err(Error::Invalid("too many repository branches"));
                }
                let seconds = stamp
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| Error::Invalid("invalid branch timestamp"))?;
                let ns = seconds
                    .checked_mul(1_000_000_000)
                    .ok_or(Error::Invalid("branch timestamp out of range"))?;
                result.refs.push(RepositoryRef {
                    name: try_string(name)?,
                    committed_at: d::Timestamp::from_time(d::Time(ns))?,
                })?;
            }
        }
        "tree" => {
            let mut tree = WorkspaceTree {
                r#ref: request.r#ref.try_clone()?,
                entries: List::new(),
            };
            for line in output.lines() {
                let Some((size, path)) = line.split_once('\t') else {
                    continue;
                };
                if path.is_empty() {
                    continue;
                }
                if tree.entries.len() >= 100_000 {
                    return Err(Error::Invalid("repository tree too large"));
                }
                tree.entries.push(WorkspaceEntry {
                    path: try_string(path)?,
                    size: size.trim().parse().unwrap_or(0),
                })?;
            }
            result.tree = Some(tree);
        }
        "file" => {
            let mut remaining = output;
            let (size, content) = loop {
                let Some((line, tail)) = remaining.split_once('\n') else {
                    return Err(Error::Invalid("repository file returned no size"));
                };
                if let Some(size) = line.strip_prefix("SPIN_SIZE ") {
                    let size = size
                        .trim()
                        .parse::<i64>()
                        .map_err(|_| Error::Invalid("invalid repository file size"))?;
                    if size < 0 {
                        return Err(Error::Invalid("negative repository file size"));
                    }
                    break (size, tail);
                }
                remaining = tail;
            };
            let binary = content.contains('\0');
            result.file = Some(WorkspaceFile {
                r#ref: request.r#ref.try_clone()?,
                path: request.path.try_clone()?,
                size,
                content: if binary {
                    String::new()
                } else {
                    try_string(content)?
                },
                binary,
                truncated: size as u64 > content.len() as u64,
            });
        }
        _ => return Err(Error::Invalid("invalid repository browse mode")),
    }
    Ok(result)
}

fn arguments<'a>(initial: &[&'a str]) -> Result<Vec<&'a str>> {
    let mut args = Vec::new();
    args.try_reserve_exact(16)
        .map_err(|_| d::Error::OutOfMemory)?;
    args.extend_from_slice(initial);
    Ok(args)
}
fn read_text(output: Output) -> Result<String> {
    if output.code != 0 {
        return checked(output);
    }
    Ok(utf8(&output.bytes, false)?)
}
