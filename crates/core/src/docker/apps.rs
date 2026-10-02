//! Appservices delen de workspace van de Session, met eigen containers en netwerk.
use super::*;
/// De naamgeving blijft gelijk aan de Go-runner, ook na herstart.
pub fn network(session: &str) -> Result<String> {
    if session.is_empty() || session.len() > 200 || session.chars().any(char::is_control) {
        return Err(Error::Invalid("invalid app session"));
    }
    Ok(runtime_name("spin-app", session)?)
}
/// Alleen gevalideerde receptnamen worden Docker-containernamen.
pub fn container(session: &str, service: &str) -> Result<String> {
    if service.is_empty()
        || service.len() > 100
        || !service
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(Error::Invalid("invalid app service"));
    }
    text(format_args!("{}-{service}", network(session)?)).map_err(Into::into)
}
impl Docker {
    /// Bouwt de startopdracht; env-inhoud blijft uitsluitend op de runner.
    pub fn start_app_service(
        &self,
        runtime: &d::CapsuleRuntime,
        session: &str,
        service: &d::AppService,
        hosts: &[String],
        env_file: Option<&str>,
    ) -> Result<Command> {
        live_runtime(runtime)?;
        if runtime.base_ref.is_empty() || service.image.starts_with('-') {
            return Err(Error::Invalid("invalid app image"));
        }
        let mut list = d::List::new();
        list.push(service.try_clone()?)?;
        let validated = crate::git::app_services(list)?;
        let service = &validated[0];
        let hosts = crate::git::service_hosts(hosts)?;
        let mut command = self.command(&[
            "run",
            "-d",
            "--name",
            &container(session, &service.name)?,
            "--label",
            "spin.managed=true",
            "--label",
            "spin.kind=app",
            "--label",
            &text(format_args!("spin.session_id={session}"))?,
            "--label",
            &text(format_args!("spin.service={}", service.name))?,
            "--network",
            &network(session)?,
            "--network-alias",
            &service.name,
            "--add-host",
            "host.docker.internal:host-gateway",
        ])?;
        for host in hosts.iter() {
            command.arg("--add-host")?;
            command.arg(host)?;
        }
        if let Some(file) = env_file {
            command.arg("--env-file")?;
            command.arg(file)?;
        }
        for port in service.ports.iter() {
            command.arg("-p")?;
            command.arg(&text(format_args!("0:{port}"))?)?;
        }
        if !service.image.is_empty() {
            command.arg(&service.image)?;
        } else {
            if !runtime.workspace_ref.is_empty() {
                command.arg("--mount")?;
                command.arg(&text(format_args!(
                    "type=volume,src={},dst=/workspace",
                    runtime.workspace_ref
                ))?)?;
            }
            let mut script = String::new();
            for step in service.prepare.iter() {
                try_push_str(&mut script, step.trim())?;
                try_push_str(&mut script, " && ")?;
            }
            try_push_str(&mut script, "exec ")?;
            try_push_str(&mut script, service.run.trim())?;
            for arg in [
                "--workdir",
                "/workspace",
                "--entrypoint",
                "sh",
                &runtime.base_ref,
                "-lc",
                &script,
            ] {
                command.arg(arg)?;
            }
        }
        command.timeout_ms = 10 * 60 * 1000;
        Ok(command)
    }
}
