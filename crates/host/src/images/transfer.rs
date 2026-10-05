//! Het bestaande gzip/tar-contract: volledige Docker-images of een geverifieerde delta.
use super::*;
use std::io::Write;

const NOTE: &str = "spin-delta.json";
struct Note {
    parent: String,
    content: String,
    layer: String,
}
impl Note {
    fn encode(&self) -> Result<Vec<u8>> {
        let mut value = d::json::Object::new();
        value
            .push("version", 1_i64.to_value().map_err(io)?)
            .map_err(io)?;
        value
            .push("parent_ref", self.parent.to_value().map_err(io)?)
            .map_err(io)?;
        value
            .push("content", self.content.to_value().map_err(io)?)
            .map_err(io)?;
        value
            .push("layer_hash", self.layer.to_value().map_err(io)?)
            .map_err(io)?;
        Ok(d::json::Value::Object(value)
            .to_json()
            .map_err(io)?
            .into_bytes())
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let value = d::json::Value::from_json_with_limit(bytes, 1 << 16).map_err(io)?;
        let object = value
            .as_object()
            .ok_or_else(|| io("delta note must be an object"))?;
        let field = |key| -> Result<String> {
            match object.get(key) {
                None | Some(d::json::Value::Null) => Ok(String::new()),
                Some(value) => String::from_value(value).map_err(io),
            }
        };
        let version =
            i64::from_value(object.get("version").unwrap_or(&d::json::Value::Null)).map_err(io)?;
        if version != 1 {
            return Err(io("unsupported delta archive version"));
        }
        Ok(Self {
            parent: field("parent_ref")?,
            content: field("content")?,
            layer: field("layer_hash")?,
        })
    }
}
fn io(error: impl std::fmt::Display) -> std::io::Error {
    match text(format_args!("{error}")) {
        Ok(value) => std::io::Error::other(value),
        Err(_) => std::io::ErrorKind::OutOfMemory.into(),
    }
}
fn validate(snapshot: &d::CapsuleSnapshot) -> Result<()> {
    if snapshot.driver != "docker" || snapshot.r#ref.trim().is_empty() {
        return Err(io("snapshot is not a transferable Docker image"));
    }
    Ok(())
}
fn padding(file: &mut std::fs::File, size: u64) -> Result<()> {
    let count = usize::try_from((512 - size % 512) % 512).map_err(io)?;
    file.write_all(&[0; 512][..count])
}
async fn delta_archive(layer: &mut Temporary, note: &Note) -> Result<Temporary> {
    let bytes = note.encode()?;
    let mut result = Temporary::new()?;
    let note_size = u64::try_from(bytes.len()).map_err(io)?;
    result
        .file
        .write_all(&tar::Header::regular(NOTE, note_size).map_err(io)?)?;
    result.file.write_all(&bytes)?;
    padding(&mut result.file, note_size)?;
    let size = layer.file.metadata()?.len();
    if size > archive::FILE_LIMIT.saturating_sub(2048 + note_size) {
        return Err(io("delta archive exceeds disk budget"));
    }
    result
        .file
        .write_all(&tar::Header::regular("layer.tar", size).map_err(io)?)?;
    archive::copy_range(&mut layer.file, &mut result.file, 0, size).await?;
    padding(&mut result.file, size)?;
    archive::finish(&mut result.file)?;
    result.gzip(false).await
}
pub(crate) async fn export(
    docker: &Docker,
    snapshot: &d::CapsuleSnapshot,
    progress: Progress<'_>,
) -> Result<Temporary> {
    validate(snapshot)?;
    let mut steps = super::Steps::new("EXPORT", &snapshot.r#ref);
    progress.report("archive", "image uitlezen", 0, 0);
    if !snapshot.delta || snapshot.parent_ref.is_empty() {
        let mut saved = save(docker, &snapshot.r#ref).await?;
        steps.step("save");
        progress.report("archive", "inpakken", 0, 0);
        let archive = saved.gzip(false).await;
        steps.step("gzip");
        return archive;
    }
    // De seal heeft deze laag al uitgelezen zolang de tag nog naar dezelfde image wijst.
    let image = control(
        docker,
        &["image", "inspect", "--format", "{{.Id}}", &snapshot.r#ref],
    )
    .await?;
    let (mut layer, hash) = match super::take_top(&image) {
        Some(top) => {
            steps.step("sealed_layer");
            (top.layer, top.hash)
        }
        None => {
            let mut layer = top_layer(&mut save(docker, &snapshot.r#ref).await?).await?;
            steps.step("top_layer");
            let hash = layer_hash(&mut layer).await?;
            steps.step("layer_hash");
            (layer, hash)
        }
    };
    let note = Note {
        parent: snapshot.parent_ref.try_clone().map_err(io)?,
        content: snapshot.content.try_clone().map_err(io)?,
        layer: hash,
    };
    progress.report("archive", "inpakken", 0, 0);
    let archive = delta_archive(&mut layer, &note).await;
    steps.step("delta_archive");
    archive
}
async fn read_note(archive: &mut Temporary) -> Result<Option<Note>> {
    let mut reader = Reader::new(&mut archive.file)?;
    let Some(entry) = reader.next().await? else {
        return Ok(None);
    };
    if entry
        .header
        .name
        .strip_prefix("./")
        .unwrap_or(&entry.header.name)
        != NOTE
    {
        return Ok(None);
    }
    if entry.header.kind != b'0' || entry.header.size > 1 << 16 {
        return Err(io("invalid delta note size or type"));
    }
    let size = usize::try_from(entry.header.size).map_err(io)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(io)?;
    bytes.resize(size, 0);
    reader.file.seek(SeekFrom::Start(entry.data))?;
    reader.file.read_exact(&mut bytes)?;
    Ok(Some(Note::decode(&bytes)?))
}
async fn extract_layer(source: &mut Temporary) -> Result<Temporary> {
    let mut reader = Reader::new(&mut source.file)?;
    while let Some(entry) = reader.next().await? {
        if entry
            .header
            .name
            .strip_prefix("./")
            .unwrap_or(&entry.header.name)
            != "layer.tar"
        {
            continue;
        }
        if entry.header.kind != b'0' {
            return Err(io("delta layer is not a file"));
        }
        let mut layer = Temporary::new()?;
        archive::copy_range(reader.file, &mut layer.file, entry.data, entry.header.size).await?;
        layer.rewind()?;
        return Ok(layer);
    }
    Err(io("delta archive has no layer"))
}
pub(crate) async fn import(
    docker: &Docker,
    snapshot: &d::CapsuleSnapshot,
    source: &mut Temporary,
) -> Result<()> {
    validate(snapshot)?;
    if source.compressed()? {
        let mut unpacked = source.gzip(true).await?;
        if let Some(note) = read_note(&mut unpacked).await? {
            return import_delta(docker, snapshot, note, &mut unpacked).await;
        }
    }
    source.rewind()?;
    archive::check(
        process::transfer(
            docker.command(&["image", "load"]).map_err(io)?,
            Some(&mut source.file),
            None,
            archive::FILE_LIMIT,
        )
        .await?,
    )?;
    control(
        docker,
        &["image", "inspect", "--format", "{{.Id}}", &snapshot.r#ref],
    )
    .await?;
    let loaded = root_fs(docker, &snapshot.r#ref).await?;
    if snapshot.root_fs.is_empty() {
        return Err(io("snapshot records no layers to verify against"));
    }
    if loaded != snapshot.root_fs {
        return Err(io("imported image has different layers than the snapshot"));
    }
    Ok(())
}
async fn import_delta(
    docker: &Docker,
    snapshot: &d::CapsuleSnapshot,
    note: Note,
    source: &mut Temporary,
) -> Result<()> {
    let parent = if note.parent.is_empty() {
        &snapshot.parent_ref
    } else {
        &note.parent
    };
    if parent.is_empty() {
        return Err(io("delta archive names no parent"));
    }
    if control(docker, &["image", "inspect", "--format", "{{.Id}}", parent])
        .await?
        .is_empty()
    {
        return Err(io("delta parent is not on this runner"));
    }
    let mut layer = extract_layer(source).await?;
    let hash = layer_hash(&mut layer).await?;
    if !note.layer.is_empty() && hash != note.layer {
        return Err(io("delta layer hash mismatch"));
    }
    let parent_content = image_identity(docker, parent).await?;
    let identity = text(format_args!("spin-layer\n{parent_content}\n{hash}")).map_err(io)?;
    let content = text(format_args!(
        "content:{}",
        spin_security::digest_hex(identity.as_bytes()).map_err(io)?
    ))
    .map_err(io)?;
    if !snapshot.content.is_empty() && content != snapshot.content {
        return Err(io(
            "rebuilt snapshot has different content; delta parent is another version",
        ));
    }
    let (mut clean, deletions) = filter(&mut layer, true, None).await?;
    let target = spin_core::docker::runtime_name(
        "spin-delta-build",
        snapshot
            .r#ref
            .strip_prefix("spin/artifact:")
            .unwrap_or(&snapshot.r#ref),
    )
    .map_err(io)?;
    let lease = docker.track_cleanup(&target).map_err(io)?;
    let result = async {
        let _ = control(docker, &["rm", "-f", &target]).await;
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
                "spin.kind=delta-build",
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
        delete_paths(docker, &target, &deletions).await?;
        copy_into(docker, &target, &mut clean).await?;
        let label = text(format_args!("LABEL spin.content={content}")).map_err(io)?;
        control(
            docker,
            &[
                "commit",
                "--pause=true",
                "--change",
                "LABEL spin.managed=true",
                "--change",
                &label,
                &target,
                &snapshot.r#ref,
            ],
        )
        .await?;
        Ok(())
    }
    .await;
    if control(docker, &["rm", "-f", &target]).await.is_ok() {
        lease.complete();
    }
    if result.is_ok()
        && let Ok(image) = control(
            docker,
            &["image", "inspect", "--format", "{{.Id}}", &snapshot.r#ref],
        )
        .await
    {
        // De laag zoals hij binnenkwam (met whiteouts): precies wat een build nodig heeft.
        super::remember_layer(&image, &mut layer).await;
    }
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn delta_archives_round_trip_and_import_checks_before_mutating() {
        crate::executor::block_on(async {
            let mut fixture = Temporary::new().unwrap();
            fixture
                .file
                .write_all(include_bytes!("../../tests/fixtures/archive/delta.tar.gz"))
                .unwrap();
            let mut unpacked = fixture.gzip(true).await.unwrap();
            let note = read_note(&mut unpacked).await.unwrap().unwrap();
            let mut layer = extract_layer(&mut unpacked).await.unwrap();
            assert_eq!(layer_hash(&mut layer).await.unwrap(), note.layer);
            // Het archief dat Spin zelf schrijft leest terug als het fixture-archief.
            let mut rust_archive = delta_archive(&mut layer, &note).await.unwrap();
            let mut unpacked = rust_archive.gzip(true).await.unwrap();
            let written = read_note(&mut unpacked).await.unwrap().unwrap();
            assert_eq!(
                (&written.parent, &written.content, &written.layer),
                (&note.parent, &note.content, &note.layer)
            );
            let mut written_layer = extract_layer(&mut unpacked).await.unwrap();
            assert_eq!(layer_hash(&mut written_layer).await.unwrap(), note.layer);

            let marker = Temporary::new().unwrap();
            let binary = marker.path().with_extension("sh");
            std::fs::write(
                &binary,
                br##"#!/bin/sh
case "$1" in
image)
 case "$4" in
  '{{.Id}}') printf 'sha256:parent\n';;
  *) printf 'content:parent\n';;
 esac;;
run|rm|commit|exec) printf '%s\n' "$*" >> "$0.calls";;
cp) cat > "$0.copied";;
*) exit 2;;
esac
"##,
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let docker = Docker::new(binary.to_str().unwrap(), "", "").unwrap();
            let calls = binary.with_extension("sh.calls");
            // Random-namen hebben geen punt; de CLI-dubbel gebruikt dezelfde achtervoegsels.
            let copied = binary.with_extension("sh.copied");
            struct Cleanup(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                    let _ = std::fs::remove_file(&self.1);
                    let _ = std::fs::remove_file(&self.2);
                }
            }
            let _cleanup = Cleanup(calls.clone(), copied.clone(), binary.clone());
            let mut snapshot = d::CapsuleSnapshot {
                driver: "docker".into(),
                r#ref: "spin/artifact:child".into(),
                content: "content:wrong".into(),
                ..Default::default()
            };
            assert!(
                import(&docker, &snapshot, &mut fixture)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("different content")
            );
            assert!(
                !calls.exists(),
                "hash validation must happen before creating a build container"
            );
            snapshot.content = note.content;
            import(&docker, &snapshot, &mut fixture).await.unwrap();
            let commands = std::fs::read_to_string(&calls).unwrap();
            assert_eq!(
                commands
                    .lines()
                    .filter(|line| line.starts_with("commit "))
                    .count(),
                1
            );
            assert!(commands.contains("spin.kind=delta-build"));
            assert!(commands.contains("find '/root/private' -mindepth 1"));
            assert!(
                commands
                    .lines()
                    .last()
                    .unwrap()
                    .starts_with("rm -f spin-delta-build")
            );
            assert!(!docker.needs_cleanup());
            let mut received = Temporary::new().unwrap();
            received
                .file
                .write_all(&std::fs::read(copied).unwrap())
                .unwrap();
            let expected = d::json::Value::from_json(include_bytes!(
                "../../tests/fixtures/archive/expected.json"
            ))
            .unwrap();
            assert_eq!(
                layer_hash(&mut received).await.unwrap(),
                expected
                    .as_object()
                    .unwrap()
                    .get("filtered_hash")
                    .unwrap()
                    .as_str()
                    .unwrap()
            );
        });
    }
}
