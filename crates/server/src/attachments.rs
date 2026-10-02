//! Bijlagebytes en metadata worden afzonderlijk gepubliceerd met dezelfde identiteit.
use super::*;
use d::try_string;
use spin_core::validation::text;
use spin_store::{BlobReply, BlobRequest};
pub(crate) fn media_type(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(b"%PDF-") {
        Ok("application/pdf")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Ok("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Ok("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Ok("image/webp")
    } else {
        Err(Error::Http(
            400,
            "only PDF, PNG, JPEG, WebP and GIF attachments are supported",
        ))
    }
}
pub(crate) fn metadata(
    id: &str,
    job: &str,
    filename: &str,
    operator: &str,
    size: i64,
) -> Result<d::CreateJobAttachmentRequest> {
    let mut name = String::new();
    for c in filename
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
    {
        if name.len() + c.len_utf8() > 180 {
            break;
        }
        name.try_reserve(c.len_utf8())
            .map_err(|_| d::Error::OutOfMemory)?;
        name.push(c);
    }
    if matches!(name.as_str(), "" | "." | "..") || !(1..=15 << 20).contains(&size) {
        return Err(Error::Http(400, "invalid attachment filename or size"));
    }
    Ok(d::CreateJobAttachmentRequest {
        id: try_string(id)?,
        job_id: try_string(job)?,
        name,
        operator: try_string(operator)?,
        size,
        ..Default::default()
    })
}
pub(crate) fn complete_metadata(
    mut value: d::CreateJobAttachmentRequest,
    digest: &str,
    media: &str,
) -> Result<d::CreateJobAttachmentRequest> {
    let extension = match media {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return Err(Error::Http(400, "unsupported attachment type")),
    };
    value.sha256 = try_string(digest.strip_prefix("sha256:").unwrap_or(digest))?;
    value.media_type = try_string(media)?;
    value.capsule_path = text(format_args!(
        "/spin/job-attachments/{}-{}.{}",
        value.id,
        d::deliverable_slug(&value.name)?,
        extension
    ))?;
    Ok(value)
}
fn multipart<'a>(req: &'a Request<'_>) -> Result<(String, &'a [u8])> {
    let content = req.header("Content-Type");
    if !content.starts_with("multipart/form-data;") {
        return Err(Error::Http(400, "multipart file required"));
    }
    let boundary = content
        .split(';')
        .find_map(|part| part.trim().strip_prefix("boundary="))
        .map(|s| s.trim_matches('"'))
        .filter(|s| !s.is_empty() && s.len() <= 70 && s.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or(Error::Http(400, "invalid multipart boundary"))?;
    let marker = text(format_args!("--{boundary}"))?;
    let mut at = 0;
    while req.body.get(at..at + marker.len()) == Some(marker.as_bytes()) {
        at += marker.len();
        if req.body.get(at..at + 2) == Some(b"--") {
            break;
        }
        if req.body.get(at..at + 2) != Some(b"\r\n") {
            return Err(Error::Http(400, "invalid multipart delimiter"));
        }
        at += 2;
        let end = req
            .body
            .get(at..)
            .and_then(|s| s.windows(4).position(|p| p == b"\r\n\r\n"))
            .filter(|n| *n <= 8192)
            .ok_or(Error::Http(400, "invalid multipart headers"))?
            + at;
        let head = core::str::from_utf8(&req.body[at..end])
            .map_err(|_| Error::Http(400, "invalid multipart header encoding"))?;
        let separator = text(format_args!("\r\n{marker}"))?;
        let start = end + 4;
        let end = req.body[start..]
            .windows(separator.len())
            .position(|p| p == separator.as_bytes())
            .ok_or(Error::Http(400, "unterminated multipart body"))?
            + start;
        if let Some(disposition) = head.lines().find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("content-disposition"))
                .map(|(_, v)| v)
        }) && disposition.split(';').any(|v| v.trim() == "name=\"file\"")
        {
            let name = disposition
                .split(';')
                .find_map(|v| {
                    v.trim()
                        .strip_prefix("filename=\"")
                        .and_then(|v| v.strip_suffix('"'))
                })
                .ok_or(Error::Http(400, "missing attachment filename"))?;
            return Ok((try_string(name)?, &req.body[start..end]));
        }
        at = end + 2;
    }
    Err(Error::Http(400, "multipart field file is required"))
}
impl<P: Persistence> Server<P> {
    pub(crate) fn attachment_route(
        &mut self,
        req: &Request<'_>,
        operator: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let staged = req.path == "/api/job-attachments";
        let job = req
            .path
            .strip_prefix("/api/jobs/")
            .and_then(|p| p.strip_suffix("/attachments"));
        if req.method == "POST" && (staged || job.is_some()) {
            let (name, bytes) = multipart(req)?;
            let media = media_type(bytes)?;
            let id = runtime.next("att")?;
            let meta = metadata(&id, job.unwrap_or(""), &name, operator, bytes.len() as i64)?;
            let reference = text(format_args!("attachment:{id}"))?;
            let BlobReply::Info(info) = self.store.blob(BlobRequest::Put {
                reference: &reference,
                kind: "job-attachment",
                bytes,
            })?
            else {
                return Err(Error::Http(500, "invalid attachment write"));
            };
            let result = self
                .store
                .create_job_attachment(complete_metadata(meta, &info.digest, media)?, now);
            if result.is_err() {
                let _ = self.store.blob(BlobRequest::Delete(&reference));
            }
            return Ok(Some(Response::json(201, &result?)?));
        }
        let Some(id) = req.path.strip_prefix("/api/job-attachments/") else {
            return Ok(None);
        };
        let reference = text(format_args!("attachment:{id}"))?;
        let response = match req.method {
            "GET" | "HEAD" => {
                let attachment = self.store.job_attachment(id, operator)?.try_clone()?;
                let mut response = Response::empty(200)?;
                response.body = self.store.read_blob(&reference, 15 << 20)?;
                if response.body.len() as i64 != attachment.size {
                    return Err(Error::Http(500, "attachment size changed"));
                }
                let hash = spin_security::sha256(&response.body);
                let mut digest = String::new();
                digest
                    .try_reserve_exact(64)
                    .map_err(|_| d::Error::OutOfMemory)?;
                for b in hash {
                    digest.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
                    digest.push(char::from(b"0123456789abcdef"[usize::from(b & 15)]));
                }
                if !digest.eq_ignore_ascii_case(&attachment.sha256) {
                    return Err(Error::Http(500, "attachment checksum changed"));
                }
                response.header("Content-Type", &attachment.media_type)?;
                response.header(
                    "Content-Disposition",
                    &text(format_args!(
                        "inline; filename=\"{}\"",
                        d::deliverable_slug(&attachment.name)?
                    ))?,
                )?;
                response
                    .headers
                    .retain(|(key, _)| key != "Content-Security-Policy");
                response.header(
                    "Content-Security-Policy",
                    "sandbox; default-src 'none'; img-src 'self' data:",
                )?;
                byte_range(req, &mut response)?;
                response
            }
            "DELETE" => {
                let attachment = self.store.delete_staged_job_attachment(id, operator)?;
                self.store.blob(BlobRequest::Delete(&reference))?;
                Response::json(200, &attachment)?
            }
            _ => return Err(Error::Http(405, "method not allowed")),
        };
        Ok(Some(response))
    }
}

