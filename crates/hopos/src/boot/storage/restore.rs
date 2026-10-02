//! Upload/extraction files belong to the native owner; never to the replicated database.
use super::*;
use replica_core::local::File;
use spin_core::backup::{Archive, CHUNK, Decoder, MAX_DATABASE, Source};
use spin_domain::{TryClone, try_string};
use spin_store::{
    BlobInfo,
    backup::{PortableState, RestoreReply as R, RestoreRequest as Q},
};
const UPLOAD: &str = "spin-restore.upload";
const STAGED: &str = "spin-restore.sqlite";
pub(super) struct RawUpload {
    id: i64,
    size: u64,
    ready: bool,
}
pub(super) struct Import {
    decoder: Option<Decoder>,
    size: u64,
    written: u64,
    key: String,
    checked: bool,
    object: Option<spin_persistence::BlobInfo>,
    after: String,
    offset: i64,
    hash: spin_security::Sha256,
    ready: bool,
}
struct Input<'a, B: Storage> {
    backend: &'a mut B,
    size: u64,
}
fn invalid() -> spin_store::Error {
    spin_store::Error::Conflict("invalid backup archive")
}
fn name(value: &str) -> spin_store::Result<Name> {
    Name::new(value).map_err(replica_error)
}
fn block<B: Storage>(
    backend: &mut B,
    path: &str,
    offset: u64,
    length: usize,
) -> spin_store::Result<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(length)
        .map_err(|_| spin_domain::Error::OutOfMemory)?;
    out.resize(length, 0);
    let mut file = File::open(backend, &name(path)?, false).map_err(replica_error)?;
    file.read(offset, &mut out).map_err(replica_error)?;
    file.close().map_err(replica_error)?;
    Ok(out)
}
impl<B: Storage> Source for Input<'_, B> {
    fn size(&self) -> u64 {
        self.size
    }
    fn read(&mut self, offset: u64, length: usize) -> spin_domain::Fallible<Vec<u8>> {
        block(self.backend, UPLOAD, offset, length).map_err(|error| match error {
            spin_store::Error::Data(error) => error,
            _ => spin_core::validation::invalid("backup", "could not read staged upload"),
        })
    }
}
pub(super) fn raw_request(request: &BlobRequest<'_>) -> bool {
    match request {
        BlobRequest::Begin { kind, .. } => *kind == "backup",
        BlobRequest::Write { object, .. }
        | BlobRequest::Pending { object, .. }
        | BlobRequest::Publish { object, .. }
        | BlobRequest::Abandon(object) => *object < 0,
        _ => false,
    }
}
fn staged<T>(
    heap: &mut [u64],
    backend: &mut Backend<'_>,
    check: bool,
    f: impl FnOnce(&mut Database<'_, '_, Backend<'_>>) -> spin_persistence::Result<T>,
) -> spin_store::Result<T> {
    // SAFETY: The owning boot stack opens one engine and closes it before returning.
    let mut engine = unsafe { replica_sqlite::Engine::initialize(heap, backend) }
        .map_err(Error::from)
        .map_err(store_error)?;
    let connection = engine
        .open(c"spin-restore.sqlite")
        .map_err(Error::from)
        .map_err(store_error)?;
    let mut db = if check {
        Database::staged(connection)
    } else {
        Database::read_only(connection)
    }
    .map_err(store_error)?;
    f(&mut db).map_err(store_error)
}
impl Owner<'_> {
    pub(super) fn raw_blob(&mut self, request: BlobRequest<'_>) -> spin_store::Result<BlobReply> {
        if self.poisoned {
            return Err(spin_store::Error::StorageUncertain(10));
        }
        if let BlobRequest::Begin { size, .. } = request {
            if self.raw_upload.is_some() || self.importing.is_some() || self.restore_fetch.is_some()
            {
                return Err(spin_store::Error::Conflict("backup upload already active"));
            }
            if size <= 0 || size as u64 > MAX_DATABASE + (1 << 20) {
                return Err(invalid());
            }
            let id = self.next_upload;
            self.next_upload = id.checked_sub(1).ok_or_else(invalid)?;
            let mut file =
                File::open(&mut self.backend, &name(UPLOAD)?, true).map_err(replica_error)?;
            file.truncate(size as u64).map_err(replica_error)?;
            file.close().map_err(replica_error)?;
            self.raw_upload = Some(RawUpload {
                id,
                size: size as u64,
                ready: false,
            });
            return Ok(BlobReply::Upload(id));
        }
        let id = match &request {
            BlobRequest::Write { object, .. }
            | BlobRequest::Pending { object, .. }
            | BlobRequest::Publish { object, .. }
            | BlobRequest::Abandon(object) => *object,
            _ => return Err(invalid()),
        };
        if matches!(request, BlobRequest::Abandon(_))
            && self.raw_upload.as_ref().is_none_or(|u| u.id != id)
        {
            return Ok(BlobReply::Done);
        }
        let upload = self
            .raw_upload
            .as_mut()
            .filter(|u| u.id == id)
            .ok_or(spin_store::Error::NotFound)?;
        match request {
            BlobRequest::Write { offset, bytes, .. } => {
                if upload.ready
                    || offset < 0
                    || offset % (1 << 20) != 0
                    || bytes.is_empty()
                    || bytes.len() > 1 << 20
                    || (offset as u64)
                        .checked_add(bytes.len() as u64)
                        .is_none_or(|n| n > upload.size)
                    || (bytes.len() != 1 << 20 && offset as u64 + bytes.len() as u64 != upload.size)
                {
                    return Err(invalid());
                }
                let mut file =
                    File::open(&mut self.backend, &name(UPLOAD)?, false).map_err(replica_error)?;
                file.write(offset as u64, bytes).map_err(replica_error)?;
                file.close().map_err(replica_error)?;
                Ok(BlobReply::Done)
            }
            BlobRequest::Pending { offset, .. } => {
                if offset < 0 || offset as u64 >= upload.size || offset % (1 << 20) != 0 {
                    return Err(invalid());
                }
                let bytes = block(
                    &mut self.backend,
                    UPLOAD,
                    offset as u64,
                    (upload.size - offset as u64).min(1 << 20) as usize,
                )?;
                Ok(BlobReply::Bytes(bytes))
            }
            BlobRequest::Publish {
                reference, digest, ..
            } => {
                let info = BlobInfo {
                    reference: try_string(reference)?,
                    digest: try_string(digest)?,
                    kind: try_string("backup")?,
                    size: upload.size as i64,
                };
                let mut file =
                    File::open(&mut self.backend, &name(UPLOAD)?, false).map_err(replica_error)?;
                file.sync().map_err(replica_error)?;
                file.close().map_err(replica_error)?;
                upload.ready = true;
                Ok(BlobReply::Info(info))
            }
            BlobRequest::Abandon(_) => {
                if self.importing.is_some() {
                    return Err(spin_store::Error::Conflict("backup is being restored"));
                }
                self.raw_upload = None;
                self.backend
                    .remove(name(UPLOAD)?.cstr().map_err(replica_error)?, true)
                    .map_err(Error::from)
                    .map_err(store_error)?;
                Ok(BlobReply::Done)
            }
            _ => Err(invalid()),
        }
    }
    pub(super) fn restore_step(&mut self, request: Q) -> spin_store::Result<R> {
        if self.poisoned {
            return Err(spin_store::Error::StorageUncertain(10));
        }
        match request {
            Q::Abort => {
                self.importing = None;
                self.restore_fetch = None;
                self.raw_upload = None;
                for path in [
                    UPLOAD,
                    STAGED,
                    "spin-restore.sqlite-journal",
                    "spin-restore.sqlite.replica-restore-data",
                    "spin-restore.sqlite.replica-restoring",
                ] {
                    self.backend
                        .remove(name(path)?.cstr().map_err(replica_error)?, true)
                        .map_err(Error::from)
                        .map_err(store_error)?;
                }
                Ok(R::Done)
            }
            Q::Begin(id) => {
                if self.importing.is_some()
                    || self.exporting.is_some()
                    || self.restore_fetch.is_some()
                {
                    return Err(spin_store::Error::Conflict(
                        "another backup or restore is active",
                    ));
                }
                let upload = self
                    .raw_upload
                    .as_ref()
                    .filter(|u| u.id == id && u.ready)
                    .ok_or_else(invalid)?;
                let mut source = Input {
                    backend: &mut self.backend,
                    size: upload.size,
                };
                let (decoder, size, key) = if source.read(0, 16)? == b"SQLite format 3\0" {
                    (None, upload.size, String::new())
                } else {
                    let archive = Archive::open(&mut source)?;
                    let mut decoder = Decoder::new(archive.key);
                    let mut key = Vec::new();
                    key.try_reserve_exact(archive.key.size() as usize)
                        .map_err(|_| spin_domain::Error::OutOfMemory)?;
                    while let Some(part) = decoder.next(&mut source)? {
                        key.extend_from_slice(&part);
                    }
                    let key = String::from_utf8(key).map_err(|_| invalid())?;
                    Cipher::from_encoded(key.trim())?;
                    (
                        Some(Decoder::new(archive.database)),
                        archive.database.size(),
                        try_string(key.trim())?,
                    )
                };
                if size > MAX_DATABASE {
                    return Err(invalid());
                }
                let mut file =
                    File::open(&mut self.backend, &name(STAGED)?, true).map_err(replica_error)?;
                file.truncate(0).map_err(replica_error)?;
                file.close().map_err(replica_error)?;
                self.importing = Some(Import {
                    decoder,
                    size,
                    written: 0,
                    key,
                    checked: false,
                    object: None,
                    after: String::new(),
                    offset: 0,
                    hash: spin_security::Sha256::new(),
                    ready: false,
                });
                Ok(R::Progress(0, size))
            }
            Q::Replica { generation, at } => {
                if self.importing.is_some()
                    || self.exporting.is_some()
                    || self.raw_upload.is_some()
                    || self.restore_fetch.is_some()
                {
                    return Err(spin_store::Error::Conflict(
                        "another backup or restore is active",
                    ));
                }
                if self.bucket.is_none() {
                    return Err(spin_store::Error::Conflict("Replica is not configured"));
                }
                replica_core::marker::generation_time(&generation).map_err(replica_error)?;
                self.restore_fetch = Some((generation, at.as_ref().map(time).transpose()?));
                Ok(R::Progress(0, 0))
            }
            Q::Step => {
                if let Some((generation, at)) = self.restore_fetch.take() {
                    self.fetch_restore(&generation, at)?;
                    return Ok(R::Progress(0, 0));
                }
                self.restore_advance()
            }
        }
    }
    fn fetch_restore(&mut self, generation: &str, at: Option<Time>) -> spin_store::Result {
        let key = self.cipher.portable_key()?;
        let bucket = self.bucket.as_mut().ok_or_else(invalid)?;
        let arena = self.heap;
        let wait = self.backend.wait;
        replica_core::archive::fetch(
            &mut self.backend,
            bucket,
            replica_core::archive::Fetch {
                namespace: &self.namespace,
                generation,
                at,
                live: name("spin.sqlite")?,
                destination: name(STAGED)?,
                page_limit: 1 << 20,
            },
            |backend, path| {
                let mut heap = arena.take(wait)?;
                // SAFETY: This exclusive arena loan outlives the staged verification engine.
                let mut engine = unsafe { replica_sqlite::Engine::initialize(&mut heap, backend) }?;
                let mut db = engine.open(path.cstr()?)?;
                let mut query = db.prepare(c"PRAGMA quick_check")?;
                if !query.step()? || query.column(0)? != replica_sqlite::Value::Text("ok") {
                    return Err(replica_core::Error::Corrupt);
                }
                Ok(())
            },
        )
        .map_err(replica_error)?;
        let mut file =
            File::open(&mut self.backend, &name(STAGED)?, false).map_err(replica_error)?;
        let size = file.size().map_err(replica_error)?;
        file.close().map_err(replica_error)?;
        if size == 0 || size > MAX_DATABASE {
            return Err(invalid());
        }
        staged(
            &mut self
                .heap
                .take(self.backend.wait)
                .map_err(Error::from)
                .map_err(store_error)?,
            &mut self.backend,
            true,
            |_| Ok(()),
        )?;
        self.importing = Some(Import {
            decoder: None,
            size,
            written: size,
            key,
            checked: true,
            object: None,
            after: String::new(),
            offset: 0,
            hash: spin_security::Sha256::new(),
            ready: false,
        });
        Ok(())
    }
    fn restore_advance(&mut self) -> spin_store::Result<R> {
        let import = self.importing.as_mut().ok_or_else(invalid)?;
        let size = self.raw_upload.as_ref().map_or(0, |u| u.size);
        if !import.checked {
            let bytes = if let Some(decoder) = &mut import.decoder {
                decoder.next(&mut Input {
                    backend: &mut self.backend,
                    size,
                })?
            } else if import.written < import.size {
                Some(block(
                    &mut self.backend,
                    UPLOAD,
                    import.written,
                    (import.size - import.written).min(CHUNK as u64) as usize,
                )?)
            } else {
                None
            };
            let mut file =
                File::open(&mut self.backend, &name(STAGED)?, false).map_err(replica_error)?;
            if let Some(bytes) = bytes {
                file.write(import.written, &bytes).map_err(replica_error)?;
                file.close().map_err(replica_error)?;
                import.written += bytes.len() as u64;
                return Ok(R::Progress(import.written, import.size));
            }
            file.sync().map_err(replica_error)?;
            file.close().map_err(replica_error)?;
            let key = staged(
                &mut self
                    .heap
                    .take(self.backend.wait)
                    .map_err(Error::from)
                    .map_err(store_error)?,
                &mut self.backend,
                true,
                |db| {
                    if import.key.is_empty() {
                        if db.read_file("backup/format", 128)? != b"spin-sqlite-backup-v1" {
                            return Err(Error::Invalid("raw database has no portable backup key"));
                        }
                        Ok(Some(db.read_file("backup/master_key", 4096)?))
                    } else {
                        Ok(None)
                    }
                },
            )?;
            if let Some(key) = key {
                import.key = String::from_utf8(key).map_err(|_| invalid())?;
                Cipher::from_encoded(import.key.trim())?;
            }
            import.checked = true;
            return Ok(R::Progress(import.size, import.size));
        }
        if !import.ready {
            if import.object.is_none() {
                import.object = staged(
                    &mut self
                        .heap
                        .take(self.backend.wait)
                        .map_err(Error::from)
                        .map_err(store_error)?,
                    &mut self.backend,
                    false,
                    |db| db.restore_object(&import.after),
                )?;
                import.offset = 0;
                import.hash = spin_security::Sha256::new();
            }
            if let Some(info) = &import.object {
                if info.size < 0 || info.size as u64 > MAX_DATABASE || info.digest.len() != 71 {
                    return Err(invalid());
                }
                if import.offset < info.size {
                    let (bytes, _) = staged(
                        &mut self
                            .heap
                            .take(self.backend.wait)
                            .map_err(Error::from)
                            .map_err(store_error)?,
                        &mut self.backend,
                        false,
                        |db| db.read_blob_chunk(&info.reference, import.offset),
                    )?;
                    import.hash.update(&bytes);
                    import.offset += bytes.len() as i64;
                    return Ok(R::Progress(import.size, import.size));
                }
                let digest = replica_core::manifest::hex(&import.hash.clone().finish());
                if info.digest.strip_prefix("sha256:").map(str::as_bytes) != Some(digest.as_slice())
                {
                    return Err(invalid());
                }
                import.after = info.reference.try_clone()?;
                import.object = None;
                return Ok(R::Progress(import.size, import.size));
            }
            import.ready = true;
        }
        let bytes = staged(
            &mut self
                .heap
                .take(self.backend.wait)
                .map_err(Error::from)
                .map_err(store_error)?,
            &mut self.backend,
            false,
            |db| db.read_file("state", MAX_STATE_BYTES),
        )?;
        Ok(R::Prepared(PortableState {
            json: String::from_utf8(bytes).map_err(|_| invalid())?,
            master_key: try_string(import.key.trim())?,
        }))
    }
    pub(super) fn restore_install(&mut self, state: &PersistedState) -> spin_store::Result {
        if !self.importing.as_ref().is_some_and(|i| i.ready) {
            return Err(invalid());
        }
        // Missing references must fail before live tables change, including an otherwise valid SQLite file.
        staged(
            &mut self
                .heap
                .take(self.backend.wait)
                .map_err(Error::from)
                .map_err(store_error)?,
            &mut self.backend,
            false,
            |db| {
                for (_, attachment) in state.job_attachments.iter() {
                    let reference =
                        spin_core::validation::text(format_args!("attachment:{}", attachment.id))?;
                    let info = db.blob_info(&reference)?;
                    if info.size != attachment.size
                        || info.digest.strip_prefix("sha256:") != Some(attachment.sha256.as_str())
                    {
                        return Err(Error::Invalid("backup attachment content mismatch"));
                    }
                }
                for (_, artifact) in state.artifacts.iter() {
                    if artifact.snapshot.restorable
                        && artifact.snapshot_pruned_at.is_none()
                        && artifact.superseded_by.is_empty()
                    {
                        let reference = spin_core::validation::text(format_args!(
                            "snapshot:{}",
                            artifact.snapshot.digest
                        ))?;
                        db.blob_info(&reference)?;
                    }
                }
                Ok(())
            },
        )?;
        let bytes = self
            .cipher
            .encrypt_state(state, &mut self.entropy)?
            .to_json()?;
        self.execute(Op::Restore(bytes.as_bytes()))?;
        Ok(())
    }
}
