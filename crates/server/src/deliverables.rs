//! Previewtokens openen uitsluitend hun revisie, met een ondoorzichtige browserorigin.
use super::*;
use d::{try_push_str, try_string};
use spin_core::{bundle, validation::text};
fn escape(value: &str) -> Result<String> {
    let mut result = String::new();
    for c in value.chars() {
        match c {
            '&' => try_push_str(&mut result, "&amp;")?,
            '<' => try_push_str(&mut result, "&lt;")?,
            '>' => try_push_str(&mut result, "&gt;")?,
            '"' => try_push_str(&mut result, "&quot;")?,
            '\'' => try_push_str(&mut result, "&#39;")?,
            _ => {
                result
                    .try_reserve(c.len_utf8())
                    .map_err(|_| d::Error::OutOfMemory)?;
                result.push(c);
            }
        }
    }
    Ok(result)
}
fn file_url(value: &str) -> Result<String> {
    let mut result = String::new();
    result
        .try_reserve(value.len() * 3)
        .map_err(|_| d::Error::OutOfMemory)?;
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            result.push(char::from(b));
        } else {
            result.push('%');
            result.push(char::from(b"0123456789ABCDEF"[usize::from(b >> 4)]));
            result.push(char::from(b"0123456789ABCDEF"[usize::from(b & 15)]));
        }
    }
    Ok(result)
}
fn preview(response: &mut Response, req: &Request<'_>, mime: &str) -> Result {
    let old = core::mem::take(&mut response.headers);
    for (key, value) in old.into_vec() {
        if !matches!(key.as_str(), "X-Frame-Options" | "Content-Security-Policy") {
            response.headers.push((key, value))?;
        }
    }
    response.header("Content-Type", mime)?;
    response.header("Cross-Origin-Resource-Policy", "cross-origin")?;
    response.header("X-Robots-Tag", "noindex, nofollow")?;
    let host = req.header("Host");
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".:-[]".contains(&b))
    {
        return Err(Error::Http(400, "invalid request host"));
    }
    let policy = if mime.starts_with("application/pdf")
        || (mime.starts_with("image/") && !mime.contains("svg"))
    {
        try_string("default-src 'none'; frame-ancestors 'self'")?
    } else {
        let origin = text(format_args!(
            "{}://{host}",
            if req.secure { "https" } else { "http" }
        ))?;
        text(format_args!(
            "sandbox allow-scripts allow-forms allow-modals; default-src {origin} data: blob: 'unsafe-inline' 'unsafe-eval'; script-src {origin} data: blob: 'unsafe-inline' 'unsafe-eval'; style-src {origin} data: blob: https: 'unsafe-inline'; font-src {origin} data: blob: https:; img-src {origin} data: blob: https:; connect-src 'none'; form-action 'none'; frame-ancestors 'self'"
        ))?
    };
    response.header("Content-Security-Policy", &policy)
}
impl<P: Persistence> Server<P> {
    pub(crate) fn public_deliverable(
        &mut self,
        req: &Request<'_>,
        now: &Timestamp,
    ) -> Result<Option<Response>> {
        let share = req.path.strip_prefix("/share/");
        let preview_path = req.path.strip_prefix("/preview/");
        let Some(path) = share.or(preview_path) else {
            return Ok(None);
        };
        if !matches!(req.method, "GET" | "HEAD") {
            return Err(Error::Http(405, "method not allowed"));
        }
        let (key, name) = path.split_once('/').unwrap_or((path, ""));
        let delivery = match self.store.deliverable_by_token(key, share.is_none(), now) {
            Ok(value) => value.try_clone()?,
            Err(spin_store::Error::NotFound) if share.is_some() => {
                return Err(Error::Http(410, "deze link is verlopen"));
            }
            Err(spin_store::Error::NotFound) => {
                if self.identity(req, now)?.is_none() {
                    return Err(Error::Http(401, "authentication required"));
                }
                self.store.deliverable(key)?.try_clone()?
            }
            Err(error) => return Err(error.into()),
        };
        let mut response = Response::empty(200)?;
        if !d::deliverable_is_bundle(&delivery.kind) && share.is_some() {
            response.body = text(format_args!(
                "<!doctype html><meta charset=\"utf-8\"><title>{}</title><h1>{}</h1><pre>{}</pre>",
                escape(&delivery.name)?,
                escape(&delivery.name)?,
                escape(&delivery.content)?
            ))?
            .into_bytes();
            preview(&mut response, req, "text/html; charset=utf-8")?;
        } else {
            let saved = delivery
                .bundle
                .as_ref()
                .ok_or(Error::Http(404, "not found"))?;
            let bytes = self.store.read_blob(&saved.r#ref, bundle::MAX_BYTES)?;
            let entries = bundle::entries(&bytes)
                .map_err(|_| Error::Http(500, "invalid deliverable archive"))?;
            let name = if name.is_empty() {
                saved.entry.as_str()
            } else {
                name
            };
            if name.is_empty() {
                let mut page = text(format_args!(
                    "<!doctype html><meta charset=\"utf-8\"><title>{}</title><h1>{}</h1>",
                    escape(&delivery.name)?,
                    escape(&delivery.name)?
                ))?;
                for entry in entries {
                    try_push_str(
                        &mut page,
                        &text(format_args!(
                            "<p><a href=\"{}\">{}</a></p>",
                            file_url(entry.name)?,
                            escape(entry.name)?
                        ))?,
                    )?;
                }
                response.body = page.into_bytes();
                preview(&mut response, req, "text/html; charset=utf-8")?;
            } else {
                if !bundle::safe_name(name) {
                    return Err(Error::Http(404, "not found"));
                }
                let entry = entries
                    .iter()
                    .find(|e| e.name == name)
                    .ok_or(Error::Http(404, "not found"))?;
                response.body = entry
                    .read()
                    .map_err(|_| Error::Http(500, "invalid deliverable entry"))?;
                preview(&mut response, req, bundle::content_type(name))?;
            }
        }
        Ok(Some(response))
    }
    pub(crate) fn deliverable_download(&mut self, req: &Request<'_>) -> Result<Option<Response>> {
        let Some(id) = req
            .path
            .strip_prefix("/api/deliverables/")
            .and_then(|s| s.strip_suffix("/download"))
        else {
            return Ok(None);
        };
        if !matches!(req.method, "GET" | "HEAD") {
            return Err(Error::Http(405, "method not allowed"));
        }
        let delivery = self.store.deliverable(id)?.try_clone()?;
        let mut response = Response::empty(200)?;
        let (ext, mime) = if let Some(bundle) = &delivery.bundle {
            response.body = self.store.read_blob(&bundle.r#ref, bundle::MAX_BYTES)?;
            ("zip", "application/zip")
        } else {
            response.body = delivery.content.try_clone()?.into_bytes();
            ("md", "text/markdown; charset=utf-8")
        };
        response.header("Content-Type", mime)?;
        response.header(
            "Content-Disposition",
            &text(format_args!(
                "attachment; filename=\"{}-r{}.{ext}\"",
                d::deliverable_slug(&delivery.name)?,
                delivery.revision
            ))?,
        )?;
        Ok(Some(response))
    }
}
