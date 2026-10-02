//! Checkout-scripts blijven bytegetrouw aan de Go-specificatie; secrets gaan door stdin.
use super::*;
use d::engine::GitAuthentication;
const WORKSPACE_SCRIPT: &str = include_str!("scripts/git-workspace.sh");
const REFERENCE_SCRIPT: &str = include_str!("scripts/git-reference.sh");
impl Docker {
    /// Bouwt een duurzame Session-volume en hergebruikt reeds bestaande branches.
    pub async fn prepare_git_workspaces(
        &self,
        executor: &mut impl Executor,
        composition: &d::Composition,
        snapshot: &d::CapsuleSnapshot,
        authentication: Option<&GitAuthentication>,
    ) -> Result<String> {
        if composition.git_workspaces().is_empty() {
            return Ok(String::new());
        }
        if composition.session_id.is_empty() {
            return Err(Error::Invalid("Git workspace requires a Session"));
        }
        let image = snapshot_ref(snapshot)?;
        let volume = runtime_name("spin-work", &composition.session_id)?;
        self.control(
            executor,
            &[
                "volume",
                "create",
                "--label",
                "spin.managed=true",
                "--label",
                "spin.kind=workspace",
                "--label",
                &text(format_args!("spin.session_id={}", composition.session_id))?,
                &volume,
            ],
        )
        .await?;
        for workspace in composition.git_workspaces() {
            let mut command =
                self.git_checkout_command(&volume, workspace, image, authentication)?;
            command.output_limit = 8 << 20;
            checked(executor.run(command).await?)?;
        }
        Ok(volume)
    }
    fn git_checkout_command(
        &self,
        volume: &str,
        workspace: &d::GitWorkspace,
        image: &str,
        authentication: Option<&GitAuthentication>,
    ) -> Result<Command> {
        if !workspace.path.is_empty()
            && (workspace.path.len() > 200
                || matches!(workspace.path.as_str(), "." | "..")
                || workspace
                    .path
                    .chars()
                    .any(|c| c.is_control() || " /'\"\\".contains(c)))
        {
            return Err(Error::Invalid("invalid workspace folder"));
        }
        let requires = !workspace.account_id.is_empty()
            || matches!(
                workspace.credential_scope.as_str(),
                d::CREDENTIAL_SCOPE_USER | d::CREDENTIAL_SCOPE_GLOBAL
            );
        if requires && authentication.is_none_or(|a| a.password.is_empty()) {
            return Err(Error::Invalid(
                "Git account is bound but no checkout authentication was supplied",
            ));
        }
        let mut secrets = String::new();
        if let Some(auth) = authentication {
            for value in [
                &auth.username,
                &auth.password,
                &auth.author_name,
                &auth.author_email,
            ] {
                if value.contains(['\n', '\r', '\0']) {
                    return Err(Error::Invalid(
                        "checkout credentials must be single-line values",
                    ));
                }
                try_push_str(&mut secrets, value)?;
                try_push_str(&mut secrets, "\n")?;
            }
        } else {
            try_push_str(&mut secrets, "\n\n\n\n")?;
        }
        let mut context = String::new();
        for reference in workspace.context_refs.iter() {
            if !crate::validation::valid_git_base_ref(reference) {
                return Err(Error::Invalid("invalid Git context ref"));
            }
            if !context.is_empty() {
                try_push_str(&mut context, " ")?;
            }
            try_push_str(&mut context, reference)?;
        }
        let mut command = self.command(&[
            "run",
            "-i",
            "--rm",
            "--read-only",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=1m",
            "--label",
            "spin.managed=true",
            "--label",
            "spin.kind=git-checkout",
            "--network",
            &self.network,
            "--mount",
            &text(format_args!("type=volume,src={volume},dst=/workspace"))?,
            "--workdir",
            "/workspace",
            "--env",
            "GIT_TERMINAL_PROMPT=0",
        ])?;
        for (key, value) in [
            ("SPIN_GIT_DIR", workspace.directory()?.as_str()),
            ("SPIN_GIT_REMOTE", &workspace.remote_url),
            ("SPIN_GIT_BASE", &workspace.base_ref),
            ("SPIN_GIT_BOOTSTRAP", &workspace.bootstrap_ref),
            ("SPIN_GIT_HEAD", &workspace.head_ref),
            ("SPIN_GIT_CONTEXT", &context),
            ("SPIN_GIT_MERGE", &workspace.merge_ref),
            ("SPIN_GIT_TARGET", &workspace.target_ref),
        ] {
            command.arg("--env")?;
            command.arg(&text(format_args!("{key}={value}"))?)?;
        }
        for arg in [
            "--entrypoint",
            "sh",
            image,
            "-lc",
            if workspace.changes() {
                WORKSPACE_SCRIPT
            } else {
                REFERENCE_SCRIPT
            },
        ] {
            command.arg(arg)?;
        }
        command.input(secrets.as_bytes())?;
        Ok(command)
    }
}
