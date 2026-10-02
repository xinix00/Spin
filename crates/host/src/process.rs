//! OS-processen met niet-blokkerende socketparen; de eigenaar pollt zelf, zonder readerthreads.
use spin_core::process::Command;
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    process::{Child, ExitStatus, Stdio},
    task::{Context, Poll},
    time::{Duration, Instant},
};
/// De levensduur bezit het proces en alle drie de stdio-kanalen.
/// Drop beëindigt uitsluitend het kind dat deze instantie zelf gestart heeft.
pub struct Process {
    child: Child,
    stdin: UnixStream,
    stdout: UnixStream,
    stderr: UnixStream,
    status: Option<ExitStatus>,
}
impl Process {
    /// Start zonder shell, met één producer/consumer per stdio-kanaal.
    pub fn spawn(command: &Command) -> std::io::Result<Self> {
        command.validate().map_err(std::io::Error::other)?;
        let (stdin, child_in) = UnixStream::pair()?;
        let (stdout, child_out) = UnixStream::pair()?;
        let (stderr, child_err) = UnixStream::pair()?;
        stdin.set_nonblocking(true)?;
        stdout.set_nonblocking(true)?;
        stderr.set_nonblocking(true)?;
        let child_err = if command.merge_stderr {
            child_out.try_clone()?
        } else {
            child_err
        };
        let mut process = std::process::Command::new(&command.program);
        process.args(&command.args);
        for (key, value) in command.environment.iter() {
            process.env(key, value);
        }
        let child = process
            .stdin(Stdio::from(OwnedFd::from(child_in)))
            .stdout(Stdio::from(OwnedFd::from(child_out)))
            .stderr(Stdio::from(OwnedFd::from(child_err)))
            .spawn()?;
        Ok(Self {
            child,
            stdin,
            stdout,
            stderr,
            status: None,
        })
    }
    /// Schrijft zoveel stdin als de kernel nu accepteert; WouldBlock bewaart terugdruk.
    pub fn write_input(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let count = self.stdin.write(data).inspect_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                crate::executor::wait_for(self.stdin.as_raw_fd(), libc::POLLOUT);
            }
        })?;
        if count > 0 {
            crate::executor::progress();
        }
        Ok(count)
    }
    /// De peer krijgt EOF zodra de laatste invoerbyte geschreven is.
    pub fn close_input(&self) -> std::io::Result<()> {
        self.stdin.shutdown(Shutdown::Write)
    }
    /// Leest stdout zonder te wachten.
    pub fn read_output(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let count = self.stdout.read(bytes).inspect_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                crate::executor::wait_for(self.stdout.as_raw_fd(), libc::POLLIN);
            }
        })?;
        if count > 0 {
            crate::executor::progress();
        }
        Ok(count)
    }
    /// Leest stderr zonder te wachten.
    pub fn read_error(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let count = self.stderr.read(bytes).inspect_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                crate::executor::wait_for(self.stderr.as_raw_fd(), libc::POLLIN);
            }
        })?;
        if count > 0 {
            crate::executor::progress();
        }
        Ok(count)
    }
    /// Reapt een geëindigd proces; de status blijft bij de eigenaar beschikbaar.
    pub fn status(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status)
    }
    /// Annulering is eigendom van de aanvraag, nooit van een globale procesnaam.
    pub fn cancel(&mut self) -> std::io::Result<()> {
        if self.status()?.is_none() {
            self.child.kill()?;
        }
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
/// Uitvoer behoudt de afzonderlijke bytekanalen, ook voor niet-UTF-8-data.
pub struct Output {
    /// De exitstatus van het eigen proces.
    pub status: ExitStatus,
    /// Begrensde stdout.
    pub stdout: Vec<u8>,
    /// Begrensde stderr.
    pub stderr: Vec<u8>,
}
/// Eén capture-future; de hostpool bepaalt hoeveel tegelijk mogen bestaan.
pub struct Capture {
    process: Process,
    input: Vec<u8>,
    offset: usize,
    input_closed: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    out_closed: bool,
    err_closed: bool,
    deadline: Instant,
    limit: usize,
}
/// Docker gebruikt dezelfde begrensde procesadapter als de andere hosttaken.
pub struct DockerExecutor;
impl spin_core::docker::Executor for DockerExecutor {
    async fn run(
        &mut self,
        command: Command,
    ) -> spin_core::docker::Result<spin_core::docker::Output> {
        let output = Capture::start(command)
            .map_err(docker_error)?
            .await
            .map_err(docker_error)?;
        Ok(spin_core::docker::Output {
            code: output.status.code().unwrap_or(-1),
            bytes: output.stdout,
        })
    }
}
fn docker_error(error: std::io::Error) -> spin_core::docker::Error {
    match spin_core::validation::text(format_args!("{error}")) {
        Ok(message) => spin_core::docker::Error::Transport(message),
        Err(error) => error.into(),
    }
}
/// Een grote processtroom gaat blok voor blok naar/vanaf een taakbestand.
/// In RAM blijft hoogstens 64 KiB per kanaal, plus de begrensde diagnostiek.
pub async fn transfer(
    command: Command,
    mut source: Option<&mut std::fs::File>,
    mut sink: Option<&mut std::fs::File>,
    file_limit: u64,
) -> std::io::Result<Output> {
    let mut process = Process::spawn(&command)?;
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(command.timeout_ms))
        .ok_or_else(|| std::io::Error::other("process deadline overflow"))?;
    let mut incoming = [0; 8192];
    let mut pending = 0;
    let mut offset = 0;
    let mut source_offset = 0;
    let mut input_closed = false;
    let mut out_closed = false;
    let mut err_closed = false;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut file_bytes = 0_u64;
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        if !input_closed {
            if offset == pending {
                offset = 0;
                pending = if let Some(source) = &mut source {
                    source.read(&mut incoming)?
                } else {
                    let n = incoming.len().min(command.input.len() - source_offset);
                    incoming[..n].copy_from_slice(&command.input[source_offset..source_offset + n]);
                    source_offset += n;
                    n
                };
                if pending == 0 {
                    process.close_input()?;
                    input_closed = true;
                }
            }
            if !input_closed {
                match process.write_input(&incoming[offset..pending]) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => offset += n,
                    Err(error) if would_block(&error) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                        input_closed = true;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        let mut bytes = [0; 8192];
        for is_stderr in [false, true] {
            for _ in 0..8 {
                if if is_stderr { err_closed } else { out_closed } {
                    break;
                }
                let result = if is_stderr {
                    process.read_error(&mut bytes)
                } else {
                    process.read_output(&mut bytes)
                };
                match result {
                    Ok(0) => {
                        if is_stderr {
                            err_closed = true;
                        } else {
                            out_closed = true;
                        }
                        break;
                    }
                    Ok(n) => {
                        if !is_stderr && let Some(sink) = &mut sink {
                            file_bytes = file_bytes
                                .checked_add(u64::try_from(n).map_err(std::io::Error::other)?)
                                .ok_or_else(|| {
                                    std::io::Error::other("process file size overflows")
                                })?;
                            if file_bytes > file_limit {
                                return Err(std::io::Error::other("process file budget exceeded"));
                            }
                            sink.write_all(&bytes[..n])?;
                        } else {
                            if n > command
                                .output_limit
                                .saturating_sub(stdout.len() + stderr.len())
                            {
                                return Err(std::io::Error::other(
                                    "process output budget exceeded",
                                ));
                            }
                            let target = if is_stderr { &mut stderr } else { &mut stdout };
                            target.try_reserve(n).map_err(std::io::Error::other)?;
                            target.extend_from_slice(&bytes[..n]);
                        }
                    }
                    Err(error) if would_block(&error) => break,
                    Err(error) => return Err(error),
                }
            }
        }
        if let Some(status) = process.status()?
            && out_closed
            && err_closed
        {
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        crate::executor::next_round().await;
    }
}
impl Capture {
    /// Een ongeldige limiet of deadline faalt vóór spawn.
    pub fn start(command: Command) -> std::io::Result<Self> {
        if command.output_limit > 64 << 20
            || command.timeout_ms == 0
            || command.timeout_ms > 24 * 60 * 60 * 1000
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid process budget",
            ));
        }
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(command.timeout_ms))
            .ok_or_else(|| std::io::Error::other("process deadline overflow"))?;
        let process = Process::spawn(&command)?;
        Ok(Self {
            process,
            input: command.input,
            offset: 0,
            input_closed: false,
            stdout: Vec::new(),
            stderr: Vec::new(),
            out_closed: false,
            err_closed: false,
            deadline,
            limit: command.output_limit,
        })
    }
    fn advance(&mut self) -> std::io::Result<Option<Output>> {
        if Instant::now() >= self.deadline {
            self.process.cancel()?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "process deadline exceeded",
            ));
        }
        if !self.input_closed {
            if self.offset < self.input.len() {
                let end = self.input.len().min(self.offset.saturating_add(64 << 10));
                match self.process.write_input(&self.input[self.offset..end]) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => self.offset += n,
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                        self.offset = self.input.len()
                    }
                    Err(error) if would_block(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            if self.offset == self.input.len() {
                self.process.close_input()?;
                self.input_closed = true;
            }
        }
        let mut bytes = [0; 8192];
        for stderr in [false, true] {
            // Hoogstens 64 KiB per kanaal per executorronde.
            for _ in 0..8 {
                if if stderr {
                    self.err_closed
                } else {
                    self.out_closed
                } {
                    break;
                }
                let read = if stderr {
                    self.process.read_error(&mut bytes)
                } else {
                    self.process.read_output(&mut bytes)
                };
                match read {
                    Ok(0) => {
                        if stderr {
                            self.err_closed = true;
                        } else {
                            self.out_closed = true;
                        }
                        break;
                    }
                    Ok(count) => {
                        if count
                            > self
                                .limit
                                .saturating_sub(self.stdout.len() + self.stderr.len())
                        {
                            self.process.cancel()?;
                            return Err(std::io::Error::other("process output budget exceeded"));
                        }
                        let output = if stderr {
                            &mut self.stderr
                        } else {
                            &mut self.stdout
                        };
                        output.try_reserve(count).map_err(std::io::Error::other)?;
                        output.extend_from_slice(&bytes[..count]);
                    }
                    Err(error) if would_block(&error) => break,
                    Err(error) => return Err(error),
                }
            }
        }
        if let Some(status) = self.process.status()?
            && self.out_closed
            && self.err_closed
        {
            return Ok(Some(Output {
                status,
                stdout: std::mem::take(&mut self.stdout),
                stderr: std::mem::take(&mut self.stderr),
            }));
        }
        Ok(None)
    }
}
fn would_block(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}
impl std::future::Future for Capture {
    type Output = std::io::Result<Output>;
    fn poll(mut self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        match self.advance() {
            Ok(Some(output)) => Poll::Ready(Ok(output)),
            Ok(None) => Poll::Pending,
            Err(error) => {
                let _ = self.process.cancel();
                Poll::Ready(Err(error))
            }
        }
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh").unwrap();
        command.arg("-c").unwrap();
        command.arg(script).unwrap();
        command
    }
    #[test]
    fn child_stdio_is_nonblocking_and_preserves_large_binary_input() {
        let mut command = shell("cat; printf err >&2");
        let bytes: Vec<_> = (0..256_u32 * 4096).map(|n| n.to_le_bytes()[0]).collect();
        command.input(&bytes).unwrap();
        let output = crate::executor::block_on(Capture::start(command).unwrap()).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, bytes);
        assert_eq!(output.stderr, b"err");
    }
    #[test]
    fn archive_transfer_preserves_large_files() {
        use std::io::{Seek, Write};
        let mut source = crate::archive::Temporary::new().unwrap();
        let mut sink = crate::archive::Temporary::new().unwrap();
        let block = [0x5a; 8192];
        for _ in 0..8192 {
            source.file.write_all(&block).unwrap();
        }
        source.file.rewind().unwrap();
        let started = Instant::now();
        let output = crate::executor::block_on(transfer(
            shell("cat"),
            Some(&mut source.file),
            Some(&mut sink.file),
            64 << 20,
        ))
        .unwrap();
        assert!(output.status.success());
        assert_eq!(sink.file.metadata().unwrap().len(), 64 << 20);
        eprintln!("64 MiB archive roundtrip: {:?}", started.elapsed());
        sink.file.rewind().unwrap();
        let mut actual = [0; 8192];
        for _ in 0..8192 {
            sink.file.read_exact(&mut actual).unwrap();
            assert_eq!(actual, block);
        }
    }
    #[test]
    fn deadlines_and_output_caps_end_only_the_owned_child() {
        let mut command = shell("while :; do :; done");
        command.timeout_ms = 40;
        let error = crate::executor::block_on(Capture::start(command).unwrap())
            .err()
            .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let mut command = shell("while :; do printf '1234567890'; done");
        command.output_limit = 19;
        assert!(crate::executor::block_on(Capture::start(command).unwrap()).is_err());
        assert!(
            crate::executor::block_on(Capture::start(shell("exit 7")).unwrap())
                .unwrap()
                .status
                .code()
                == Some(7)
        );
    }
}
