//! Een terminalstream houdt maximaal 1 MiB transcript en een begrensde invoerrij.
use crate::{pty::Pty, runner_agent::Output};
use spin_core::docker::Docker;
use spin_domain::{self as d, protocol::InteractivePayload};
use std::io;
pub(crate) struct Terminal {
    pub(crate) container: String,
    process: Pty,
    input: Vec<u8>,
    offset: usize,
    transcript: Vec<u8>,
    closed: bool,
}
impl Terminal {
    pub(crate) fn new(docker: &Docker, payload: &InteractivePayload) -> io::Result<Self> {
        let command = docker
            .interactive_command(&payload.recording, &payload.input)
            .map_err(io::Error::other)?;
        let container = d::try_string(
            payload
                .recording
                .runtime
                .as_ref()
                .map(|r| r.container_id.as_str())
                .unwrap_or(""),
        )
        .map_err(io::Error::other)?;
        Ok(Self {
            container,
            process: Pty::spawn(&command, payload.rows, payload.cols)?,
            input: Vec::new(),
            offset: 0,
            transcript: Vec::new(),
            closed: false,
        })
    }
    pub(crate) fn buffered(&self) -> usize {
        self.input.capacity()
    }
    pub(crate) fn input(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > (1_usize << 20).saturating_sub(self.input.len() - self.offset) {
            return Err(io::Error::other("terminal input buffer full"));
        }
        self.input.drain(..self.offset);
        self.offset = 0;
        self.input
            .try_reserve_exact(bytes.len())
            .map_err(io::Error::other)?;
        self.input.extend_from_slice(bytes);
        Ok(())
    }
    pub(crate) fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        self.process.resize(rows, cols)
    }
    pub(crate) fn poll(&mut self) -> io::Result<Output> {
        if self.offset < self.input.len() {
            let end = self.input.len().min(self.offset + (64 << 10));
            match self.process.write(&self.input[self.offset..end]) {
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
        if !self.closed {
            let mut bytes = [0; 8192];
            match self.process.read(&mut bytes) {
                Ok(0) => self.closed = true,
                Ok(n) => {
                    let remember = n.min((1_usize << 20).saturating_sub(self.transcript.len()));
                    self.transcript
                        .try_reserve_exact(remember)
                        .map_err(io::Error::other)?;
                    self.transcript.extend_from_slice(&bytes[..remember]);
                    let mut output = Vec::new();
                    output.try_reserve_exact(n).map_err(io::Error::other)?;
                    output.extend_from_slice(&bytes[..n]);
                    return Ok(Output::Bytes(output));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        if self.closed
            && let Some(status) = self.process.status()?
        {
            return Ok(Output::Exit(d::engine::Execution {
                exit_code: i64::from(status.code().unwrap_or(-1)),
                output: spin_core::docker::utf8(&self.transcript, true)
                    .map_err(io::Error::other)?,
            }));
        }
        Ok(Output::Waiting)
    }
}
