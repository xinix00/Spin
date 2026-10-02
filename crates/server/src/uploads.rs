//! Centrale blobs reizen in korte HTTP-verzoeken; hashwerk geeft na iedere chunk terug.
use super::*;
use d::try_string;
use spin_core::{
    upload::{Assembler, Prepared},
    validation::text,
};
use spin_store::{BlobReply, BlobRequest};
const CHUNK: usize = 1 << 20;
const LIFETIME: u64 = 6 * 3600 * 1_000_000_000;
/// Een HTTP-verzoek wacht op incrementele verificatie, zonder de actor te bezetten.
pub struct UploadWait {
    id: String,
}
pub(crate) struct Upload {
    pub(super) id: String,
    object: i64,
    owner: String,
    attachment: Option<d::CreateJobAttachmentRequest>,
    media: String,
    kind: String,
    name: String,
    reference: String,
    size: u64,
    expires: u64,
    assembler: Assembler,
    hash: spin_security::Sha256,
    hashed: u64,
    completing: bool,
    response: Option<Response>,
    published: Option<spin_store::BlobInfo>,
    created: Option<d::JobAttachment>,
    failure: Option<Error>,
}
fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .as_object()
        .and_then(|o| o.get(key))
        .unwrap_or(&Value::Null)
}
impl Upload {
    fn status(&self, status: u16) -> Result<Response> {
        Response::json(
            status,
            &http::object(&[
                ("id", Value::string(&self.id)?),
                ("kind", Value::string(&self.kind)?),
                ("name", Value::string(&self.name)?),
                ("size", Value::uint(self.size)),
                ("offset", Value::uint(self.assembler.offset())),
                ("chunk_size", Value::uint(CHUNK as u64)),
                ("parallel", Value::uint(4)),
                (
                    "expires_at",
                    Timestamp::from_time(d::Time(self.expires))?.to_value()?,
                ),
            ])?,
        )
    }
}
impl<P: Persistence> Server<P> {
    pub(crate) fn upload_route(
        &mut self,
        req: &Request<'_>,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        let upload_path = req.path == "/api/uploads" || req.path.starts_with("/api/uploads/");
        let snapshot = req.path.strip_prefix("/api/snapshots/");
        let bundle = req.path.strip_prefix("/api/blobs/");
        if !upload_path && snapshot.is_none() && bundle.is_none() {
            return Ok(None);
        }
        let worker = self.valid_worker(req);
        if (snapshot.is_some() || bundle.is_some()) && !worker {
            return Err(Error::Http(401, "runner authorization required"));
        }
        let identity = if worker {
            None
        } else {
            Some(
                self.identity(req, now)?
                    .ok_or(Error::Http(401, "authentication required"))?,
            )
        };
        if let Some((_, session)) = &identity
            && req.is_mutation()
        {
            auth::check_csrf(req, session)?;
        }
        let owner = identity
            .as_ref()
            .map_or("", |(_, session)| session.token_hash.as_str());
        if worker && !req.header("Origin").is_empty() {
            return Err(Error::Http(
                403,
                "runner endpoint does not accept browser origins",
            ));
        }
        if let Some(reference) = snapshot.or(bundle) {
            if req.method != "GET" {
                return Err(Error::Http(405, "method not allowed"));
            }
            if reference.is_empty()
                || reference.contains('/')
                || (bundle.is_some() && !reference.starts_with("bundle:"))
            {
                return Err(Error::Http(400, "invalid blob reference"));
            }
            let offset = req
                .query("offset")?
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|n| *n >= 0)
                .ok_or(Error::Http(400, "a non-negative offset is required"))?;
            let reference = if snapshot.is_some() {
                text(format_args!("snapshot:{reference}"))?
            } else {
                try_string(reference)?
            };
            let BlobReply::Chunk(bytes, info) = self.store.blob(BlobRequest::Chunk {
                reference: &reference,
                offset,
            })?
            else {
                return Err(Error::Http(500, "invalid blob storage result"));
            };
            let mut response = Response::empty(if bytes.is_empty() { 416 } else { 200 })?;
            response.header("X-Spin-Size", &text(format_args!("{}", info.size))?)?;
            response.header("X-Spin-Digest", &info.digest)?;
            response.header("Content-Type", "application/octet-stream")?;
            response.body = bytes;
            return Ok(Some(Outcome::Response(response)));
        }
        let time = now.time()?.0;
        if req.path == "/api/uploads" {
            if req.method != "POST" {
                return Err(Error::Http(405, "method not allowed"));
            }
            let value = Value::from_json(req.body)?;
            let kind = field(&value, "kind").as_str().unwrap_or("").trim();
            let size = field(&value, "size").as_i64().unwrap_or(0);
            let maximum = match kind {
                "restore"
                    if identity
                        .as_ref()
                        .is_some_and(|(user, _)| user.role == d::USER_ADMIN) =>
                {
                    64_i64 << 30
                }
                "snapshot" if worker => 64_i64 << 30,
                "bundle" if worker => 25 << 20,
                "attachment" if !worker => 15 << 20,
                "restore" => return Err(Error::Http(403, "admin role required")),
                _ => return Err(Error::Http(400, "invalid upload kind")),
            };
            if kind == "restore" {
                self.restore_available()?;
            }
            if size <= 0 || size > maximum {
                return Err(Error::Http(413, "invalid upload size"));
            }
            if self.uploads.len() >= 16
                && let Some(index) = self.uploads.iter().position(|u| u.response.is_some())
            {
                self.uploads.remove(index);
            }
            if self.uploads.len() >= 16 {
                return Err(Error::Http(503, "upload capacity reached"));
            }
            self.uploads
                .try_reserve(1)
                .map_err(|_| d::Error::OutOfMemory)?;
            let snapshot = d::CapsuleSnapshot::from_value(field(&value, "snapshot"))?;
            if kind == "snapshot"
                && (snapshot.digest.trim().is_empty()
                    || snapshot.digest.len() > 256
                    || snapshot.digest.contains('/'))
            {
                return Err(Error::Http(400, "snapshot digest is required"));
            }
            let name = field(&value, "name").as_str().unwrap_or("").trim();
            if name.len() > 255 {
                return Err(Error::Http(400, "upload name is too long"));
            }
            if kind == "restore" && name.is_empty() {
                return Err(Error::Http(400, "backup filename is required"));
            }
            let mut upload = Upload {
                id: runtime.next("upl")?,
                object: 0,
                owner: try_string(owner)?,
                attachment: if kind == "attachment" {
                    Some(crate::attachments::metadata(
                        &runtime.next("att")?,
                        field(&value, "job_id").as_str().unwrap_or(""),
                        name,
                        identity
                            .as_ref()
                            .map_or("", |(user, _)| user.username.as_str()),
                        size,
                    )?)
                } else {
                    None
                },
                media: String::new(),
                kind: try_string(kind)?,
                name: try_string(if name.is_empty() {
                    &snapshot.r#ref
                } else {
                    name
                })?,
                reference: if kind == "snapshot" {
                    text(format_args!("snapshot:{}", snapshot.digest.trim()))?
                } else {
                    String::new()
                },
                size: size as u64,
                expires: time.saturating_add(LIFETIME),
                assembler: Assembler::new(size as u64),
                hash: spin_security::Sha256::new(),
                hashed: 0,
                completing: false,
                response: None,
                published: None,
                created: None,
                failure: None,
            };
            if let Some(meta) = &upload.attachment {
                upload.reference = text(format_args!("attachment:{}", meta.id))?;
            }
            // Allocate the response before the durable object exists.
            let response = upload.status(201)?;
            let BlobReply::Upload(object) = self.store.blob(BlobRequest::Begin {
                kind: if kind == "restore" {
                    "backup"
                } else if kind == "snapshot" {
                    "docker-snapshot"
                } else if kind == "attachment" {
                    "job-attachment"
                } else {
                    "deliverable-bundle"
                },
                size,
            })?
            else {
                return Err(Error::Http(500, "invalid upload handle"));
            };
            upload.object = object;
            self.uploads.push(upload);
            return Ok(Some(Outcome::Response(response)));
        }
        let path = req
            .path
            .strip_prefix("/api/uploads/")
            .ok_or(Error::Http(404, "not found"))?;
        let (id, completion) = path
            .strip_suffix("/complete")
            .map_or((path, false), |p| (p, true));
        let index = self
            .uploads
            .iter()
            .position(|u| u.id == id && u.expires > time && u.owner == owner)
            .ok_or(Error::Http(404, "upload not found or expired"))?;
        let upload = &mut self.uploads[index];
        if completion && req.method == "POST" {
            if upload.response.is_some() {
                return Ok(Some(Outcome::Upload(UploadWait {
                    id: upload.id.try_clone()?,
                })));
            }
            if upload.assembler.offset() != upload.size {
                return Err(Error::Http(409, "upload is incomplete"));
            }
            let wait = UploadWait {
                id: upload.id.try_clone()?,
            };
            if !upload.completing {
                upload
                    .assembler
                    .finish()
                    .map_err(|_| Error::Http(409, "upload is incomplete"))?;
                upload.completing = true;
            }
            upload.expires = time.saturating_add(LIFETIME);
            return Ok(Some(Outcome::Upload(wait)));
        }
        if completion {
            return Err(Error::Http(405, "method not allowed"));
        }
        let response = match req.method {
            "GET" => upload.status(200)?,
            "PUT" => {
                if upload.completing {
                    return Err(Error::Http(409, "upload is completing"));
                }
                let length = req.body.len();
                if length == 0 || length > CHUNK {
                    return Err(Error::Http(413, "upload chunks must contain at most 1 MiB"));
                }
                let offset = req
                    .header("X-Spin-Upload-Offset")
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| Error::Http(400, "invalid upload offset"))?;
                if offset % CHUNK as u64 != 0
                    || (length != CHUNK && offset.checked_add(length as u64) != Some(upload.size))
                {
                    return Err(Error::Http(
                        400,
                        "chunks must be aligned and full except the last",
                    ));
                }
                match upload
                    .assembler
                    .prepare(offset, length as u64)
                    .map_err(|_| Error::Http(409, "upload offset mismatch"))?
                {
                    Prepared::Committed(_) => {}
                    Prepared::Write(ticket) => {
                        let result = self.store.blob(BlobRequest::Write {
                            object: upload.object,
                            offset: offset as i64,
                            bytes: req.body,
                        });
                        upload
                            .assembler
                            .complete(ticket, length as u64, result.is_ok())
                            .map_err(|_| Error::Http(500, "invalid upload write completion"))?;
                        result?;
                    }
                }
                upload.expires = time.saturating_add(LIFETIME);
                upload.status(200)?
            }
            "DELETE" => {
                self.store.blob(BlobRequest::Abandon(upload.object))?;
                self.uploads.remove(index);
                Response::empty(204)?
            }
            _ => return Err(Error::Http(405, "method not allowed")),
        };
        Ok(Some(Outcome::Response(response)))
    }
    /// Hash maximaal één MiB per actorronde; andere HTTP-verzoeken blijven lopen.
    pub fn maintain_uploads(&mut self, now: &Timestamp) -> Result {
        let time = now.time()?.0;
        for index in 0..self.uploads.len() {
            if self.uploads[index].expires <= time {
                self.store
                    .blob(BlobRequest::Abandon(self.uploads[index].object))?;
                self.uploads.remove(index);
                return Ok(());
            }
            let upload = &mut self.uploads[index];
            if upload.response.is_some() {
                continue;
            }
            if let Some(error) = &upload.failure {
                upload.response = Some(error.response()?);
                return Ok(());
            }
            if upload.hashed < upload.assembler.offset() {
                let BlobReply::Bytes(bytes) = self.store.blob(BlobRequest::Pending {
                    object: upload.object,
                    offset: upload.hashed as i64,
                })?
                else {
                    return Err(Error::Http(500, "invalid upload chunk"));
                };
                if upload.hashed == 0 && upload.attachment.is_some() {
                    match crate::attachments::media_type(&bytes) {
                        Ok(media) => upload.media = try_string(media)?,
                        Err(error) => {
                            self.store.blob(BlobRequest::Abandon(upload.object))?;
                            upload.response = Some(error.response()?);
                            upload.completing = true;
                            return Ok(());
                        }
                    }
                }
                upload.hash.update(&bytes);
                upload.hashed += bytes.len() as u64;
                self.uploads.rotate_left(1);
                return Ok(());
            }
            if upload.completing && upload.hashed == upload.size {
                let mut digest = try_string("sha256:")?;
                digest
                    .try_reserve_exact(64)
                    .map_err(|_| d::Error::OutOfMemory)?;
                // A later allocation can fail; retry must retain the completed hash.
                let hash = upload.hash.clone().finish();
                for byte in hash {
                    digest.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
                    digest.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
                }
                if upload.reference.is_empty() {
                    upload.reference = text(format_args!("bundle:{}", &digest[7..]))?;
                }
                if upload.published.is_none() {
                    match self.store.blob(BlobRequest::Publish {
                        object: upload.object,
                        reference: &upload.reference,
                        digest: &digest,
                    }) {
                        Ok(BlobReply::Info(info)) => upload.published = Some(info),
                        Ok(_) => {
                            upload.failure = Some(Error::Http(500, "invalid upload publication"))
                        }
                        Err(error) => upload.failure = Some(error.into()),
                    }
                }
                if upload.failure.is_some() {
                    return Ok(());
                }
                if upload.kind == "restore" {
                    let id = upload.id.try_clone()?;
                    let object = upload.object;
                    match self.start_restore(&id, object, now) {
                        Ok(response) => self.uploads[index].response = Some(response),
                        Err(error) => self.uploads[index].failure = Some(error),
                    }
                    return Ok(());
                }
                let info = upload
                    .published
                    .as_ref()
                    .ok_or(Error::Http(500, "missing upload publication"))?;
                if upload.attachment.is_some() {
                    if upload.created.is_none() {
                        let metadata = upload
                            .attachment
                            .as_ref()
                            .ok_or(Error::Http(500, "missing attachment metadata"))?
                            .try_clone()?;
                        let result = self.store.create_job_attachment(
                            crate::attachments::complete_metadata(
                                metadata,
                                &info.digest,
                                &upload.media,
                            )?,
                            now,
                        );
                        match result {
                            Ok(attachment) => upload.created = Some(attachment),
                            Err(error) => {
                                let _ = self.store.blob(BlobRequest::Delete(&info.reference));
                                upload.failure = Some(error.into());
                                return Ok(());
                            }
                        }
                    }
                    upload.response = Some(Response::json(
                        200,
                        upload
                            .created
                            .as_ref()
                            .ok_or(Error::Http(500, "missing attachment publication"))?,
                    )?);
                } else {
                    upload.response = Some(Response::json(
                        200,
                        &http::object(&[
                            ("ref", Value::string(&info.reference)?),
                            ("digest", Value::string(&info.digest)?),
                            ("size", info.size.to_value()?),
                        ])?,
                    )?);
                }
                return Ok(());
            }
        }
        Ok(())
    }
    /// De upload blijft bij de app wanneer de aanvragende HTTP-verbinding verdwijnt.
    pub fn poll_upload(&mut self, wait: &UploadWait) -> Result<Option<Response>> {
        let index = self
            .uploads
            .iter()
            .position(|u| u.id == wait.id)
            .ok_or(Error::Http(404, "upload not found or expired"))?;
        if self.uploads[index].response.is_none() {
            return Ok(None);
        }
        let upload = &self.uploads[index];
        if let Some(response) = &upload.response {
            let mut headers = d::List::new();
            for (key, value) in response.headers.iter() {
                headers.push((key.try_clone()?, value.try_clone()?))?;
            }
            Ok(Some(Response {
                status: response.status,
                headers,
                body: response.body.try_clone()?,
            }))
        } else {
            Ok(None)
        }
    }
}
