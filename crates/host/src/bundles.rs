//! Visuele bestanden reizen als gecontroleerde ZIP, capsules ontvangen een veilige tar.
use crate::{
    archive::{self, Reader, Temporary},
    blob_client::Client,
    process,
};
use spin_core::{bundle, docker::Docker, validation::text};
use spin_domain::{self as d, try_string};
use std::io::{Read, Seek, SeekFrom, Write};
type Result<T> = std::io::Result<T>;
fn io(e: impl std::fmt::Debug) -> std::io::Error {
    crate::client_net::error(e)
}
pub(crate) async fn create(
    client: &Client,
    docker: &Docker,
    payload: &d::protocol::BundleDeliverablePayload,
) -> Result<d::DeliverableBundle> {
    let command = docker
        .bundle_workspace(&payload.runtime, &payload.path)
        .map_err(io)?;
    let mut tar = Temporary::new()?;
    archive::check(
        process::transfer(
            command,
            None,
            Some(&mut tar.file),
            (bundle::MAX_BYTES + 4 * 1024 * 1024) as u64,
        )
        .await?,
    )?;
    let mut reader = Reader::new(&mut tar.file)?;
    let mut writer = bundle::Writer::default();
    let mut files = 0;
    let mut total = 0;
    let mut folder = false;
    let mut first = String::new();
    let mut index = false;
    while let Some(entry) = reader.next().await? {
        if entry.header.kind == b'5' {
            folder = true;
            continue;
        }
        if entry.header.kind != b'0' {
            continue;
        }
        let name = entry
            .header
            .name
            .strip_prefix("./")
            .unwrap_or(&entry.header.name);
        if !bundle::safe_name(name) {
            return Err(io("invalid bundle entry"));
        }
        total += entry.header.size;
        if total > bundle::MAX_BYTES as u64 || files >= bundle::MAX_FILES {
            return Err(io("bundle exceeds size or file limit"));
        }
        let size = usize::try_from(entry.header.size).map_err(io)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).map_err(io)?;
        bytes.resize(size, 0);
        reader.file.seek(SeekFrom::Start(entry.data))?;
        reader.file.read_exact(&mut bytes)?;
        writer.add(name, &bytes).map_err(io)?;
        if files == 0 {
            first = try_string(name).map_err(io)?;
        }
        index |= name == "index.html";
        files += 1;
    }
    let bytes = writer.finish().map_err(io)?;
    let mut zip = Temporary::new()?;
    zip.file.write_all(&bytes)?;
    drop(bytes);
    let name = payload
        .path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("bundle");
    let saved = client
        .upload(
            "bundle",
            name,
            None,
            &mut zip,
            crate::progress::Progress::NONE,
        )
        .await?;
    let entry = if !folder {
        first
    } else if index {
        try_string("index.html").map_err(io)?
    } else {
        String::new()
    };
    let content_type = if entry.is_empty() {
        String::new()
    } else {
        try_string(bundle::content_type(&entry)).map_err(io)?
    };
    Ok(d::DeliverableBundle {
        r#ref: saved.r#ref,
        digest: saved.digest,
        size: saved.size,
        files: files as i64,
        folder,
        entry,
        content_type,
    })
}
pub(crate) async fn place(
    client: &Client,
    docker: &Docker,
    payload: &d::protocol::PlaceDeliverablePayload,
) -> Result<()> {
    if !payload.bundle.r#ref.starts_with("bundle:")
        || !payload
            .bundle
            .r#ref
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b':')
    {
        return Err(io("invalid bundle reference"));
    }
    let command = docker
        .place_bundle(&payload.runtime, &payload.target, payload.bundle.folder)
        .map_err(io)?;
    let path = text(format_args!("/api/blobs/{}", payload.bundle.r#ref)).map_err(io)?;
    let mut zip = client
        .download(&path, payload.bundle.size, bundle::MAX_BYTES as u64)
        .await?;
    let size = zip.file.metadata()?.len() as usize;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(io)?;
    bytes.resize(size, 0);
    zip.file.read_exact(&mut bytes)?;
    let entries = bundle::entries(&bytes).map_err(io)?;
    if !payload.bundle.folder && entries.len() != 1 {
        return Err(io("single-file bundle contains multiple files"));
    }
    let mut tar = Temporary::new()?;
    for entry in entries {
        let contents = entry.read().map_err(io)?;
        let name = if payload.bundle.folder {
            entry.name
        } else {
            payload.target.rsplit('/').next().unwrap_or("")
        };
        tar.file.write_all(
            &spin_core::archive::Header::regular(name, contents.len() as u64).map_err(io)?,
        )?;
        tar.file.write_all(&contents)?;
        let padding = (512 - contents.len() % 512) % 512;
        tar.file.write_all(&[0; 512][..padding])?;
        crate::executor::next_round().await;
    }
    archive::finish(&mut tar.file)?;
    tar.rewind()?;
    archive::check(
        process::transfer(
            command,
            Some(&mut tar.file),
            None,
            (bundle::MAX_BYTES + 4 * 1024 * 1024) as u64,
        )
        .await?,
    )
}
