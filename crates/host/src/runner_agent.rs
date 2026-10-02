//! Eén agentproces per capsule/capability; stdio en container-cleanup hebben dezelfde eigenaar.
use crate::{process::Process, storage::Random};
use spin_core::{
    docker::{CleanupLease, Docker},
    process::Command,
    validation::text,
};
use spin_domain::Wire;
use spin_domain::{self as d, TryClone};
use spin_store::IdSource;
use std::io;
const INPUT_LIMIT: usize = 8 << 20;
const STDERR_LIMIT: usize = 256 << 10;
pub(crate) struct Agent<'a> {
    pub(crate) key: String,
    docker: &'a Docker,
    container: String,
    pid: String,
    command: Option<Command>,
    process: Option<Process>,
    lease: Option<CleanupLease<'a>>,
    input: Vec<u8>,
    offset: usize,
    stderr: Vec<u8>,
    out_closed: bool,
    err_closed: bool,
    start_deadline: std::time::Instant,
}
pub(crate) enum Output {
    Waiting,
    Bytes(Vec<u8>),
    Exit(d::engine::Execution),
}
impl<'a> Agent<'a> {
    pub(crate) fn prepare(
        docker: &'a Docker,
        payload: &d::protocol::EnabledPayload,
    ) -> io::Result<Self> {
        let pid = text(format_args!(
            "/tmp/{}.pid",
            Random::open()?
                .next("spin-enabled")
                .map_err(io::Error::other)?
        ))
        .map_err(io::Error::other)?;
        let command = docker
            .enabled_command(&payload.runtime, &payload.enablement, &pid)
            .map_err(io::Error::other)?;
        Ok(Self {
            key: text(format_args!(
                "{}\0{}",
                payload.runtime.container_id, payload.enablement.name
            ))
            .map_err(io::Error::other)?,
            docker,
            container: payload
                .runtime
                .container_id
                .try_clone()
                .map_err(io::Error::other)?,
            pid,
            command: Some(command),
            process: None,
            lease: None,
            input: Vec::new(),
            offset: 0,
            stderr: Vec::new(),
            out_closed: false,
            err_closed: false,
            start_deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
        })
    }
    pub(crate) fn buffered(&self) -> usize {
        self.input.capacity()
    }
    pub(crate) fn input(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > INPUT_LIMIT.saturating_sub(self.input.len() - self.offset) {
            return Err(io::Error::other("agent input exceeds buffer budget"));
        }
        if self.offset > 0 {
            self.input.drain(..self.offset);
            self.offset = 0;
        }
        self.input
            .try_reserve_exact(bytes.len())
            .map_err(io::Error::other)?;
        self.input.extend_from_slice(bytes);
        Ok(())
    }
    pub(crate) fn poll(&mut self) -> io::Result<Output> {
        if self.process.is_none() {
            if std::time::Instant::now() >= self.start_deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            if self.docker.cleanup_pending(&self.key) {
                return Ok(Output::Waiting);
            }
            let Some(command) = self.command.take() else {
                return Err(io::Error::other("agent has no start command"));
            };
            let lease = self
                .docker
                .track_process_cleanup(&self.key, &self.container, &self.pid)
                .map_err(io::Error::other)?;
            self.process = Some(Process::spawn(&command)?);
            self.lease = Some(lease);
        }
        let Some(process) = &mut self.process else {
            return Ok(Output::Waiting);
        };
        if self.offset < self.input.len() {
            let end = self.input.len().min(self.offset + (64 << 10));
            match process.write_input(&self.input[self.offset..end]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.offset += n;
                    if self.offset == self.input.len() {
                        self.input = Vec::new();
                        self.offset = 0;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        let mut buffer = [0; 8192];
        if !self.err_closed {
            for _ in 0..8 {
                match process.read_error(&mut buffer) {
                    Ok(0) => {
                        self.err_closed = true;
                        break;
                    }
                    Ok(n) => {
                        if self.stderr.len() + n > STDERR_LIMIT {
                            self.stderr.drain(..self.stderr.len() + n - STDERR_LIMIT);
                        }
                        self.stderr.try_reserve(n).map_err(io::Error::other)?;
                        self.stderr.extend_from_slice(&buffer[..n]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error),
                }
            }
        }
        if !self.out_closed {
            match process.read_output(&mut buffer) {
                Ok(0) => self.out_closed = true,
                Ok(n) => {
                    let mut bytes = Vec::new();
                    bytes.try_reserve_exact(n).map_err(io::Error::other)?;
                    bytes.extend_from_slice(&buffer[..n]);
                    return Ok(Output::Bytes(bytes));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        if self.out_closed
            && self.err_closed
            && let Some(status) = process.status()?
        {
            let output = spin_core::docker::utf8(&self.stderr, true).map_err(io::Error::other)?;
            return Ok(Output::Exit(d::engine::Execution {
                output,
                exit_code: i64::from(status.code().unwrap_or(-1)),
            }));
        }
        Ok(Output::Waiting)
    }
}

/// ACP-probes stoppen op antwoord-ID 0, niet pas wanneer de langlevende agent eindigt.
pub(crate) async fn probe(
    docker: &Docker,
    payload: &d::protocol::EnabledPayload,
) -> io::Result<d::json::Value> {
    let mut agent = Agent::prepare(docker, payload)?;
    // Een probe heeft eigen procesbezit naast een eventuele reeds draaiende agent.
    agent.key = agent.pid.try_clone().map_err(io::Error::other)?;
    let request = payload
        .request
        .0
        .as_ref()
        .unwrap_or(&d::json::Value::Null)
        .to_json()
        .map_err(io::Error::other)?;
    agent.input(request.as_bytes())?;
    agent.input(b"\n")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut line = Vec::new();
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match agent.poll()? {
            Output::Waiting => {}
            Output::Exit(execution) => {
                return Err(io::Error::other(
                    text(format_args!(
                        "agent closed before its ACP response: {}",
                        execution.output
                    ))
                    .map_err(io::Error::other)?,
                ));
            }
            Output::Bytes(bytes) => {
                for byte in bytes {
                    if byte != b'\n' {
                        if line.len() >= 4 << 20 {
                            return Err(io::Error::other("ACP probe line exceeds 4 MiB"));
                        }
                        d::try_push(&mut line, byte).map_err(io::Error::other)?;
                        continue;
                    }
                    let value = d::json::Value::from_json_with_limit(&line, 4 << 20)
                        .map_err(io::Error::other)?;
                    if value
                        .as_object()
                        .and_then(|object| object.get("id"))
                        .and_then(d::json::Value::as_i64)
                        == Some(0)
                    {
                        return Ok(value);
                    }
                    line.clear();
                }
            }
        }
        crate::executor::next_round().await;
    }
}
