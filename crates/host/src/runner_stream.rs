//! Streams leven bij de runner, onafhankelijk van de socket. Archives blijven op schijf.
use crate::{
    archive::{FILE_LIMIT, Temporary},
    executor, images,
};
use spin_core::{docker::Docker, validation::text, worker::Worker};
use spin_domain::{
    self as d, TryClone, Wire,
    protocol::{self as p, WireMessage},
};
use std::{
    cell::RefCell,
    io::{Read, Write},
    task::Context,
    time::{Duration, Instant},
};
const CAPACITY: usize = 64;
type Result<T> = std::io::Result<T>;
type Outcome = Result<Option<Temporary>>;
pub(crate) struct Mail(RefCell<[Option<Outcome>; CAPACITY]>);
impl Mail {
    pub(crate) fn new() -> Self {
        Self(RefCell::new(std::array::from_fn(|_| None)))
    }
}
// De vaste tabel reserveert < 32 KiB; zo heeft ieder streamslot zonder extra
// heapallocatie ruimte voor zijn snapshot tijdens de ontvangstfase.
#[allow(clippy::large_enum_variant)]
enum State<'a> {
    Agent(crate::runner_agent::Agent<'a>),
    Terminal(crate::runner_terminal::Terminal),
    Receiving {
        file: Temporary,
        snapshot: d::CapsuleSnapshot,
        size: u64,
    },
    Working(executor::Task<'a>),
    Sending(Temporary),
    Ended,
}
struct Stream<'a> {
    id: String,
    importing: bool,
    state: State<'a>,
    pending: Option<WireMessage>,
    touched: Instant,
}
pub(crate) struct Streams<'a> {
    slots: [Option<Stream<'a>>; CAPACITY],
    mail: &'a Mail,
    docker: &'a Docker,
    archive: &'a crate::blob_client::Client,
}
fn io(error: impl std::error::Error + Send + Sync + 'static) -> std::io::Error {
    std::io::Error::other(error)
}
fn message(kind: &str, id: &str) -> Result<WireMessage> {
    Ok(WireMessage {
        version: p::PROTOCOL_VERSION,
        r#type: d::try_string(kind).map_err(io)?,
        id: d::try_string(id).map_err(io)?,
        ..Default::default()
    })
}
fn exit(id: &str, result: Result<()>) -> Result<WireMessage> {
    let mut value = message(p::MESSAGE_STREAM_EXIT, id)?;
    let mut execution = d::engine::Execution::default();
    if let Err(error) = result {
        value.error = text(format_args!("{error}")).map_err(io)?;
        execution.exit_code = 1;
        execution.output = value.error.try_clone().map_err(io)?;
    }
    value.execution = Some(execution);
    Ok(value)
}
impl<'a> Streams<'a> {
    pub(crate) fn new(
        mail: &'a Mail,
        docker: &'a Docker,
        archive: &'a crate::blob_client::Client,
    ) -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            mail,
            docker,
            archive,
        }
    }
    pub(crate) fn ids(&self, owner: &Worker) -> d::Fallible<d::List<String>> {
        let mut ids = d::List::default();
        for stream in self.slots.iter().flatten() {
            ids.push(stream.id.try_clone()?)?;
        }
        owner.append_stream_ids(&mut ids)?;
        Ok(ids)
    }
    pub(crate) fn install(&mut self, request: &WireMessage) -> Result<d::json::Value> {
        if !self
            .slots
            .iter()
            .flatten()
            .any(|stream| stream.id == request.id)
        {
            let index = self
                .slots
                .iter()
                .position(Option::is_none)
                .ok_or_else(|| std::io::Error::other("runner stream table is full"))?;
            let payload = request.payload.0.as_ref().unwrap_or(&d::json::Value::Null);
            let id = request.id.try_clone().map_err(io)?;
            // Alle fallibele antwoordallocaties gebeuren vóór het werk gepubliceerd wordt.
            let reply = p::StreamResponse {
                stream_id: id.try_clone().map_err(io)?,
            }
            .to_value()
            .map_err(io)?;
            let state = if request.method == p::METHOD_START_INTERACTIVE {
                let payload = p::InteractivePayload::from_value(payload).map_err(io)?;
                let container = payload
                    .recording
                    .runtime
                    .as_ref()
                    .map(|r| r.container_id.as_str())
                    .unwrap_or("");
                if self.slots.iter().flatten().filter(|stream| matches!(&stream.state, State::Terminal(terminal) if terminal.container == container)).count() >= 8 { return Err(std::io::Error::other("capsule already has 8 interactive processes")); }
                State::Terminal(crate::runner_terminal::Terminal::new(
                    self.docker,
                    &payload,
                )?)
            } else if request.method == p::METHOD_START_ENABLED {
                let payload = p::EnabledPayload::from_value(payload).map_err(io)?;
                let agent = crate::runner_agent::Agent::prepare(self.docker, &payload)?;
                for old in self.slots.iter_mut().flatten() {
                    if let State::Agent(previous) = &old.state
                        && previous.key == agent.key
                    {
                        let message = exit(&old.id, Err(std::io::Error::other("agent replaced")))?;
                        old.state = State::Ended;
                        old.pending = Some(message);
                    }
                }
                State::Agent(agent)
            } else if request.method == p::METHOD_PULL_SNAPSHOT {
                let payload = p::SnapshotPullPayload::from_value(payload).map_err(io)?;
                let mail = self.mail;
                let docker = self.docker;
                let archive = self.archive;
                State::Working(executor::task(async move {
                    mail.0.borrow_mut()[index] = Some(
                        archive
                            .pull(docker, &payload.snapshot, payload.size)
                            .await
                            .map(|()| None),
                    );
                })?)
            } else if request.method == p::METHOD_IMPORT_SNAPSHOT {
                let payload = p::SnapshotPayload::from_value(payload).map_err(io)?;
                State::Receiving {
                    file: Temporary::new()?,
                    snapshot: payload.snapshot,
                    size: 0,
                }
            } else {
                let payload = p::SnapshotPayload::from_value(payload).map_err(io)?;
                let mail = self.mail;
                let docker = self.docker;
                State::Working(executor::task(async move {
                    let result = images::export(docker, &payload.snapshot).await.map(Some);
                    mail.0.borrow_mut()[index] = Some(result);
                })?)
            };
            self.slots[index] = Some(Stream {
                id,
                importing: request.method == p::METHOD_IMPORT_SNAPSHOT,
                state,
                pending: None,
                touched: Instant::now(),
            });
            return Ok(reply);
        }
        p::StreamResponse {
            stream_id: request.id.try_clone().map_err(io)?,
        }
        .to_value()
        .map_err(io)
    }
    pub(crate) fn input(&mut self, request: &WireMessage) -> Result<()> {
        let buffered: usize = self
            .slots
            .iter()
            .flatten()
            .map(|stream| match &stream.state {
                State::Agent(agent) => agent.buffered(),
                State::Terminal(terminal) => terminal.buffered(),
                _ => 0,
            })
            .sum();
        let Some(index) = self
            .slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|s| s.id == request.id))
        else {
            return Ok(());
        };
        let Some(stream) = &mut self.slots[index] else {
            return Ok(());
        };
        stream.touched = Instant::now();
        if matches!(stream.state, State::Agent(_) | State::Terminal(_)) {
            let bytes = request.data.0.as_deref().unwrap_or_default();
            let result = if bytes.len() > (32_usize << 20).saturating_sub(buffered) {
                Err(std::io::Error::other("runner agent input budget is full"))
            } else {
                match &mut stream.state {
                    State::Agent(agent) => agent.input(bytes),
                    State::Terminal(terminal) => terminal.input(bytes),
                    _ => Ok(()),
                }
            };
            if let Err(error) = result {
                let message = exit(&stream.id, Err(error))?;
                stream.state = State::Ended;
                stream.pending = Some(message);
            }
            return Ok(());
        }
        if let State::Receiving { file, size, .. } = &mut stream.state {
            let bytes = request.data.0.as_deref().unwrap_or_default();
            let length = u64::try_from(bytes.len()).map_err(io)?;
            let result = if length > FILE_LIMIT.saturating_sub(*size) {
                Err(std::io::Error::other("snapshot input exceeds disk budget"))
            } else {
                file.file.write_all(bytes)
            };
            match result {
                Ok(()) => {
                    *size += length;
                    executor::progress();
                }
                Err(error) => {
                    let message = exit(&stream.id, Err(error))?;
                    stream.state = State::Ended;
                    stream.pending = Some(message);
                }
            }
        }
        Ok(())
    }
    pub(crate) fn resize(&mut self, request: &WireMessage) -> Result<()> {
        if let Some(stream) = self
            .slots
            .iter_mut()
            .flatten()
            .find(|stream| stream.id == request.id)
            && let State::Terminal(terminal) = &mut stream.state
        {
            terminal.resize(request.rows, request.cols)?;
        }
        Ok(())
    }
    pub(crate) fn close(&mut self, id: &str, cancel: bool) -> Result<()> {
        let Some(index) = self
            .slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|s| s.id == id))
        else {
            return Ok(());
        };
        let Some(stream) = &mut self.slots[index] else {
            return Ok(());
        };
        if matches!(stream.state, State::Ended) {
            return Ok(());
        }
        if !cancel && stream.importing && !matches!(stream.state, State::Receiving { .. }) {
            return Ok(());
        }
        if cancel || !matches!(stream.state, State::Receiving { .. }) {
            let message = exit(id, Err(std::io::Error::other("stream cancelled")))?;
            stream.state = State::Ended;
            stream.pending = Some(message);
            self.mail.0.borrow_mut()[index] = None;
            return Ok(());
        }
        let State::Receiving {
            mut file, snapshot, ..
        } = std::mem::replace(&mut stream.state, State::Ended)
        else {
            return Ok(());
        };
        let mail = self.mail;
        let docker = self.docker;
        match executor::task(async move {
            let result = images::import(docker, &snapshot, &mut file)
                .await
                .map(|()| None);
            mail.0.borrow_mut()[index] = Some(result);
        }) {
            Ok(task) => stream.state = State::Working(task),
            Err(error) => stream.pending = Some(exit(id, Err(error))?),
        }
        stream.touched = Instant::now();
        Ok(())
    }
    pub(crate) fn poll(&mut self, context: &mut Context<'_>, owner: &mut Worker) -> Result<()> {
        for index in 0..CAPACITY {
            let Some(stream) = &mut self.slots[index] else {
                continue;
            };
            if let Some(pending) = &stream.pending {
                if !owner.enqueue(pending).map_err(io)? {
                    continue;
                }
                stream.pending = None;
                stream.touched = Instant::now();
                if matches!(stream.state, State::Ended) {
                    self.slots[index] = None;
                    continue;
                }
            }
            if matches!(stream.state, State::Receiving { .. })
                && stream.touched.elapsed() > Duration::from_secs(15 * 60)
            {
                stream.pending = Some(exit(&stream.id, Err(std::io::ErrorKind::TimedOut.into()))?);
                stream.state = State::Ended;
                continue;
            }
            if let State::Working(task) = &mut stream.state
                && task.as_mut().poll(context).is_ready()
            {
                let outcome = self.mail.0.borrow_mut()[index]
                    .take()
                    .ok_or_else(|| std::io::Error::other("stream task returned no outcome"))?;
                match outcome {
                    Ok(Some(file)) => stream.state = State::Sending(file),
                    result => {
                        stream.pending = Some(exit(&stream.id, result.map(|_| ()))?);
                        stream.state = State::Ended;
                    }
                }
            }
            if let State::Sending(file) = &mut stream.state {
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(32 << 10).map_err(io)?;
                bytes.resize(32 << 10, 0);
                match file.file.read(&mut bytes) {
                    Ok(n) if n > 0 => {
                        bytes.truncate(n);
                        let mut value = message(p::MESSAGE_STREAM_DATA, &stream.id)?;
                        value.data = d::Bytes(Some(bytes));
                        stream.pending = Some(value);
                        executor::progress();
                    }
                    result => {
                        stream.pending = Some(exit(&stream.id, result.map(|_| ()))?);
                        stream.state = State::Ended;
                    }
                }
            }
            let process = match &mut stream.state {
                State::Agent(agent) => Some(agent.poll()),
                State::Terminal(terminal) => Some(terminal.poll()),
                _ => None,
            };
            if let Some(output) = process {
                use crate::runner_agent::Output;
                match output {
                    Ok(Output::Waiting) => {}
                    Ok(Output::Bytes(bytes)) => {
                        let mut value = message(p::MESSAGE_STREAM_DATA, &stream.id)?;
                        value.data = d::Bytes(Some(bytes));
                        stream.pending = Some(value);
                    }
                    result => {
                        let mut message = exit(&stream.id, Ok(()))?;
                        match result {
                            Ok(Output::Exit(execution)) => message.execution = Some(execution),
                            Err(error) => message = exit(&stream.id, Err(error))?,
                            _ => {}
                        }
                        stream.pending = Some(message);
                        stream.state = State::Ended;
                    }
                }
            }
        }
        Ok(())
    }
}
