//! Het Docker-contract zonder OS-toegang. De host bezit ieder gestart proces.
use crate::{process::Command, validation::text};
use alloc::{string::String, vec::Vec};
use spin_domain::{self as d, TryClone, try_push_str, try_string};

/// Appservice-opdrachten en herstartvaste naamgeving.
pub mod apps;
mod enabled;
mod files;
mod git;
mod git_operations;
mod git_read;
#[cfg(test)]
mod tests;

/// Een fout behoudt de processtatus zonder argv of credentials te loggen.
#[derive(Debug)]
pub enum Error {
    /// Ongeldige domeindata of uitgeput allocatiebudget.
    Domain(d::Error),
    /// De host kon de opdracht niet uitvoeren.
    Transport(String),
    /// Docker eindigde met een fout; de uitvoer is al begrensd.
    Exit {
        /// Exitcode van de Docker CLI.
        code: i32,
        /// Begrensde uitvoer, zonder de meegegeven argumenten.
        output: String,
    },
    /// Een capsule-aanvraag voldoet niet aan het enginecontract.
    Invalid(&'static str),
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Domain(error) => error.fmt(f),
            Self::Transport(error) => f.write_str(error),
            Self::Exit { code, output } => write!(f, "docker exited {code}: {output}"),
            Self::Invalid(error) => f.write_str(error),
        }
    }
}
impl core::error::Error for Error {}
impl From<d::Error> for Error {
    fn from(error: d::Error) -> Self {
        Self::Domain(error)
    }
}
/// Resultaat van één engine-operatie.
pub type Result<T> = core::result::Result<T, Error>;
/// Docker schrijft beide uitvoerkanalen naar één geordende stroom.
pub struct Output {
    /// Negatief wanneer het kind door een signaal eindigde.
    pub code: i32,
    /// De begrensde, ongewijzigde uitvoerbytes.
    pub bytes: Vec<u8>,
}
/// Eén eigenaar voert een proces uit; laten vallen annuleert uitsluitend dat proces.
pub trait Executor {
    /// Voert de expliciete argv uit, zonder een extra shell.
    fn run(&mut self, command: Command) -> impl core::future::Future<Output = Result<Output>>;
}
/// Onveranderlijke engine-instellingen; geen verborgen processen of gedeelde locks.
pub struct Docker {
    binary: String,
    base: String,
    network: String,
    cleanup: core::cell::RefCell<[Option<Cleanup>; 128]>,
    cleanup_cursor: core::cell::Cell<usize>,
}
struct Cleanup {
    name: String,
    process: Option<(String, String)>,
    ready: bool,
}
/// Een tijdelijke buildcontainer heeft ook bij Future-annulering een opruimeigenaar.
pub struct CleanupLease<'a> {
    docker: &'a Docker,
    index: usize,
    active: bool,
}
impl CleanupLease<'_> {
    /// Alleen na een geslaagde expliciete remove hoeft de achtergrondrij niets meer te doen.
    pub fn complete(mut self) {
        self.docker.cleanup.borrow_mut()[self.index] = None;
        self.active = false;
    }
}
impl Drop for CleanupLease<'_> {
    fn drop(&mut self) {
        if self.active
            && let Some(cleanup) = &mut self.docker.cleanup.borrow_mut()[self.index]
        {
            cleanup.ready = true;
        }
    }
}
const KEEP_ALIVE: &str = "trap 'exit 0' TERM INT; while :; do sleep 3600; done";
impl Docker {
    /// Lege instellingen krijgen dezelfde defaults als de Go-runner.
    pub fn new(binary: &str, base: &str, network: &str) -> Result<Self> {
        let binary = if binary.is_empty() { "docker" } else { binary };
        Command::new(binary)?;
        Ok(Self {
            binary: try_string(binary)?,
            base: try_string(if base.is_empty() { "alpine:3.24" } else { base })?,
            network: try_string(if network.is_empty() {
                "bridge"
            } else {
                network
            })?,
            cleanup: core::cell::RefCell::new(core::array::from_fn(|_| None)),
            cleanup_cursor: core::cell::Cell::new(0),
        })
    }
    /// Reserveert cleanup vóór er een buildcontainer gemaakt kan worden.
    pub fn track_cleanup(&self, name: &str) -> Result<CleanupLease<'_>> {
        let name = try_string(name)?;
        let mut slots = self.cleanup.borrow_mut();
        if slots.iter().flatten().any(|entry| entry.name == name) {
            return Err(Error::Invalid(
                "temporary container is still owned by another operation",
            ));
        }
        let index = slots
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Invalid("temporary container cleanup table is full"))?;
        slots[index] = Some(Cleanup {
            name,
            process: None,
            ready: false,
        });
        Ok(CleanupLease {
            docker: self,
            index,
            active: true,
        })
    }
    /// Klaarstaande cleanup is onafhankelijk van de verdwenen aanvraag of socket.
    pub fn needs_cleanup(&self) -> bool {
        self.cleanup
            .borrow()
            .iter()
            .flatten()
            .any(|entry| entry.ready)
    }
    /// Een vervangende agent wacht totdat zijn voorganger ook in de capsule is gestopt.
    pub fn cleanup_pending(&self, name: &str) -> bool {
        self.cleanup
            .borrow()
            .iter()
            .flatten()
            .any(|entry| entry.name == name)
    }
    /// Een Docker-exec-proces moet binnen zijn container worden beëindigd, niet alleen op de host.
    pub fn track_process_cleanup(
        &self,
        key: &str,
        container: &str,
        pid_file: &str,
    ) -> Result<CleanupLease<'_>> {
        enabled::validate_pid_file(pid_file)?;
        let process = (try_string(container)?, try_string(pid_file)?);
        let lease = self.track_cleanup(key)?;
        if let Some(entry) = &mut self.cleanup.borrow_mut()[lease.index] {
            entry.process = Some(process);
        }
        Ok(lease)
    }
    /// Probeert één cleanup; een mislukte poging houdt haar plek vast voor een volgende ronde.
    pub async fn clean_one(&self, executor: &mut impl Executor) -> Result<()> {
        let pending = {
            let slots = self.cleanup.borrow();
            (0..slots.len())
                .find_map(|offset| {
                    let index = (self.cleanup_cursor.get() + offset) % slots.len();
                    let entry = &slots[index];
                    entry
                        .as_ref()
                        .filter(|entry| entry.ready)
                        .map(|entry| -> d::Fallible<_> {
                            Ok((
                                index,
                                entry.name.try_clone()?,
                                entry
                                    .process
                                    .as_ref()
                                    .map(|(container, pid)| -> d::Fallible<_> {
                                        Ok((container.try_clone()?, pid.try_clone()?))
                                    })
                                    .transpose()?,
                            ))
                        })
                })
                .transpose()?
        };
        let Some((index, name, process)) = pending else {
            return Ok(());
        };
        self.cleanup_cursor.set((index + 1) % 128);
        let mut command = match process {
            Some((container, pid)) => {
                let script = enabled::cleanup_command(&pid)?;
                self.command(&["exec", &container, "sh", "-lc", &script])?
            }
            None => self.command(&["rm", "-f", &name])?,
        };
        command.timeout_ms = 15_000;
        let output = executor.run(command).await?;
        if output.code != 0
            && !contains(&output.bytes, b"No such container")
            && !contains(&output.bytes, b"is not running")
        {
            checked(output)?;
        }
        self.cleanup.borrow_mut()[index] = None;
        Ok(())
    }
    /// De imageadapter kan dezelfde begrensde CLI-opdracht aan een bestandsstroom koppelen.
    pub fn command(&self, args: &[&str]) -> Result<Command> {
        let mut command = Command::new(&self.binary)?;
        command.merge_stderr = true;
        command.timeout_ms = 15 * 60 * 1000;
        for arg in args {
            command.arg(arg)?;
        }
        Ok(command)
    }
    /// Een kleine Docker-controlopdracht, met gecontroleerde exitstatus.
    pub async fn control(&self, executor: &mut impl Executor, args: &[&str]) -> Result<String> {
        checked(executor.run(self.command(args)?).await?)
    }
    /// Vraagt de daemon binnen vijftien seconden om zijn versie.
    pub async fn probe(&self, executor: &mut impl Executor) -> Result<d::CapsuleEngineInfo> {
        let mut command = self.command(&["version", "--format", "{{.Server.Version}}"])?;
        command.timeout_ms = 15_000;
        if checked(executor.run(command).await?)?.is_empty() {
            return Err(Error::Invalid("Docker daemon returned no version"));
        }
        Ok(d::CapsuleEngineInfo {
            driver: try_string("docker")?,
            available: true,
            base_image: self.base.try_clone()?,
            filesystem_snapshots: true,
            process_checkpoints: false,
            interactive_attach_command: true,
            detail: try_string(
                "Docker image commit/clone; process memory and provider KV cache are not included",
            )?,
        })
    }
    async fn container_id(
        &self,
        executor: &mut impl Executor,
        name: &str,
    ) -> Result<Option<String>> {
        let output = executor
            .run(self.command(&["container", "inspect", "--format", "{{.Id}}", name])?)
            .await?;
        if output.code != 0
            && (contains(&output.bytes, b"No such container")
                || contains(&output.bytes, b"No such object"))
        {
            return Ok(None);
        }
        let id = checked(output)?;
        if id.is_empty() {
            return Err(Error::Invalid("Docker returned no container ID"));
        }
        Ok(Some(id))
    }
    /// Herhaalde starts hervatten de opname met dezelfde deterministische containernaam.
    pub async fn start_recording(
        &self,
        executor: &mut impl Executor,
        recording: &d::Recording,
        parents: &[d::Artifact],
    ) -> Result<d::CapsuleRuntime> {
        if parents.len() > 1 {
            return Err(Error::Invalid(
                "Docker recording requires one linear parent snapshot",
            ));
        }
        let base = if let Some(parent) = parents.first() {
            snapshot_ref(&parent.snapshot)?
        } else {
            &self.base
        };
        let name = runtime_name("spin-rec", &recording.id)?;
        let id = if let Some(id) = self.container_id(executor, &name).await? {
            self.control(executor, &["start", &id]).await?;
            id
        } else {
            self.control(
                executor,
                &[
                    "run",
                    "-d",
                    "--pull=missing",
                    "--init",
                    "--name",
                    &name,
                    "--label",
                    "spin.managed=true",
                    "--label",
                    "spin.kind=recording",
                    "--label",
                    &text(format_args!("spin.recording_id={}", recording.id))?,
                    "--network",
                    &self.network,
                    "--env",
                    "DISABLE_AUTOUPDATER=1",
                    "--workdir",
                    "/workspace",
                    "--entrypoint",
                    "sh",
                    base,
                    "-lc",
                    KEEP_ALIVE,
                ],
            )
            .await?;
            self.container_id(executor, &name)
                .await?
                .ok_or(Error::Invalid("started capsule disappeared"))?
        };
        capsule(id, name, base, "recording")
    }
    /// Uitvoeren van een recordingopdracht; een niet-nul exitcode is gewone uitvoer.
    pub async fn execute(
        &self,
        executor: &mut impl Executor,
        recording: &d::Recording,
        input: &str,
    ) -> Result<d::engine::Execution> {
        let runtime = recording
            .runtime
            .as_ref()
            .ok_or(Error::Invalid("recording has no live Docker capsule"))?;
        live_runtime(runtime)?;
        let output = executor
            .run(self.command(&[
                "exec",
                "-i",
                "-w",
                "/workspace",
                &runtime.container_id,
                "sh",
                "-lc",
                input,
            ])?)
            .await?;
        Ok(d::engine::Execution {
            output: utf8(&output.bytes, true)?,
            exit_code: i64::from(output.code),
        })
    }
    /// Start een capsule uit een reeds samengestelde snapshot; de planner bezit de laagopbouw.
    pub async fn materialize_snapshot(
        &self,
        executor: &mut impl Executor,
        composition: &d::Composition,
        snapshot: &d::CapsuleSnapshot,
        authentication: Option<&d::engine::GitAuthentication>,
    ) -> Result<d::CapsuleRuntime> {
        let image = snapshot_ref(snapshot)?;
        let name = runtime_name("spin-use", &composition.id)?;
        // Een replay moet de bestaande workspace en capsule behouden.
        if let Some(id) = self.container_id(executor, &name).await? {
            self.control(executor, &["start", &id]).await?;
            let mut runtime = capsule(id, name, image, "ready")?;
            if !composition.git_workspaces().is_empty() {
                runtime.workspace_ref = runtime_name("spin-work", &composition.session_id)?;
            }
            return Ok(runtime);
        }
        let volume = self
            .prepare_git_workspaces(executor, composition, snapshot, authentication)
            .await?;
        let mut command = self.command(&[
            "run",
            "-d",
            "--init",
            "--name",
            &name,
            "--label",
            "spin.managed=true",
            "--label",
            "spin.kind=composition",
            "--label",
            &text(format_args!("spin.composition_id={}", composition.id))?,
            "--label",
            &text(format_args!(
                "spin.operator={}",
                safe_name(&composition.operator)?
            ))?,
            "--network",
            &self.network,
            "--env",
            "DISABLE_AUTOUPDATER=1",
        ])?;
        if !volume.is_empty() {
            command.arg("--mount")?;
            command.arg(&text(format_args!(
                "type=volume,src={volume},dst=/workspace"
            ))?)?;
        }
        for arg in [
            "--workdir",
            "/workspace",
            "--entrypoint",
            "sh",
            image,
            "-lc",
            KEEP_ALIVE,
        ] {
            command.arg(arg)?;
        }
        checked(executor.run(command).await?)?;
        let id = self
            .container_id(executor, &name)
            .await?
            .ok_or(Error::Invalid("started capsule disappeared"))?;
        let mut runtime = capsule(id, name, image, "ready")?;
        runtime.workspace_ref = volume;
        Ok(runtime)
    }
    /// Alleen gelabelde actieve capsules worden als workload geteld.
    pub async fn live_capsules(
        &self,
        executor: &mut impl Executor,
    ) -> Result<d::engine::LiveCapsules> {
        let mut command = self.command(&["ps", "--filter", "label=spin.managed=true", "--filter", "status=running",
            "--format", "{{.Label \"spin.kind\"}}\t{{.Label \"spin.composition_id\"}}\t{{.Label \"spin.recording_id\"}}"])?;
        command.timeout_ms = 10_000;
        command.output_limit = 1 << 20;
        let output = checked(executor.run(command).await?)?;
        let mut live = d::engine::LiveCapsules {
            compositions: d::List::new(),
            recordings: d::List::new(),
        };
        for line in output.lines() {
            let mut fields = line.trim_end_matches('\r').split('\t').map(str::trim);
            let kind = fields.next().unwrap_or("");
            let composition = fields.next().unwrap_or("");
            let recording = fields.next().unwrap_or("");
            if kind == "composition" && !composition.is_empty() {
                live.compositions.push(try_string(composition)?)?;
            }
            if kind == "recording" && !recording.is_empty() {
                live.recordings.push(try_string(recording)?)?;
            }
        }
        Ok(live)
    }
    async fn remove_container(&self, executor: &mut impl Executor, id: &str) -> Result<()> {
        if id.is_empty() || id.starts_with('-') {
            return Err(Error::Invalid("invalid container ID"));
        }
        let output = executor.run(self.command(&["rm", "-f", id])?).await?;
        if output.code == 0 || contains(&output.bytes, b"No such container") {
            Ok(())
        } else {
            checked(output).map(|_| ())
        }
    }
    /// Verwijdert uitsluitend de opgevraagde Spin-labels; geen globale daemon-cleanup.
    pub async fn remove_capsules(
        &self,
        executor: &mut impl Executor,
        compositions: &[String],
        recordings: &[String],
    ) -> Result<usize> {
        let mut removed = 0;
        let mut first_error = None;
        for (kind, label, ids) in [
            ("composition", "spin.composition_id", compositions),
            ("recording", "spin.recording_id", recordings),
        ] {
            for id in ids {
                let result = self
                    .control(
                        executor,
                        &[
                            "ps",
                            "-aq",
                            "--filter",
                            "label=spin.managed=true",
                            "--filter",
                            &text(format_args!("label=spin.kind={kind}"))?,
                            "--filter",
                            &text(format_args!("label={label}={id}"))?,
                        ],
                    )
                    .await;
                match result {
                    Ok(output) => {
                        for container in output.split_whitespace() {
                            match self.remove_container(executor, container).await {
                                Ok(()) => removed += 1,
                                Err(error) => {
                                    if first_error.is_none() {
                                        first_error = Some(error);
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(removed),
        }
    }
    /// Annulering werkt ook wanneer de server nog geen runtime kon opslaan.
    pub async fn cancel(
        &self,
        executor: &mut impl Executor,
        recording: &d::Recording,
    ) -> Result<()> {
        let fallback = runtime_name("spin-rec", &recording.id)?;
        let id = recording
            .runtime
            .as_ref()
            .map(|r| r.container_id.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&fallback);
        self.remove_container(executor, id).await
    }
    /// Stopt een capsule en verwijdert haar tijdelijke composition-image.
    pub async fn stop(
        &self,
        executor: &mut impl Executor,
        runtime: &d::CapsuleRuntime,
    ) -> Result<()> {
        if !runtime.container_id.is_empty() {
            self.remove_container(executor, &runtime.container_id)
                .await?;
        }
        if runtime.base_ref.starts_with("spin/composition:") {
            self.remove_image(executor, &runtime.base_ref).await?;
        }
        Ok(())
    }
    async fn remove_image(&self, executor: &mut impl Executor, image: &str) -> Result<()> {
        let output = executor.run(self.command(&["image", "rm", image])?).await?;
        if output.code == 0 || contains(&output.bytes, b"No such image") {
            Ok(())
        } else {
            checked(output).map(|_| ())
        }
    }
    /// Een onbekende of al verwijderde snapshot hoeft niet nogmaals opgeruimd te worden.
    pub async fn remove_snapshot(
        &self,
        executor: &mut impl Executor,
        snapshot: &d::CapsuleSnapshot,
    ) -> Result<()> {
        if snapshot.driver == "docker" && !snapshot.r#ref.trim().is_empty() {
            self.remove_image(executor, &snapshot.r#ref).await?;
        }
        Ok(())
    }
}
fn snapshot_ref(snapshot: &d::CapsuleSnapshot) -> Result<&str> {
    if snapshot.driver != "docker"
        || !snapshot.restorable
        || snapshot.r#ref.is_empty()
        || snapshot.r#ref.starts_with('-')
    {
        return Err(Error::Invalid(
            "artifact is not a restorable Docker snapshot",
        ));
    }
    Ok(&snapshot.r#ref)
}
fn live_runtime(runtime: &d::CapsuleRuntime) -> Result<()> {
    if runtime.driver != "docker"
        || runtime.container_id.is_empty()
        || runtime.container_id.starts_with('-')
        || runtime.status == "stopped"
    {
        return Err(Error::Invalid("composition has no live Docker capsule"));
    }
    Ok(())
}
fn capsule(id: String, name: String, base: &str, status: &str) -> Result<d::CapsuleRuntime> {
    Ok(d::CapsuleRuntime {
        driver: try_string("docker")?,
        attach_command: text(format_args!("docker exec -it {id} sh"))?,
        container_id: id,
        container_name: name,
        base_ref: try_string(base)?,
        status: try_string(status)?,
        ..Default::default()
    })
}
fn contains(bytes: &[u8], pattern: &[u8]) -> bool {
    bytes.windows(pattern.len()).any(|s| s == pattern)
}
fn checked(output: Output) -> Result<String> {
    let value = utf8(&output.bytes, true)?;
    if output.code != 0 {
        return Err(Error::Exit {
            code: output.code,
            output: value,
        });
    }
    Ok(value)
}
// Zoals encoding/json: ongeldige UTF-8 uit een kind wordt vervangen, niet gepanickt.
/// Faalbare UTF-8-vervanging voor CLI-uitvoer, met dezelfde trimoptie als Go.
pub fn utf8(mut bytes: &[u8], trim: bool) -> d::Fallible<String> {
    let mut value = String::new();
    while !bytes.is_empty() {
        match core::str::from_utf8(bytes) {
            Ok(part) => {
                try_push_str(&mut value, part)?;
                break;
            }
            Err(error) => {
                let valid = core::str::from_utf8(&bytes[..error.valid_up_to()])
                    .map_err(|_| crate::validation::invalid("process", "invalid text boundary"))?;
                try_push_str(&mut value, valid)?;
                try_push_str(&mut value, "\u{fffd}")?;
                bytes = &bytes[error.valid_up_to()
                    + error
                        .error_len()
                        .unwrap_or(bytes.len() - error.valid_up_to())..];
            }
        }
    }
    if trim {
        try_string(value.trim())
    } else {
        Ok(value)
    }
}
/// Dezelfde ASCII-Dockernaam als de Go-engine, ook voor Unicode-invoer.
pub fn safe_name(value: &str) -> d::Fallible<String> {
    let mut result = String::new();
    for ch in value.chars().map(|c| c.to_lowercase().next().unwrap_or(c)) {
        let mut bytes = [0; 4];
        try_push_str(
            &mut result,
            if ch.is_ascii_alphanumeric() || "_-.".contains(ch) {
                ch.encode_utf8(&mut bytes)
            } else {
                "-"
            },
        )?;
    }
    if result.is_empty() {
        try_push_str(&mut result, "0")?;
    }
    Ok(result)
}
/// Een capsule-ID krijgt maximaal 32 veilige ASCII-tekens achter de prefix.
pub fn runtime_name(prefix: &str, id: &str) -> d::Fallible<String> {
    let id = safe_name(id)?;
    text(format_args!("{prefix}-{}", &id[..id.len().min(32)]))
}
