//! Begrensde credentialbestanden; nil betekent overslaan, leeg betekent leeg bestand.
use super::*;
use d::{Bytes, Wire, WireMap, engine::TrackedSelection, json::Value};

const FILE_LIMIT: usize = 1 << 20;
const FOLDER_LIMIT: usize = 2000;
/// Alleen absolute, ondubbelzinnige paden kunnen in een tracked selectie staan.
fn valid_path(path: &str) -> bool {
    path.len() <= 300
        && path.starts_with('/')
        && !path
            .chars()
            .any(|c| c.is_control() || " \t\r\n'\"\\*?[]".contains(c))
        && path
            .trim_end_matches('/')
            .strip_prefix('/')
            .is_some_and(|tail| {
                !tail.is_empty()
                    && tail
                        .split('/')
                        .all(|s| !s.is_empty() && s != "." && s != "..")
            })
        && !path.ends_with("//")
}
fn check_path(path: &str) -> Result<()> {
    if valid_path(path) {
        Ok(())
    } else {
        Err(Error::Invalid("invalid tracked path"))
    }
}
fn read_script(selection: &TrackedSelection) -> Result<String> {
    let mut prune = String::new();
    for path in selection.excludes.iter() {
        check_path(path)?;
        if !prune.is_empty() {
            try_push_str(&mut prune, " -o ")?;
        }
        try_push_str(
            &mut prune,
            &text(format_args!("-path '{}'", path.trim_end_matches('/')))?,
        )?;
    }
    if !prune.is_empty() {
        prune = text(format_args!("\\( {prune} \\) -prune -o "))?;
    }
    let emit = "printf 'SPIN_FILE %s ' \"$f\"; base64 < \"$f\" | tr -d '\\n'; printf '\\n'";
    let mut script = String::new();
    for path in selection.paths.iter() {
        check_path(path)?;
        let line = if path.ends_with('/') {
            let folder = path.trim_end_matches('/');
            text(format_args!(
                "if [ -d '{folder}' ]; then n=0; find '{folder}' {prune}-type f ! -name '*.lock' ! -name '*.spin-tmp' -print | head -n 20000 | while IFS= read -r f; do case \"$f\" in *' '*|*\"'\"*|*'\"'*|*'\\'*|*'*'*|*'?'*|*'['*) continue;; esac; if [ \"$n\" -lt {FOLDER_LIMIT} ] && [ \"$(wc -c < \"$f\")\" -le {FILE_LIMIT} ]; then {emit}; n=$((n+1)); else printf 'SPIN_SKIP %s\\n' \"$f\"; fi; done; fi\n"
            ))?
        } else {
            text(format_args!(
                "if [ -f '{path}' ]; then f='{path}'; if [ \"$(wc -c < '{path}')\" -le {FILE_LIMIT} ]; then {emit}; else printf 'SPIN_SKIP %s\\n' \"$f\"; fi; fi\n"
            ))?
        };
        if line.len() > crate::process::ARGUMENT_BYTES.saturating_sub(script.len()) {
            return Err(Error::Invalid("tracked selection exceeds process budget"));
        }
        try_push_str(&mut script, &line)?;
    }
    Ok(script)
}
fn parse(output: &str) -> Result<WireMap<Bytes>> {
    let mut files = WireMap::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let Some(kind) = fields.next() else { continue };
        let Some(path) = fields.next() else { continue };
        if !matches!(kind, "SPIN_SKIP" | "SPIN_FILE") {
            continue;
        }
        check_path(path)?;
        if files.len() >= 20_000 && files.get(path).is_none() {
            return Err(Error::Invalid("too many tracked files"));
        }
        if kind == "SPIN_SKIP" {
            if files.get(path).is_none() {
                files.insert(try_string(path)?, Bytes(None))?;
            }
        } else {
            let encoded = fields.next().unwrap_or("");
            if encoded.len() > FILE_LIMIT.div_ceil(3) * 4 || fields.next().is_some() {
                return Err(Error::Invalid("invalid tracked file data"));
            }
            let data = Bytes::from_value(&Value::String(try_string(encoded)?))?;
            files.insert(try_string(path)?, data)?;
        }
    }
    Ok(files)
}
impl Docker {
    /// Leest bestaande bestanden, met behoud van lege en bewust overgeslagen bestanden.
    pub async fn read_tracked_files(
        &self,
        executor: &mut impl Executor,
        runtime: &d::CapsuleRuntime,
        selection: &TrackedSelection,
    ) -> Result<WireMap<Bytes>> {
        live_runtime(runtime)?;
        let script = read_script(selection)?;
        let mut command = self.command(&["exec", &runtime.container_id, "sh", "-c", &script])?;
        command.output_limit = 16 << 20;
        let output = executor.run(command).await?;
        // Go houdt geldige regels vast als een bestand tijdens het lezen verdwijnt.
        if output.code < 0 {
            return Err(Error::Invalid("tracked file reader was interrupted"));
        }
        parse(&utf8(&output.bytes, false)?)
    }
    /// Plaatst secrets via stdin en een atomaire rename met mode 0600.
    pub async fn write_tracked_files(
        &self,
        executor: &mut impl Executor,
        runtime: &d::CapsuleRuntime,
        files: &WireMap<Bytes>,
    ) -> Result<()> {
        live_runtime(runtime)?;
        let mut input = String::new();
        for (path, data) in files.iter() {
            check_path(path)?;
            let encoded = data.to_value()?;
            let encoded = encoded.as_str().unwrap_or("");
            let line = text(format_args!("{path} {encoded}\n"))?;
            if line.len() > (16_usize << 20).saturating_sub(input.len()) {
                return Err(Error::Invalid("tracked files exceed input budget"));
            }
            try_push_str(&mut input, &line)?;
        }
        // set -e voorkomt dat een geslaagde laatste regel eerdere schrijffouten verbergt.
        let script = "set -e\nwhile IFS=' ' read -r path data; do\n  [ -n \"$path\" ] || continue\n  mkdir -p \"$(dirname \"$path\")\"\n  printf '%s' \"$data\" | base64 -d > \"$path.spin-tmp\"\n  chmod 600 \"$path.spin-tmp\"\n  mv \"$path.spin-tmp\" \"$path\"\ndone";
        let mut command =
            self.command(&["exec", "-i", &runtime.container_id, "sh", "-c", script])?;
        command.input(input.as_bytes())?;
        checked(executor.run(command).await?).map(|_| ())
    }
    /// Langlevende watcher; de host leest CHANGED-regels en bezit annulering/terugdruk.
    pub fn watch_tracked_files(
        &self,
        runtime: &d::CapsuleRuntime,
        selection: &TrackedSelection,
    ) -> Result<Option<Command>> {
        live_runtime(runtime)?;
        let mut dirs = d::List::new();
        for path in selection.paths.iter() {
            check_path(path)?;
            let dir = if path.ends_with('/') {
                path.trim_end_matches('/')
            } else {
                path.rsplit_once('/')
                    .map(|p| if p.0.is_empty() { "/" } else { p.0 })
                    .unwrap_or("/")
            };
            if !dirs.iter().any(|d: &String| d == dir) {
                dirs.push(try_string(dir)?)?;
            }
        }
        if dirs.is_empty() {
            return Ok(None);
        }
        // Paden blijven argv, ook $, backticks en haakjes krijgen geen shell-evaluatie.
        let script = "for d do mkdir -p \"$d\" 2>/dev/null; done\nif command -v inotifywait >/dev/null 2>&1; then\n  inotifywait -m -q -r -e close_write -e moved_to -e create -e delete -e attrib --format '%w%f' \"$@\" 2>/dev/null | while IFS= read -r f; do echo CHANGED; done\nelse\n  prev=\"\"\n  while :; do\n    cur=\"$(find \"$@\" -type f 2>/dev/null | head -n 5000 | while IFS= read -r f; do stat -c '%n %s %Y' \"$f\" 2>/dev/null; done | cksum)\"\n    if [ -n \"$prev\" ] && [ \"$cur\" != \"$prev\" ]; then echo CHANGED; fi\n    prev=\"$cur\"\n    sleep 1\n  done\nfi";
        let mut command = self.command(&[
            "exec",
            "-i",
            &runtime.container_id,
            "sh",
            "-c",
            script,
            "spin-watch",
        ])?;
        command.merge_stderr = false;
        for dir in dirs.iter() {
            command.arg(dir)?;
        }
        Ok(Some(command))
    }
}

