//! Een opaque entrypoint krijgt de omgeving van zijn laag en een eigen opruimtoken.
use super::*;
pub(super) fn validate_pid_file(path: &str) -> Result<()> {
    let Some(name) = path.strip_prefix("/tmp/spin-") else {
        return Err(Error::Invalid("invalid process pid file"));
    };
    if name.is_empty()
        || name.len() > 200
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(Error::Invalid("invalid process pid file"));
    }
    Ok(())
}
pub(super) fn cleanup_command(pid_file: &str) -> Result<String> {
    validate_pid_file(pid_file)?;
    Ok(text(format_args!(
        "if read enabled_pid < {pid_file}; then case \"$enabled_pid\" in ''|*[!0-9]*) ;; *) kill \"$enabled_pid\" 2>/dev/null || true;; esac; fi; rm -f {pid_file}"
    ))?)
}
impl Docker {
    /// Docker krijgt een echte host-PTY; terminalhints worden buiten de opname gehouden.
    pub fn interactive_command(&self, recording: &d::Recording, input: &str) -> Result<Command> {
        let runtime = recording
            .runtime
            .as_ref()
            .ok_or(Error::Invalid("recording has no live Docker capsule"))?;
        if runtime.driver != "docker" || runtime.container_id.is_empty() {
            return Err(Error::Invalid("recording has no live Docker capsule"));
        }
        if input.trim().is_empty() {
            return Err(Error::Invalid("interactive command is required"));
        }
        let mut command = self.command(&[
            "exec",
            "-it",
            "-w",
            "/workspace",
            "-e",
            "TERM=xterm-256color",
            &runtime.container_id,
            "sh",
            "-lc",
            input,
        ])?;
        command.env("DOCKER_CLI_HINTS", "false")?;
        Ok(command)
    }
    /// De host houdt stdin/stdout afzonderlijk open; stderr mag het ACP-kanaal niet vervuilen.
    pub fn enabled_command(
        &self,
        runtime: &d::CapsuleRuntime,
        enabled: &d::Enablement,
        pid_file: &str,
    ) -> Result<Command> {
        if runtime.driver != "docker"
            || runtime.container_id.is_empty()
            || runtime.status == "stopped"
        {
            return Err(Error::Invalid("composition has no live Docker capsule"));
        }
        if enabled.transport != "stdio" || enabled.command.trim().is_empty() {
            return Err(Error::Invalid(
                "enabled capability requires a stdio command entrypoint",
            ));
        }
        let name = enabled.name.trim();
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err(Error::Invalid("invalid enabled capability name"));
        }
        validate_pid_file(pid_file)?;
        let shell = r#"if [ -z "${SHELL:-}" ]; then for candidate in /bin/bash /usr/bin/bash /bin/zsh /bin/sh; do if [ -x "$candidate" ]; then export SHELL="$candidate"; break; fi; done; fi; "#;
        let script = text(format_args!(
            "set -a; if [ -f /etc/spin/enabled/{name}.env ]; then . /etc/spin/enabled/{name}.env; fi; set +a; {shell}echo $$ > {pid_file}; exec {}",
            enabled.command
        ))?;
        let mut command = self.command(&[
            "exec",
            "-i",
            "-w",
            "/workspace",
            &runtime.container_id,
            "sh",
            "-lc",
            &script,
        ])?;
        command.merge_stderr = false;
        Ok(command)
    }
}
