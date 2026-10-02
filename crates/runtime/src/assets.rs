//! De ongewijzigde frontend krijgt dezelfde versie- en cachegrenzen als in Go.
use alloc::string::String;
use leanhttp::{Conn, Exchange};
include!(concat!(env!("OUT_DIR"), "/assets.rs"));
const HTML: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ui.html"));
fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        "html" => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    }
}
pub(crate) async fn serve<C: Conn>(exchange: &mut Exchange<'_, C>) -> leanhttp::Result<bool> {
    if !matches!(exchange.req.method.as_str(), "GET" | "HEAD") {
        return Ok(false);
    }
    let (body, content_type, immutable) =
        if let Some(mut path) = exchange.req.path.strip_prefix("/assets/") {
            let mut immutable = false;
            if let Some((version, rest)) = path.split_once('/')
                && let Some(digits) = version.strip_prefix('v')
                && !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
            {
                immutable = digits == VERSION;
                path = rest;
            }
            let Some((_, body)) = ASSETS.iter().find(|(name, _)| *name == path) else {
                exchange.error(404, "not found").await?;
                return Ok(true);
            };
            (*body, mime(path), immutable)
        } else if !exchange.req.path.starts_with("/api/")
            && exchange.req.path != "/healthz"
            && !exchange.req.path.starts_with("/share/")
            && !exchange.req.path.starts_with("/preview/")
        {
            (HTML, "text/html; charset=utf-8", false)
        } else {
            return Ok(false);
        };
    let headers = spin_server::Response::empty(200)
        .map_err(|_| leanhttp::Error::Alloc { bytes: 1024 })?
        .headers;
    for (name, value) in headers.iter() {
        exchange.header_mut().set(name, value)?;
    }
    exchange.header_mut().set("Content-Type", content_type)?;
    if immutable {
        for name in [
            "CDN-Cache-Control",
            "Cloudflare-CDN-Cache-Control",
            "Surrogate-Control",
            "Pragma",
            "Expires",
        ] {
            exchange.header_mut().remove(name);
        }
        exchange
            .header_mut()
            .set("Cache-Control", "public, max-age=31536000, immutable")?;
    }
    let mut length = String::new();
    use core::fmt::Write;
    length
        .try_reserve(24)
        .map_err(|_| leanhttp::Error::Alloc { bytes: 24 })?;
    write!(&mut length, "{}", body.len()).map_err(|_| leanhttp::Error::Alloc { bytes: 24 })?;
    exchange.header_mut().set("Content-Length", &length)?;
    exchange.write_header(200)?;
    exchange.write(body).await?;
    Ok(true)
}