impl<P: Persistence> Server<P> {
    /// Reconcile one immutable attachment at a time; ACK, reconnect, and a new capsule
    /// are safe because placement overwrites the same scoped path.
    pub(crate) fn maintain_attachments(
        &mut self,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        use crate::capsules::Action;
        use d::{List, protocol as p};
        let compositions = self.store.running_compositions()?;
        self.attachment_stamps.retain(|key, _| {
            key.split_once(':')
                .is_some_and(|(id, _)| compositions.iter().any(|c| c.id == id))
        });
        for composition in compositions
            .iter()
            .filter(|c| !c.for_login && !c.session_id.is_empty())
        {
            let Some(capsule) = composition
                .runtime
                .as_ref()
                .filter(|r| r.status == "ready" && !r.stop_pending)
            else {
                continue;
            };
            if !self
                .runners
                .iter()
                .any(|p| p.client().id == capsule.client_id && p.is_connected())
                || self
                    .calls
                    .iter()
                    .any(|c| !c.finished && c.action.object() == composition.id)
            {
                continue;
            }
            let session = self.store.session(&composition.session_id)?;
            if session.job_id.is_empty() {
                continue;
            }
            let job = self.store.job(&session.job_id)?;
            let ids = [job.id.try_clone()?, job.forked_from_job_id.try_clone()?];
            for id in ids.iter().filter(|s| !s.is_empty()) {
                for attachment in self.store.job_attachments(id)?.iter() {
                    let key = text(format_args!("{}:{}", composition.id, attachment.id))?;
                    let stamp = text(format_args!(
                        "{}:{}",
                        capsule.container_id, attachment.sha256
                    ))?;
                    if self.attachment_stamps.get(&key) == Some(&stamp) {
                        continue;
                    }
                    if self.attachment_stamps.len() >= 8192
                        && self.attachment_stamps.get(&key).is_none()
                    {
                        return Err(Error::Http(503, "live attachment capacity reached"));
                    }
                    let reference = text(format_args!("attachment:{}", attachment.id))?;
                    let bytes = self.store.read_blob(&reference, 15 << 20)?;
                    if bytes.len() as i64 != attachment.size
                        || spin_security::digest_hex(&bytes)? != attachment.sha256
                    {
                        return Err(Error::Http(500, "attachment checksum changed"));
                    }
                    let mut attachments = List::new();
                    attachments.push(p::AttachmentPayload {
                        target_path: attachment.capsule_path.try_clone()?,
                        data: d::Bytes(Some(bytes)),
                    })?;
                    let wait = self.enqueue_call(
                        Action::Attachment {
                            composition: composition.id.try_clone()?,
                            key,
                            stamp,
                        },
                        &capsule.client_id,
                        p::METHOD_INJECT_ATTACHMENTS,
                        &p::InjectAttachmentsPayload {
                            runtime: capsule.try_clone()?,
                            attachments,
                        },
                        now,
                        random,
                    )?;
                    self.detach_capsule(wait);
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

fn byte_range(req: &Request<'_>, response: &mut Response) -> Result {
    response.header("Accept-Ranges", "bytes")?;
    let range = req.header("Range");
    // A server may ignore Range; multipart ranges and unrecognized validators get
    // the complete representation. PDF viewers normally request a single interval.
    if req.method != "GET"
        || range.is_empty()
        || !req.header("If-Range").is_empty()
        || range.contains(',')
    {
        return Ok(());
    }
    let total = response.body.len();
    let parse = || -> Option<(usize, usize)> {
        let (start, end) = range.strip_prefix("bytes=")?.split_once('-')?;
        if start.is_empty() {
            let count = end.parse::<usize>().ok()?;
            if count == 0 || total == 0 {
                return None;
            }
            return Some((total.saturating_sub(count), total));
        }
        let start = start.parse::<usize>().ok()?;
        let end = if end.is_empty() {
            total
        } else {
            end.parse::<usize>().ok()?.saturating_add(1).min(total)
        };
        (start < end && start < total).then_some((start, end))
    };
    if let Some((start, end)) = parse() {
        response.status = 206;
        response.header(
            "Content-Range",
            &text(format_args!("bytes {start}-{}/{total}", end - 1))?,
        )?;
        response.body.drain(..start);
        response.body.truncate(end - start);
    } else {
        response.status = 416;
        response.header("Content-Range", &text(format_args!("bytes */{total}"))?)?;
        response.body.clear();
    }
    Ok(())
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod range_tests {
    use super::*;
    #[test]
    fn pdf_ranges_preserve_offsets_suffixes_and_unsatisfiable_lengths() {
        for (range, status, bytes) in [
            ("bytes=2-5", 206, &b"2345"[..]),
            ("bytes=-3", 206, &b"789"[..]),
            ("bytes=8-", 206, &b"89"[..]),
            ("bytes=50-60", 416, &b""[..]),
            ("bytes=5-2", 416, &b""[..]),
        ] {
            let headers = [("Range", range)];
            let req = Request {
                method: "GET",
                path: "/",
                raw_query: "",
                headers: &headers,
                body: &[],
                peer: "",
                secure: false,
            };
            let mut response = Response::empty(200).unwrap();
            response.body.extend_from_slice(b"0123456789");
            byte_range(&req, &mut response).unwrap();
            assert_eq!(response.status, status);
            assert_eq!(response.body, bytes);
            assert!(
                response
                    .headers
                    .iter()
                    .any(|(k, v)| k == "Content-Range" && v.ends_with("/10"))
            );
        }
    }
}
