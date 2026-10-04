//! The legacy restore endpoint streams into the same authenticated chunk owner.
//! Lean still validates the complete HTTP header; only this endpoint's body framing
//! is handed to the bounded bulk reader after its original length is validated.
use super::*;
use core::time::Duration;
use leanhttp::{AsyncRead, AsyncWrite, Close, IoError};
use spin_domain::{TryClone, Wire, json::Value};

pub(crate) struct Prefixed<C> {
    socket: C,
    prefix: Vec<u8>,
    at: usize,
}
impl<C: AsyncRead> AsyncRead for Prefixed<C> {
    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        out: &mut [u8],
    ) -> Poll<core::result::Result<usize, IoError>> {
        if self.at < self.prefix.len() {
            let count = out.len().min(self.prefix.len() - self.at);
            out[..count].copy_from_slice(&self.prefix[self.at..self.at + count]);
            self.at += count;
            Poll::Ready(Ok(count))
        } else {
            self.socket.poll_read(cx, out)
        }
    }
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> core::result::Result<(), IoError> {
        self.socket.set_read_timeout(timeout)
    }
}
impl<C: AsyncWrite> AsyncWrite for Prefixed<C> {
    fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<core::result::Result<usize, IoError>> {
        self.socket.poll_write(cx, bytes)
    }
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<core::result::Result<(), IoError>> {
        self.socket.poll_flush(cx)
    }
    fn set_write_timeout(
        &mut self,
        timeout: Option<Duration>,
    ) -> core::result::Result<(), IoError> {
        self.socket.set_write_timeout(timeout)
    }
}
impl<C: Close> Close for Prefixed<C> {
    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<core::result::Result<(), IoError>> {
        self.socket.poll_close(cx)
    }
    fn has_grown(&self) -> bool {
        self.socket.has_grown()
    }
}
fn extend(out: &mut Vec<u8>, bytes: &[u8]) -> leanhttp::Result {
    out.try_reserve(bytes.len())
        .map_err(|_| leanhttp::Error::Alloc { bytes: bytes.len() })?;
    out.extend_from_slice(bytes);
    Ok(())
}
pub(crate) async fn prefix<C: leanhttp::Conn>(
    mut socket: C,
) -> leanhttp::Result<(Prefixed<C>, Option<u64>)> {
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut bytes = Vec::new();
    let mut block = [0u8; 4096];
    let length = loop {
        if let Some(end) = bytes.windows(2).position(|p| p == b"\r\n") {
            let line = &bytes[..end];
            let bulk = line == b"POST /api/restore HTTP/1.1"
                || (line.starts_with(b"POST /api/restore?") && line.ends_with(b" HTTP/1.1"));
            if !bulk {
                break None;
            }
            if let Some(end) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
                if end + 4 > leanhttp::MAX_HEADER_BYTES {
                    return Err(leanhttp::Error::HeadersTooLarge {
                        limit: leanhttp::MAX_HEADER_BYTES,
                    });
                }
                let head = core::str::from_utf8(&bytes[..end + 2])
                    .map_err(|_| leanhttp::Error::NotUtf8)?;
                let mut offset = head
                    .find("\r\n")
                    .ok_or(leanhttp::Error::MalformedRequestLine)?
                    + 2;
                let mut framing = None;
                for line in head[offset..].split("\r\n").filter(|l| !l.is_empty()) {
                    let (name, value) = line
                        .split_once(':')
                        .ok_or(leanhttp::Error::MalformedHeader)?;
                    if name.eq_ignore_ascii_case("Transfer-Encoding") {
                        return Err(leanhttp::Error::RequestTransferEncoding);
                    }
                    if name.eq_ignore_ascii_case("Content-Length") {
                        if framing.is_some() {
                            return Err(leanhttp::Error::RepeatedFraming);
                        }
                        let value = value.trim();
                        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                            return Err(leanhttp::Error::BadContentLength);
                        }
                        let length = value
                            .parse::<u64>()
                            .map_err(|_| leanhttp::Error::BadContentLength)?;
                        if length == 0 || length > 64u64 << 30 {
                            return Err(leanhttp::Error::BodyTooLarge {
                                len: length,
                                limit: 64u64 << 30,
                            });
                        }
                        framing = Some((offset, offset + line.len(), length));
                    }
                    offset += line.len() + 2;
                }
                let (start, end, length) = framing.ok_or(leanhttp::Error::BadContentLength)?;
                let mut rewritten = Vec::new();
                extend(&mut rewritten, &bytes[..start])?;
                extend(&mut rewritten, b"Content-Length: 0")?;
                extend(&mut rewritten, &bytes[end..])?;
                bytes = rewritten;
                break Some(length);
            }
        }
        if bytes.len() >= leanhttp::MAX_HEADER_BYTES {
            return Err(leanhttp::Error::HeadersTooLarge {
                limit: leanhttp::MAX_HEADER_BYTES,
            });
        }
        let count = leanhttp::read(&mut socket, &mut block).await?;
        if count == 0 {
            return Err(leanhttp::Error::UnexpectedEof);
        }
        extend(&mut bytes, &block[..count])?;
    };
    Ok((
        Prefixed {
            socket,
            prefix: bytes,
            at: 0,
        },
        length,
    ))
}
/// Het vaste deel van elk intern verzoek van één herstelverbinding.
struct Lane<'a, K> {
    mail: &'a Mail,
    index: usize,
    clock: K,
    template: Input,
}
async fn request(
    lane: &Lane<'_, impl Clock>,
    method: &str,
    path: &str,
    body: Vec<u8>,
    offset: Option<u64>,
) -> leanhttp::Result<Response> {
    let (mail, index) = (lane.mail, lane.index);
    let mut headers = Vec::new();
    for (key, value) in &lane.template.headers {
        if key.eq_ignore_ascii_case("Content-Length")
            || key.eq_ignore_ascii_case("X-Spin-Upload-Offset")
        {
            continue;
        }
        headers
            .try_reserve(1)
            .map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
        headers.push((
            key.try_clone().map_err(http_alloc)?,
            value.try_clone().map_err(http_alloc)?,
        ));
    }
    if let Some(offset) = offset {
        headers
            .try_reserve(1)
            .map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
        headers.push((
            try_string("X-Spin-Upload-Offset").map_err(http_alloc)?,
            spin_core::validation::text(format_args!("{offset}")).map_err(http_alloc)?,
        ));
    }
    let input = Input {
        method: try_string(method).map_err(http_alloc)?,
        path: try_string(path).map_err(http_alloc)?,
        raw_query: String::new(),
        headers,
        body,
        peer: lane.template.peer.try_clone().map_err(http_alloc)?,
        secure: lane.template.secure,
    };
    {
        let mut slots = mail.slots.0.borrow_mut();
        slots[index].routed = true;
        slots[index].queued_ms = lane.clock.millis();
        slots[index].request = Some(input);
    }
    mail.nudge();
    core::future::poll_fn(|_| {
        let mut slots = mail.slots.0.borrow_mut();
        if slots[index].abort {
            return Poll::Ready(Err(leanhttp::Error::Io(IoError::Closed)));
        }
        slots[index]
            .response
            .take()
            .map_or(Poll::Pending, |response| Poll::Ready(Ok(response)))
    })
    .await
}
async fn response(raw: &mut impl AsyncWrite, response: &Response) -> leanhttp::Result {
    raw.set_write_timeout(Some(Duration::from_secs(20)))?;
    leanhttp::write_all(raw, response_head(response)?.as_bytes()).await?;
    leanhttp::write_all(raw, &response.body).await?;
    leanhttp::flush(raw).await?;
    Ok(())
}
async fn event(raw: &mut impl AsyncWrite, value: &Value) -> leanhttp::Result {
    let json = value.to_json().map_err(http_alloc)?;
    let head = spin_core::validation::text(format_args!("{:x}\r\n", json.len() + 1))
        .map_err(http_alloc)?;
    raw.set_write_timeout(Some(Duration::from_secs(20)))?;
    leanhttp::write_all(raw, head.as_bytes()).await?;
    leanhttp::write_all(raw, json.as_bytes()).await?;
    leanhttp::write_all(raw, b"\n\r\n").await?;
    leanhttp::flush(raw).await?;
    Ok(())
}
pub(crate) async fn serve<C: leanhttp::Conn, K: Clock>(
    exchange: &mut leanhttp::Exchange<'_, C>,
    size: u64,
    mail: &Mail,
    index: usize,
    peer: &str,
    secure: bool,
    clock: K,
) -> leanhttp::Result {
    let mut headers = Vec::new();
    let mut progress = false;
    for (key, value) in exchange.req.header.iter() {
        if key.eq_ignore_ascii_case("Accept") && value.contains("application/x-ndjson") {
            progress = true;
        }
        headers
            .try_reserve(1)
            .map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
        headers.push((
            try_string(key).map_err(http_alloc)?,
            try_string(value).map_err(http_alloc)?,
        ));
    }
    let lane = Lane {
        mail,
        index,
        clock,
        template: Input {
            method: String::new(),
            path: String::new(),
            raw_query: String::new(),
            headers,
            body: Vec::new(),
            peer: try_string(peer).map_err(http_alloc)?,
            secure,
        },
    };
    let body = spin_core::validation::text(format_args!(
        "{{\"kind\":\"restore\",\"name\":\"backup.zip\",\"size\":{size}}}"
    ))
    .map_err(http_alloc)?
    .into_bytes();
    let created = request(&lane, "POST", "/api/uploads", body, None).await?;
    let mut raw = exchange.hijack()?;
    if created.status != 201 {
        return response(&mut raw, &created).await;
    }
    let value = Value::from_json(&created.body).map_err(http_alloc)?;
    let id = value
        .as_object()
        .and_then(|o| o.get("id"))
        .and_then(Value::as_str)
        .ok_or(leanhttp::Error::BadTarget)?;
    let path =
        spin_core::validation::text(format_args!("/api/uploads/{id}")).map_err(http_alloc)?;
    let mut offset = 0;
    while offset < size {
        let count = (size - offset).min(1 << 20) as usize;
        let mut block = Vec::new();
        block
            .try_reserve_exact(count)
            .map_err(|_| leanhttp::Error::Alloc { bytes: count })?;
        block.resize(count, 0);
        raw.set_read_timeout(Some(Duration::from_secs(120)))?;
        let read = async {
            let mut filled = 0;
            while filled < block.len() {
                let count = leanhttp::read(&mut raw, &mut block[filled..]).await?;
                if count == 0 {
                    return Err(leanhttp::Error::UnexpectedEof);
                }
                filled += count;
            }
            Ok(())
        }
        .await;
        if let Err(error) = read {
            let _ = request(&lane, "DELETE", &path, Vec::new(), None).await;
            return Err(error);
        }
        let uploaded = request(&lane, "PUT", &path, block, Some(offset)).await?;
        if uploaded.status >= 400 {
            let _ = request(&lane, "DELETE", &path, Vec::new(), None).await;
            return response(&mut raw, &uploaded).await;
        }
        offset += count as u64;
    }
    let complete =
        spin_core::validation::text(format_args!("{path}/complete")).map_err(http_alloc)?;
    let started = request(&lane, "POST", &complete, Vec::new(), None).await?;
    if started.status != 202 {
        return response(&mut raw, &started).await;
    }
    let value = Value::from_json(&started.body).map_err(http_alloc)?;
    let id = value
        .as_object()
        .and_then(|o| o.get("id"))
        .and_then(Value::as_str)
        .ok_or(leanhttp::Error::BadTarget)?;
    let status_path =
        spin_core::validation::text(format_args!("/api/restores/{id}")).map_err(http_alloc)?;
    if progress {
        let mut head = Response::empty(200).map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
        head.header("Content-Type", "application/x-ndjson")
            .map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
        head.header("Transfer-Encoding", "chunked")
            .map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
        head.header("X-Accel-Buffering", "no")
            .map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
        response(&mut raw, &head).await?;
    }
    let mut sent = 0;
    loop {
        let status = request(&lane, "GET", &status_path, Vec::new(), None).await?;
        let value = Value::from_json(&status.body).map_err(http_alloc)?;
        let fields = value.as_object().ok_or(leanhttp::Error::BadTarget)?;
        let state = fields
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("error");
        if state == "running" {
            if progress && clock.millis().saturating_sub(sent) >= 200 {
                let mut report = value.try_clone().map_err(http_alloc)?;
                if let Value::Object(fields) = &mut report {
                    fields
                        .push("type", Value::string("progress").map_err(http_alloc)?)
                        .map_err(http_alloc)?;
                }
                event(&mut raw, &report).await?;
                sent = clock.millis();
            }
            continue;
        }
        if progress {
            let mut report = value.try_clone().map_err(http_alloc)?;
            if let Value::Object(fields) = &mut report {
                fields
                    .push(
                        "type",
                        Value::string(if state == "complete" {
                            "complete"
                        } else {
                            "error"
                        })
                        .map_err(http_alloc)?,
                    )
                    .map_err(http_alloc)?;
            }
            event(&mut raw, &report).await?;
            leanhttp::write_all(&mut raw, b"0\r\n\r\n").await?;
            leanhttp::flush(&mut raw).await?;
            return Ok(());
        }
        let result = fields
            .get(if state == "complete" {
                "result"
            } else {
                "error"
            })
            .unwrap_or(&Value::Null);
        let value = if state == "complete" {
            result.try_clone().map_err(http_alloc)?
        } else {
            spin_core::acp::object(&[("error", result.try_clone().map_err(http_alloc)?)])
                .map_err(http_alloc)?
        };
        let result = Response::json(if state == "complete" { 200 } else { 400 }, &value)
            .map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
        return response(&mut raw, &result).await;
    }
}