impl Docker {
    /// Leest een visuele oplevering als tar; paden blijven letterlijke argv.
    pub fn bundle_workspace(&self, runtime: &d::CapsuleRuntime, path: &str) -> Result<Command> {
        live_runtime(runtime)?;
        check_path(path)?;
        let script = "p=$1\nif [ -d \"$p\" ]; then cd \"$p\" && tar -cf - .\nelif [ -f \"$p\" ]; then cd \"$(dirname \"$p\")\" && tar -cf - \"$(basename \"$p\")\"\nelse exit 44; fi";
        let mut command = self.command(&[
            "exec",
            &runtime.container_id,
            "sh",
            "-c",
            script,
            "spin-bundle",
            path,
        ])?;
        command.merge_stderr = false;
        Ok(command)
    }
    /// Ontvangt een vooraf gecontroleerde tar. Een losse file wist geen naastgelegen werk.
    pub fn place_bundle(
        &self,
        runtime: &d::CapsuleRuntime,
        target: &str,
        folder: bool,
    ) -> Result<Command> {
        live_runtime(runtime)?;
        check_path(target)?;
        if matches!(
            target.trim_end_matches('/'),
            "/" | "/workspace" | "/spin" | "/root" | "/home" | "/tmp"
        ) {
            return Err(Error::Invalid("bundle target must be a child directory"));
        }
        let script = if folder {
            "set -e; t=$1; rm -rf -- \"$t\"; mkdir -p -- \"$t\"; tar -C \"$t\" -xf -"
        } else {
            "set -e; t=$1; d=$(dirname \"$t\"); mkdir -p -- \"$d\"; rm -f -- \"$t\"; tar -C \"$d\" -xf -"
        };
        let mut command = self.command(&[
            "exec",
            "-i",
            &runtime.container_id,
            "sh",
            "-c",
            script,
            "spin-place",
            target,
        ])?;
        command.merge_stderr = false;
        Ok(command)
    }
    /// Plaatst immutable Job-bijlagen onder hun vaste capsulepad.
    pub async fn inject_attachments(
        &self,
        executor: &mut impl Executor,
        runtime: &d::CapsuleRuntime,
        attachments: &[d::protocol::AttachmentPayload],
    ) -> Result<()> {
        live_runtime(runtime)?;
        for attachment in attachments {
            let name = attachment
                .target_path
                .strip_prefix("/spin/job-attachments/")
                .ok_or(Error::Invalid("invalid attachment target"))?;
            if !crate::bundle::safe_name(name) || name.contains('/') {
                return Err(Error::Invalid("invalid attachment target"));
            }
            let data = attachment
                .data
                .0
                .as_deref()
                .ok_or(Error::Invalid("attachment has no data"))?;
            let script = "set -e; mkdir -p /spin/job-attachments; chmod 0755 /spin /spin/job-attachments; t=$1; rm -f -- \"$t.spin-tmp\"; umask 077; cat > \"$t.spin-tmp\"; chmod 0444 \"$t.spin-tmp\"; mv -f -- \"$t.spin-tmp\" \"$t\"";
            let mut command = self.command(&[
                "exec",
                "-i",
                &runtime.container_id,
                "sh",
                "-c",
                script,
                "spin-attachment",
                &attachment.target_path,
            ])?;
            command.input(data)?;
            checked(executor.run(command).await?)?;
        }
        Ok(())
    }
}
