//! Docker-images worden via taakbestanden verwerkt; geen complete image belandt op de heap.
use crate::{
    archive::{self, Reader, Temporary},
    process::{self, DockerExecutor},
    progress::Progress,
};
use spin_core::{
    archive as tar,
    docker::{Docker, Executor},
    validation::text,
};
use spin_domain::{self as d, TryClone, Wire, try_push, try_push_str, try_string};
use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Instant;
mod transfer;
pub(crate) use transfer::{export, import};
type Result<T> = std::io::Result<T>;
/// De stappen van een seal, laagbewerking of export in de runnerlog (stderr):
/// `SPIN_<WAT>_STEP id= step= ms= total_ms=`, zodat een trage of hangende
/// stap bij naam te vinden is.
pub(crate) struct Steps<'a> {
    what: &'static str,
    id: &'a str,
    started: Instant,
    last: Instant,
}
impl<'a> Steps<'a> {
    pub(crate) fn new(what: &'static str, id: &'a str) -> Self {
        let now = Instant::now();
        eprintln!("SPIN_{what}_BEGIN id={id}");
        Self {
            what,
            id,
            started: now,
            last: now,
        }
    }
    /// Sluit de stap af die bij de vorige stap (of het begin) begon.
    pub(crate) fn step(&mut self, step: &str) {
        let now = Instant::now();
        eprintln!(
            "SPIN_{}_STEP id={} step={step} ms={} total_ms={}",
            self.what,
            self.id,
            now.duration_since(self.last).as_millis(),
            now.duration_since(self.started).as_millis()
        );
        self.last = now;
    }
}
fn io(error: impl std::error::Error + Send + Sync + 'static) -> std::io::Error {
    std::io::Error::other(error)
}
/// De bovenste laag van de laatst verzegelde image met haar hash. Seal haalt hem
/// één keer uit `docker save`; de export van precies die image gebruikt hem op.
struct Top {
    image: String,
    layer: Temporary,
    hash: String,
}
thread_local! {
    static TOP: RefCell<Option<Top>> = const { RefCell::new(None) };
}
/// Neemt de bewaarde laag mee wanneer `image` (een image-Id) dezelfde is.
fn take_top(image: &str) -> Option<Top> {
    TOP.with_borrow_mut(|top| top.take().filter(|top| top.image == image))
}
async fn control(docker: &Docker, args: &[&str]) -> Result<String> {
    docker.control(&mut DockerExecutor, args).await.map_err(io)
}
async fn input(docker: &Docker, args: &[&str], data: &[u8]) -> Result<String> {
    let mut command = docker.command(args).map_err(io)?;
    command.input(data).map_err(io)?;
    let output = DockerExecutor.run(command).await.map_err(io)?;
    let value = String::from_utf8(output.bytes).map_err(io)?;
    if output.code != 0 {
        return Err(std::io::Error::other(
            text(format_args!(
                "docker exited {}: {}",
                output.code,
                value.trim()
            ))
            .map_err(io)?,
        ));
    }
    Ok(value)
}
fn hex(hash: &[u8]) -> Result<String> {
    let mut value = String::new();
    value.try_reserve_exact(hash.len() * 2).map_err(io)?;
    for byte in hash {
        for nibble in [byte >> 4, byte & 15] {
            value.push(char::from(b"0123456789abcdef"[usize::from(nibble)]));
        }
    }
    Ok(value)
}
async fn save(docker: &Docker, image: &str) -> Result<Temporary> {
    Temporary::capture(docker.command(&["image", "save", image]).map_err(io)?).await
}
/// Docker save kan de manifest achteraan zetten; de tweede passage kiest exact de toplaag.
async fn top_layer(save: &mut Temporary) -> Result<Temporary> {
    let mut top = String::new();
    let mut reader = Reader::new(&mut save.file)?;
    while let Some(entry) = reader.next().await? {
        if tar::clean_path(&entry.header.name).map_err(io)? != "manifest.json" {
            continue;
        }
        if entry.header.size > 4 << 20 {
            return Err(std::io::Error::other("Docker manifest exceeds budget"));
        }
        reader.file.seek(SeekFrom::Start(entry.data))?;
        let size = usize::try_from(entry.header.size).map_err(io)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).map_err(io)?;
        bytes.resize(size, 0);
        reader.file.read_exact(&mut bytes)?;
        let manifests =
            d::List::<d::json::Value>::from_json_with_limit(&bytes, 4 << 20).map_err(io)?;
        top = manifests
            .first()
            .and_then(|v| v.as_object())
            .and_then(|v| v.get("Layers"))
            .and_then(|v| v.as_array())
            .and_then(|v| v.last())
            .and_then(|v| v.as_str())
            .map(try_string)
            .transpose()
            .map_err(io)?
            .unwrap_or_default();
    }
    if top.is_empty() {
        return Err(std::io::Error::other(
            "Docker archive has no layers in its manifest",
        ));
    }
    let top = tar::clean_path(&top).map_err(io)?;
    let mut reader = Reader::new(&mut save.file)?;
    while let Some(entry) = reader.next().await? {
        if tar::clean_path(&entry.header.name).map_err(io)? != top {
            continue;
        }
        let mut layer = Temporary::new()?;
        archive::copy_range(reader.file, &mut layer.file, entry.data, entry.header.size).await?;
        if layer.compressed()? {
            return layer.gzip(true).await;
        }
        layer.rewind()?;
        return Ok(layer);
    }
    Err(std::io::Error::other(
        "top layer is missing from Docker archive",
    ))
}
async fn layer_hash(layer: &mut Temporary) -> Result<String> {
    let mut lines = Vec::new();
    let mut budget = 64_usize << 20;
    let mut reader = Reader::new(&mut layer.file)?;
    while let Some(entry) = reader.next().await? {
        let digest = if entry.header.kind == b'0' {
            archive::hash_range(reader.file, entry.data, entry.header.size).await?
        } else {
            spin_security::sha256(&[])
        };
        let header = entry.header;
        let line = text(format_args!(
            "{}\0{}\0{:o}\0{}\0{}\0{}",
            header.name.strip_prefix("./").unwrap_or(&header.name),
            char::from(header.kind),
            header.mode,
            header.size,
            header.link,
            hex(&digest)?
        ))
        .map_err(io)?;
        if line.len() > budget {
            return Err(std::io::Error::other(
                "layer identity metadata exceeds budget",
            ));
        }
        budget -= line.len();
        try_push(&mut lines, line).map_err(io)?;
    }
    lines.sort_unstable();
    let mut hash = spin_security::Sha256::new();
    for (index, line) in lines.iter().enumerate() {
        if index != 0 {
            hash.update(b"\n");
        }
        hash.update(line.as_bytes());
    }
    hex(&hash.finish())
}
async fn root_fs(docker: &Docker, image: &str) -> Result<String> {
    let value = control(
        docker,
        &[
            "image",
            "inspect",
            "--format",
            "{{json .RootFS.Layers}}",
            image,
        ],
    )
    .await?;
    let layers = d::List::<String>::from_json(value.as_bytes()).map_err(io)?;
    if layers.is_empty() {
        return Ok(String::new());
    }
    let mut hash = spin_security::Sha256::new();
    for (index, layer) in layers.iter().enumerate() {
        if index != 0 {
            hash.update(b"\n");
        }
        hash.update(layer.as_bytes());
    }
    text(format_args!("sha256:{}", hex(&hash.finish())?)).map_err(io)
}
async fn image_identity(docker: &Docker, image: &str) -> Result<String> {
    if image.is_empty() {
        return Ok(String::new());
    }
    if let Ok(value) = control(
        docker,
        &[
            "image",
            "inspect",
            "--format",
            "{{index .Config.Labels \"spin.content\"}}",
            image,
        ],
    )
    .await
        && !value.is_empty()
    {
        return Ok(value);
    }
    root_fs(docker, image).await
}
async fn content_identity(docker: &Docker, layer: &str, parent: &str) -> Result<String> {
    let parent = image_identity(docker, parent).await?;
    let value = text(format_args!("spin-layer\n{parent}\n{layer}")).map_err(io)?;
    text(format_args!(
        "content:{}",
        spin_security::digest_hex(value.as_bytes()).map_err(io)?
    ))
    .map_err(io)
}
/// Een tag telt alleen mee wanneer zijn inhoud of laagketen exact overeenkomt.
pub(crate) async fn has_snapshot(docker: &Docker, snapshot: &d::CapsuleSnapshot) -> Result<bool> {
    if snapshot.driver != "docker" || snapshot.r#ref.trim().is_empty() {
        return Ok(false);
    }
    if control(
        docker,
        &["image", "inspect", "--format", "{{.Id}}", &snapshot.r#ref],
    )
    .await
    .is_err()
    {
        return Ok(false);
    }
    if !snapshot.content.is_empty()
        && image_identity(docker, &snapshot.r#ref)
            .await
            .is_ok_and(|value| value == snapshot.content)
    {
        return Ok(true);
    }
    if snapshot.root_fs.is_empty() {
        return Ok(false);
    }
    Ok(root_fs(docker, &snapshot.r#ref)
        .await
        .is_ok_and(|value| value == snapshot.root_fs))
}
async fn filter(
    layer: &mut Temporary,
    whiteouts: bool,
    dropped: Option<&d::Map<bool>>,
) -> Result<(Temporary, Vec<tar::Whiteout>)> {
    let mut output = Temporary::new()?;
    let mut deletions = Vec::new();
    let mut reader = Reader::new(&mut layer.file)?;
    while let Some(entry) = reader.next().await? {
        if tar::managed_path(&entry.header.name).map_err(io)? {
            continue;
        }
        if whiteouts && let Some(deletion) = tar::whiteout(&entry.header.name).map_err(io)? {
            try_push(&mut deletions, deletion).map_err(io)?;
            continue;
        }
        let absolute = text(format_args!(
            "/{}",
            tar::clean_path(&entry.header.name).map_err(io)?
        ))
        .map_err(io)?;
        if entry.header.kind == b'0'
            && dropped.is_some_and(|drop| drop.get(&absolute).copied().unwrap_or(false))
        {
            continue;
        }
        archive::copy_range(
            reader.file,
            &mut output.file,
            entry.start,
            entry.end - entry.start,
        )
        .await?;
    }
    archive::finish(&mut output.file)?;
    output.rewind()?;
    Ok((output, deletions))
}
async fn delete_paths(docker: &Docker, container: &str, deletions: &[tar::Whiteout]) -> Result<()> {
    let mut script = String::new();
    for deletion in deletions {
        let line = match deletion {
            tar::Whiteout::File(path) => text(format_args!("rm -rf -- {}\n", tar::shell_quote(path).map_err(io)?)),
            tar::Whiteout::Opaque(path) => {
                if path == "/" { return Err(std::io::Error::other("cannot make the container root opaque")); }
                text(format_args!("if [ -d {0} ]; then find {0} -mindepth 1 -maxdepth 1 -exec rm -rf -- {{}} +; fi\n", tar::shell_quote(path).map_err(io)?))
            }
        }.map_err(io)?;
        if script.len() + line.len() > (512 << 10) {
            control(docker, &["exec", container, "sh", "-ec", &script]).await?;
            script.clear();
        }
        try_push_str(&mut script, &line).map_err(io)?;
    }
    if !script.is_empty() {
        control(docker, &["exec", container, "sh", "-ec", &script]).await?;
    }
    Ok(())
}
async fn copy_into(docker: &Docker, container: &str, layer: &mut Temporary) -> Result<()> {
    layer.rewind()?;
    let mut command = docker
        .command(&["cp", "-", &text(format_args!("{container}:/")).map_err(io)?])
        .map_err(io)?;
    command.merge_stderr = false;
    archive::check(
        process::transfer(command, Some(&mut layer.file), None, archive::FILE_LIMIT).await?,
    )
}
async fn remove(docker: &Docker, name: &str) -> bool {
    control(docker, &["rm", "-f", name]).await.is_ok()
}
async fn build(
    docker: &Docker,
    composition: &d::Composition,
    plan: spin_core::layers::LayerPlan<'_>,
) -> Result<String> {
    let target =
        spin_core::docker::runtime_name("spin-compose-build", &composition.id).map_err(io)?;
    let image = text(format_args!(
        "spin/composition:{}",
        spin_core::docker::safe_name(&composition.id).map_err(io)?
    ))
    .map_err(io)?;
    let cleanup = docker.track_cleanup(&target).map_err(io)?;
    let mut steps = Steps::new("BUILD", &composition.id);
    remove(docker, &target).await;
    control(
        docker,
        &[
            "run",
            "-d",
            "--name",
            &target,
            "--label",
            "spin.managed=true",
            "--label",
            "spin.kind=composition-build",
            "--label",
            &text(format_args!("spin.composition_id={}", composition.id)).map_err(io)?,
            "--network",
            "none",
            "--entrypoint",
            "sh",
            &plan.base.snapshot.r#ref,
            "-lc",
            "trap 'exit 0' TERM INT; while :; do sleep 3600; done",
        ],
    )
    .await?;
    steps.step("base_container");
    let result = async {
        for (index, step) in plan.steps.iter().enumerate() {
            if step.full {
                let source = spin_core::docker::runtime_name(
                    "spin-compose-source",
                    &text(format_args!("{}-{}", composition.id, index + 1)).map_err(io)?,
                )
                .map_err(io)?;
                let source_cleanup = docker.track_cleanup(&source).map_err(io)?;
                remove(docker, &source).await;
                control(
                    docker,
                    &[
                        "create",
                        "--name",
                        &source,
                        "--label",
                        "spin.managed=true",
                        "--label",
                        "spin.kind=composition-source",
                        "--entrypoint",
                        "sh",
                        &step.artifact.snapshot.r#ref,
                        "-lc",
                        "exit 0",
                    ],
                )
                .await?;
                let result = async {
                    let mut export =
                        Temporary::capture(docker.command(&["export", &source]).map_err(io)?)
                            .await?;
                    let (mut filtered, _) = filter(&mut export, false, None).await?;
                    copy_into(docker, &target, &mut filtered).await
                }
                .await;
                if remove(docker, &source).await {
                    source_cleanup.complete();
                }
                result?;
                steps.step("full_layer");
            } else {
                let mut saved = save(docker, &step.artifact.snapshot.r#ref).await?;
                steps.step("save");
                let mut layer = top_layer(&mut saved).await?;
                steps.step("top_layer");
                let (mut diff, deletions) = filter(&mut layer, true, None).await?;
                delete_paths(docker, &target, &deletions).await?;
                copy_into(docker, &target, &mut diff).await?;
                steps.step("apply_layer");
            }
        }
        control(
            docker,
            &[
                "commit",
                "--pause=true",
                "--change",
                "LABEL spin.managed=true",
                "--change",
                "LABEL spin.kind=composition-image",
                "--change",
                &text(format_args!("LABEL spin.composition_id={}", composition.id)).map_err(io)?,
                &target,
                &image,
            ],
        )
        .await?;
        steps.step("commit");
        Ok(image)
    }
    .await;
    if remove(docker, &target).await {
        cleanup.complete();
    }
    result
}
async fn selected(
    docker: &Docker,
    composition: &d::Composition,
    artifacts: &[d::Artifact],
) -> Result<(d::CapsuleSnapshot, bool)> {
    let plan = spin_core::layers::plan_layers(composition, artifacts).map_err(io)?;
    if plan.steps.is_empty() {
        return Ok((plan.base.snapshot.try_clone().map_err(io)?, false));
    }
    Ok((
        d::CapsuleSnapshot {
            driver: try_string("docker").map_err(io)?,
            r#ref: build(docker, composition, plan).await?,
            restorable: true,
            ..Default::default()
        },
        true,
    ))
}
pub(crate) async fn materialize(
    docker: &Docker,
    value: &d::protocol::MaterializePayload,
) -> Result<d::CapsuleRuntime> {
    let (snapshot, ephemeral) = selected(docker, &value.composition, &value.artifacts).await?;
    let result = docker
        .materialize_snapshot(
            &mut DockerExecutor,
            &value.composition,
            &snapshot,
            value.authentication.as_ref(),
        )
        .await
        .map_err(io);
    if result.is_err() && ephemeral {
        let _ = control(docker, &["image", "rm", &snapshot.r#ref]).await;
    }
    result
}
pub(crate) async fn start_recording(
    docker: &Docker,
    value: &d::protocol::StartRecordingPayload,
) -> Result<d::CapsuleRuntime> {
    let Some(stack) = value
        .stack
        .as_ref()
        .filter(|stack| !stack.layers.is_empty() && value.parents.len() == 1)
    else {
        return docker
            .start_recording(&mut DockerExecutor, &value.recording, &value.parents)
            .await
            .map_err(io);
    };
    let parent = &value.parents[0];
    let name = spin_core::docker::runtime_name("spin-rec", &value.recording.id).map_err(io)?;
    if control(
        docker,
        &["container", "inspect", "--format", "{{.Id}}", &name],
    )
    .await
    .is_ok()
    {
        let mut runtime = docker
            .start_recording(&mut DockerExecutor, &value.recording, &value.parents)
            .await
            .map_err(io)?;
        runtime.parent_ref = parent.snapshot.r#ref.try_clone().map_err(io)?;
        return Ok(runtime);
    }
    let composition = d::Composition {
        id: text(format_args!("rec-{}", value.recording.id)).map_err(io)?,
        layers: stack.layers.try_clone().map_err(io)?,
        ..Default::default()
    };
    let (snapshot, ephemeral) = selected(docker, &composition, &stack.artifacts).await?;
    let mut base = parent.try_clone().map_err(io)?;
    base.snapshot.r#ref = snapshot.r#ref.try_clone().map_err(io)?;
    let result = docker
        .start_recording(&mut DockerExecutor, &value.recording, &[base])
        .await
        .map_err(io);
    if ephemeral {
        let _ = control(docker, &["image", "rm", &snapshot.r#ref]).await;
    }
    let mut runtime = result?;
    runtime.parent_ref = parent.snapshot.r#ref.try_clone().map_err(io)?;
    Ok(runtime)
}
async fn diff_entries(diff: &mut Temporary) -> Result<(Vec<d::ContentEntry>, d::Map<String>)> {
    let mut entries = Vec::new();
    let mut hashes = d::Map::new();
    let mut reader = Reader::new(&mut diff.file)?;
    while let Some(entry) = reader.next().await? {
        if entry.header.kind != b'0' {
            continue;
        }
        let path = text(format_args!(
            "/{}",
            tar::clean_path(&entry.header.name).map_err(io)?
        ))
        .map_err(io)?;
        let hash = archive::hash_range(reader.file, entry.data, entry.header.size).await?;
        hashes
            .insert(path.try_clone().map_err(io)?, hex(&hash)?)
            .map_err(io)?;
        try_push(
            &mut entries,
            d::ContentEntry {
                path,
                bytes: i64::try_from(entry.header.size).map_err(io)?,
                ..Default::default()
            },
        )
        .map_err(io)?;
    }
    Ok((entries, hashes))
}
async fn hashes_in_image(
    docker: &Docker,
    image: &str,
    paths: &d::Map<String>,
) -> Result<d::Map<String>> {
    let mut input_bytes = String::new();
    let mut result = d::Map::new();
    for (path, _) in paths.iter() {
        if path.contains(['\r', '\n']) {
            continue;
        }
        if input_bytes.len() + path.len() > 1 << 20 {
            hash_batch(docker, image, &input_bytes, &mut result).await?;
            input_bytes.clear();
        }
        try_push_str(&mut input_bytes, path).map_err(io)?;
        try_push_str(&mut input_bytes, "\n").map_err(io)?;
    }
    if !input_bytes.is_empty() {
        hash_batch(docker, image, &input_bytes, &mut result).await?;
    }
    Ok(result)
}
async fn hash_batch(
    docker: &Docker,
    image: &str,
    paths: &str,
    result: &mut d::Map<String>,
) -> Result<()> {
    let output = input(
        docker,
        &[
            "run",
            "--rm",
            "-i",
            "--network",
            "none",
            "--entrypoint",
            "sh",
            image,
            "-c",
            "while IFS= read -r p; do [ -f \"$p\" ] && sha256sum \"$p\"; done; true",
        ],
        paths.as_bytes(),
    )
    .await?;
    for line in output.lines() {
        if let Some((hash, path)) = line.trim().split_once("  ")
            && hash.len() == 64
        {
            result
                .insert(try_string(path).map_err(io)?, try_string(hash).map_err(io)?)
                .map_err(io)?;
        }
    }
    Ok(())
}
async fn rebuild(
    docker: &Docker,
    tag: &str,
    parent: &str,
    recording: &str,
    diff: &mut Temporary,
    deletions: &[tar::Whiteout],
) -> Result<()> {
    let target = spin_core::docker::runtime_name("spin-seal-build", recording).map_err(io)?;
    let cleanup = docker.track_cleanup(&target).map_err(io)?;
    remove(docker, &target).await;
    control(
        docker,
        &[
            "run",
            "-d",
            "--name",
            &target,
            "--label",
            "spin.managed=true",
            "--label",
            "spin.kind=seal-build",
            "--network",
            "none",
            "--entrypoint",
            "sh",
            parent,
            "-lc",
            "trap 'exit 0' TERM INT; while :; do sleep 3600; done",
        ],
    )
    .await?;
    let result = async {
        delete_paths(docker, &target, deletions).await?;
        copy_into(docker, &target, diff).await?;
        let previous = control(docker, &["image", "inspect", "--format", "{{.Id}}", tag])
            .await
            .ok();
        control(
            docker,
            &[
                "commit",
                "--pause=true",
                "--change",
                "LABEL spin.managed=true",
                "--change",
                &text(format_args!("LABEL spin.recording_id={recording}")).map_err(io)?,
                &target,
                tag,
            ],
        )
        .await?;
        if let Some(previous) = previous.filter(|s| !s.is_empty()) {
            let _ = control(docker, &["image", "rm", "-f", &previous]).await;
        }
        Ok(())
    }
    .await;
    if remove(docker, &target).await {
        cleanup.complete();
    }
    result
}
/// Leest een pad per regel van stdin (relatief aan /) en schrijft alleen die als tar:
/// bestanden, links en lege mappen; een gevulde map komt mee via haar inhoud.
const CHANGED_TAR: &str = "cd / || exit 1\nl=$(mktemp) || exit 1\nwhile IFS= read -r p; do\n  if [ -d \"$p\" ] && [ ! -L \"$p\" ]; then\n    [ -n \"$(ls -A \"$p\" 2>/dev/null)\" ] || printf '%s\\n' \"$p\"\n  elif [ -e \"$p\" ] || [ -L \"$p\" ]; then\n    printf '%s\\n' \"$p\"\n  fi\ndone > \"$l\"\nif [ -s \"$l\" ]; then exec tar -cf - -T \"$l\"; fi";
/// De bovenste laag zonder `docker save` (dat de hele image wegschrijft): `docker
/// diff` op de opnamecontainer noemt de veranderde paden en een tijdelijke container
/// van de zojuist gecommitte image levert alleen die als tar. Verwijderd wordt whiteout.
async fn changed_layer(
    docker: &Docker,
    container: &str,
    image: &str,
) -> Result<(Temporary, Vec<tar::Whiteout>)> {
    let changes = control(docker, &["diff", container]).await?;
    let mut list = Temporary::new()?;
    let mut deletions = Vec::new();
    for line in changes.lines() {
        let Some((kind, path)) = line.split_once(' ') else {
            continue;
        };
        let name = tar::clean_path(path).map_err(io)?;
        if name.is_empty() || tar::managed_path(&name).map_err(io)? {
            continue;
        }
        match kind {
            "D" => try_push(
                &mut deletions,
                tar::Whiteout::File(text(format_args!("/{name}")).map_err(io)?),
            )
            .map_err(io)?,
            "A" | "C" => {
                list.file.write_all(name.as_bytes())?;
                list.file.write_all(b"\n")?;
            }
            _ => return Err(std::io::Error::other("unexpected docker diff line")),
        }
    }
    list.rewind()?;
    let mut command = docker
        .command(&[
            "run",
            "--rm",
            "-i",
            "--user",
            "0:0",
            "--network",
            "none",
            "--entrypoint",
            "sh",
            image,
            "-c",
            CHANGED_TAR,
        ])
        .map_err(io)?;
    command.merge_stderr = false;
    let mut layer = Temporary::new()?;
    archive::check(
        process::transfer(
            command,
            Some(&mut list.file),
            Some(&mut layer.file),
            archive::FILE_LIMIT,
        )
        .await?,
    )?;
    if layer.file.metadata()?.len() == 0 {
        archive::finish(&mut layer.file)?;
    }
    layer.rewind()?;
    Ok((layer, deletions))
}
/// De laag zoals een delta hem draagt: eerst de whiteouts, dan de bestanden.
async fn sealed_layer(files: &mut Temporary, deletions: &[tar::Whiteout]) -> Result<Temporary> {
    let mut layer = Temporary::new()?;
    for deletion in deletions {
        let name = match deletion {
            tar::Whiteout::File(path) => {
                let (dir, base) = path.rsplit_once('/').unwrap_or(("", path));
                let dir = dir.trim_start_matches('/');
                if dir.is_empty() {
                    text(format_args!(".wh.{base}"))
                } else {
                    text(format_args!("{dir}/.wh.{base}"))
                }
            }
            tar::Whiteout::Opaque(path) => text(format_args!(
                "{}/.wh..wh..opq",
                path.trim_start_matches('/')
            )),
        }
        .map_err(io)?;
        layer
            .file
            .write_all(&tar::Header::regular(&name, 0).map_err(io)?)?;
    }
    let mut reader = Reader::new(&mut files.file)?;
    while let Some(entry) = reader.next().await? {
        archive::copy_range(
            reader.file,
            &mut layer.file,
            entry.start,
            entry.end - entry.start,
        )
        .await?;
    }
    archive::finish(&mut layer.file)?;
    layer.rewind()?;
    Ok(layer)
}
/// Geeft ook de laag terug zoals een delta hem draagt, of `None` als die niet te
/// maken was (dan leest de seal hem alsnog uit `docker save`).
async fn clean_layer(
    docker: &Docker,
    container: &str,
    tag: &str,
    parent: &str,
    recording: &str,
    rebase: bool,
    progress: Progress<'_>,
) -> Result<(d::LayerContents, Option<Temporary>)> {
    let mut steps = Steps::new("LAYER", recording);
    progress.report("clean", "wijzigingen uitlezen", 0, 0);
    let changed = async {
        let (mut layer, mut deletions) = changed_layer(docker, container, tag).await?;
        let (diff, whiteouts) = filter(&mut layer, true, None).await?;
        for whiteout in whiteouts {
            try_push(&mut deletions, whiteout).map_err(io)?;
        }
        Ok::<_, std::io::Error>((diff, deletions))
    }
    .await;
    let (mut diff, deletions) = match changed {
        Ok(value) => {
            steps.step("changes");
            value
        }
        Err(error) => {
            eprintln!("SPIN_LAYER_SAVE_FALLBACK id={recording} error={error}");
            let mut saved = save(docker, tag).await?;
            steps.step("save");
            let mut layer = top_layer(&mut saved).await?;
            steps.step("top_layer");
            filter(&mut layer, true, None).await?
        }
    };
    progress.report("clean", "bestanden vergelijken", 0, 0);
    let (entries, hashes) = diff_entries(&mut diff).await?;
    steps.step("hash_entries");
    let parent_hashes = if parent.is_empty() || hashes.is_empty() {
        d::Map::new()
    } else {
        hashes_in_image(docker, parent, &hashes).await?
    };
    steps.step("parent_hashes");
    let mut dropped = d::Map::new();
    let mut total = d::ContentTotal::default();
    let mut kept = Vec::new();
    for entry in entries {
        if hashes
            .get(&entry.path)
            .is_some_and(|hash| parent_hashes.get(&entry.path) == Some(hash))
        {
            total.files += 1;
            total.bytes = total
                .bytes
                .checked_add(entry.bytes)
                .ok_or_else(|| std::io::Error::other("layer size overflows"))?;
            dropped.insert(entry.path, true).map_err(io)?;
        } else {
            try_push(&mut kept, entry).map_err(io)?;
        }
    }
    let mut contents = tar::summarize(&kept).map_err(io)?;
    contents.dropped_identical = total;
    if !dropped.is_empty() || rebase {
        progress.report("clean", "laag opnieuw bouwen", 0, 0);
        let (mut filtered, _) = filter(&mut diff, false, Some(&dropped)).await?;
        rebuild(docker, tag, parent, recording, &mut filtered, &deletions).await?;
        steps.step("rebuild");
        diff = filtered;
    }
    let layer = sealed_layer(&mut diff, &deletions).await;
    steps.step("sealed_layer");
    match layer {
        Ok(layer) => Ok((contents, Some(layer))),
        Err(error) => {
            eprintln!("SPIN_LAYER_SAVE_FALLBACK id={recording} error={error}");
            Ok((contents, None))
        }
    }
}
async fn extends(docker: &Docker, image: &str, parent: &str) -> bool {
    let result = async {
        let layers = d::List::<String>::from_json(
            control(
                docker,
                &[
                    "image",
                    "inspect",
                    "--format",
                    "{{json .RootFS.Layers}}",
                    image,
                ],
            )
            .await?
            .as_bytes(),
        )
        .map_err(io)?;
        let below = d::List::<String>::from_json(
            control(
                docker,
                &[
                    "image",
                    "inspect",
                    "--format",
                    "{{json .RootFS.Layers}}",
                    parent,
                ],
            )
            .await?
            .as_bytes(),
        )
        .map_err(io)?;
        Ok::<_, std::io::Error>(
            !below.is_empty() && layers.len() == below.len() + 1 && layers[..below.len()] == *below,
        )
    }
    .await;
    result.unwrap_or(false)
}
/// Commit, opschonen, content-identiteit en pas daarna de opnamecontainer verwijderen.
pub(crate) async fn seal(
    docker: &Docker,
    recording: &d::Recording,
    progress: Progress<'_>,
) -> Result<d::CapsuleSnapshot> {
    let runtime = recording
        .runtime
        .as_ref()
        .filter(|r| r.driver == "docker" && !r.container_id.is_empty())
        .ok_or_else(|| std::io::Error::other("recording has no live Docker capsule"))?;
    let tag = text(format_args!(
        "spin/artifact:{}",
        spin_core::docker::safe_name(&recording.id).map_err(io)?
    ))
    .map_err(io)?;
    let mut steps = Steps::new("SEAL", &recording.id);
    let _ = control(docker, &["exec", &runtime.container_id, "sh", "-c", "rm -rf /root/.npm/_cacache /root/.cache/pip /root/.cache/go-build /tmp/* /var/cache/apk/* 2>/dev/null; true"]).await;
    steps.step("cleanup");
    let rebase = !runtime.parent_ref.is_empty();
    let parent = if rebase {
        runtime.parent_ref.try_clone().map_err(io)?
    } else {
        match control(
            docker,
            &[
                "inspect",
                "--format",
                "{{.Config.Image}}",
                &runtime.container_id,
            ],
        )
        .await
        {
            Ok(parent) => parent,
            Err(_) => runtime.base_ref.try_clone().map_err(io)?,
        }
    };
    steps.step("parent");
    progress.report("commit", "container vastleggen", 0, 0);
    let committed = control(
        docker,
        &[
            "commit",
            "--pause=true",
            "--change",
            "LABEL spin.managed=true",
            "--change",
            &text(format_args!("LABEL spin.recording_id={}", recording.id)).map_err(io)?,
            &runtime.container_id,
            &tag,
        ],
    )
    .await;
    steps.step("commit");
    let mut contents = None;
    let mut top = None;
    if let Err(error) = committed {
        if control(docker, &["image", "inspect", "--format", "{{.Id}}", &tag])
            .await
            .is_err()
        {
            return Err(error);
        }
        if rebase && !extends(docker, &tag, &parent).await {
            contents = Some(
                clean_layer(
                    docker,
                    &runtime.container_id,
                    &tag,
                    &parent,
                    &recording.id,
                    true,
                    progress,
                )
                .await?
                .0,
            );
        }
    } else {
        match clean_layer(
            docker,
            &runtime.container_id,
            &tag,
            &parent,
            &recording.id,
            rebase,
            progress,
        )
        .await
        {
            Ok((value, layer)) => (contents, top) = (Some(value), layer),
            Err(error) if rebase => return Err(error),
            Err(error) => eprintln!("SPIN_SEAL_FULL_DIFF error={error}"),
        }
    }
    steps.step("clean_layer");
    let digest = control(docker, &["image", "inspect", "--format", "{{.Id}}", &tag]).await?;
    let root_fs = root_fs(docker, &tag).await?;
    steps.step("inspect");
    progress.report("identity", "inhoud vaststellen", 0, 0);
    let mut layer = match top {
        Some(layer) => layer,
        None => top_layer(&mut save(docker, &tag).await?).await?,
    };
    let hash = layer_hash(&mut layer).await?;
    let content = content_identity(docker, &hash, &parent).await?;
    steps.step("content_identity");
    let delta = parent.starts_with("spin/artifact:");
    remove(docker, &runtime.container_id).await;
    steps.step("remove_container");
    // Alleen een delta-export gebruikt de laag; anders blijft hij niet op schijf staan.
    TOP.set(if delta {
        Some(Top {
            image: digest.try_clone().map_err(io)?,
            layer,
            hash,
        })
    } else {
        None
    });
    Ok(d::CapsuleSnapshot {
        driver: try_string("docker").map_err(io)?,
        r#ref: tag,
        digest,
        root_fs,
        restorable: true,
        includes_process_state: false,
        contents,
        content,
        delta,
        parent_ref: if delta { parent } else { String::new() },
        ..Default::default()
    })
}
/// Bestanden buiten /workspace die een sessie op haar capsule heeft veranderd.
pub(crate) async fn changes(
    docker: &Docker,
    runtime: &d::CapsuleRuntime,
) -> Result<d::LayerContents> {
    if runtime.driver != "docker" || runtime.container_id.is_empty() || runtime.status == "stopped"
    {
        return Err(std::io::Error::other(
            "composition has no live Docker capsule",
        ));
    }
    let output = control(docker, &["diff", &runtime.container_id]).await?;
    let mut paths = String::new();
    for line in output.lines() {
        let Some((kind, path)) = line.trim().split_once(' ') else {
            continue;
        };
        if !matches!(kind, "A" | "C") {
            continue;
        }
        let path = tar::clean_path(path).map_err(io)?;
        if path == "workspace" || path.starts_with("workspace/") || path.contains(['\n', '\r']) {
            continue;
        }
        try_push_str(&mut paths, &text(format_args!("/{path}\n")).map_err(io)?).map_err(io)?;
    }
    if paths.is_empty() {
        return Ok(d::LayerContents::default());
    }
    let output = input(docker, &["exec", "-i", &runtime.container_id, "sh", "-c", "while IFS= read -r p; do [ -f \"$p\" ] && printf '%s %s\\n' \"$(stat -c %s \"$p\" 2>/dev/null || wc -c < \"$p\")\" \"$p\"; done; true"], paths.as_bytes()).await?;
    let mut entries = Vec::new();
    for line in output.lines() {
        if let Some((size, path)) = line.trim().split_once(' ') {
            try_push(
                &mut entries,
                d::ContentEntry {
                    path: try_string(path).map_err(io)?,
                    bytes: size.parse().unwrap_or(0),
                    ..Default::default()
                },
            )
            .map_err(io)?;
        }
    }
    tar::summarize(&entries).map_err(io)
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::io::Write;
    fn spool(bytes: &[u8]) -> Temporary {
        let mut file = Temporary::new().unwrap();
        file.file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        file
    }
    fn expected() -> d::json::Value {
        d::json::parse(include_str!("../tests/fixtures/archive/expected.json").as_bytes()).unwrap()
    }
    #[test]
    fn sealed_layers_carry_their_whiteouts_to_the_importer() {
        crate::executor::block_on(async {
            let expected = expected();
            let expected = expected.as_object().unwrap();
            let mut save = spool(include_bytes!("../tests/fixtures/archive/save.tar"));
            let mut layer = top_layer(&mut save).await.unwrap();
            let (mut files, deletions) = filter(&mut layer, true, None).await.unwrap();
            let mut sealed = sealed_layer(&mut files, &deletions).await.unwrap();
            // De import filtert de delta-laag: dezelfde bestanden en dezelfde verwijderingen.
            let (mut again, recovered) = filter(&mut sealed, true, None).await.unwrap();
            assert_eq!(recovered, deletions);
            assert_eq!(
                layer_hash(&mut again).await.unwrap(),
                expected.get("filtered_hash").unwrap().as_str().unwrap()
            );
        });
    }
    #[test]
    fn changed_tar_packs_listed_files_links_and_empty_dirs_only() {
        let root = Temporary::new().unwrap().path().with_extension("root");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::create_dir_all(root.join("n")).unwrap();
        std::fs::write(root.join("a/f"), b"file").unwrap();
        std::fs::write(root.join("n/x"), b"unlisted").unwrap();
        std::os::unix::fs::symlink("a/f", root.join("l")).unwrap();
        // macOS-bsdtar schrijft anders binaire xattrs; een capsule-tar doet dat niet.
        let script = CHANGED_TAR.replacen("cd /", "cd \"$1\"", 1).replacen(
            "tar -cf",
            "tar --no-xattrs --no-mac-metadata -cf",
            1,
        );
        let run = |list: &[u8]| {
            let mut child = std::process::Command::new("/bin/sh")
                .args(["-c", &script, "sh", root.to_str().unwrap()])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(list).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            output.stdout
        };
        assert!(run(b"").is_empty());
        let mut tar = spool(&run(b"a/f\ne\nn\nl\nmissing\n"));
        let names = crate::executor::block_on(async {
            let mut names = Vec::new();
            let mut reader = Reader::new(&mut tar.file).unwrap();
            while let Some(entry) = reader.next().await.unwrap() {
                names.push((
                    entry.header.name.trim_end_matches('/').to_string(),
                    entry.header.kind,
                ));
            }
            names
        });
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            names,
            [("a/f".into(), b'0'), ("e".into(), b'5'), ("l".into(), b'2')]
        );
    }
    #[test]
    fn go_image_archives_preserve_hashes_links_whiteouts_and_manifests() {
        crate::executor::block_on(async {
            let expected = expected();
            let expected = expected.as_object().unwrap();
            for bytes in [
                include_bytes!("../tests/fixtures/archive/save.tar").as_slice(),
                include_bytes!("../tests/fixtures/archive/save-compressed.tar").as_slice(),
            ] {
                let mut save = spool(bytes);
                let mut layer = top_layer(&mut save).await.unwrap();
                assert_eq!(
                    layer_hash(&mut layer).await.unwrap(),
                    expected.get("hash").unwrap().as_str().unwrap()
                );
                let (mut filtered, deletions) = filter(&mut layer, true, None).await.unwrap();
                assert_eq!(
                    layer_hash(&mut filtered).await.unwrap(),
                    expected.get("filtered_hash").unwrap().as_str().unwrap()
                );
                let deletions: Vec<_> = deletions
                    .iter()
                    .map(|deletion| match deletion {
                        tar::Whiteout::File(path) => path.clone(),
                        tar::Whiteout::Opaque(path) => format!("{path}/*"),
                    })
                    .collect();
                assert_eq!(
                    deletions.as_slice(),
                    &*d::List::<String>::from_value(expected.get("deletions").unwrap()).unwrap()
                );
                let (entries, _) = diff_entries(&mut filtered).await.unwrap();
                assert_eq!(
                    tar::summarize(&entries).unwrap(),
                    d::LayerContents::from_value(expected.get("contents").unwrap()).unwrap()
                );
            }
        });
    }
    #[test]
    fn truncated_corrupt_archives_and_disk_stream_budget_fail_without_partial_success() {
        crate::executor::block_on(async {
            let mut corrupt = include_bytes!("../tests/fixtures/archive/layer.tar").to_vec();
            corrupt[4] ^= 1;
            assert!(
                Reader::new(&mut spool(&corrupt).file)
                    .unwrap()
                    .next()
                    .await
                    .is_err()
            );
            let mut short = spool(&include_bytes!("../tests/fixtures/archive/layer.tar")[..700]);
            let mut reader = Reader::new(&mut short.file).unwrap();
            assert!(reader.next().await.unwrap().is_some());
            assert!(reader.next().await.is_err());
            let mut source = spool(include_bytes!("../tests/fixtures/archive/layer.tar"));
            let mut destination = Temporary::new().unwrap();
            let mut command = spin_core::process::Command::new("/bin/cat").unwrap();
            command.timeout_ms = 2000;
            assert!(
                process::transfer(
                    command,
                    Some(&mut source.file),
                    Some(&mut destination.file),
                    100
                )
                .await
                .is_err()
            );
            assert!(destination.file.metadata().unwrap().len() <= 100);
        });
    }
}
