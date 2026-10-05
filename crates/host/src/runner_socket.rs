//! Niet-blokkerende client-WebSocket. Eén uitgaand frame en één decoder per socket.
use crate::{client_net::Transport, storage::Random};
use leanhttp::{AsyncRead, AsyncWrite};
use spin_core::{
    validation::text,
    websocket::{self as ws, Decoder, Event, Role},
};
use spin_domain::try_string;
use spin_security::Entropy;
use std::{
    task::{Context, Poll},
    time::{Duration, Instant},
};

struct WriteFrame {
    bytes: Vec<u8>,
    offset: usize,
    ticket: Option<u64>,
    started: Instant,
}
pub(crate) struct Socket {
    socket: Transport,
    expected: String,
    headers: Vec<u8>,
    remaining: Vec<u8>,
    ready: bool,
    decoder: Decoder,
    writing: Option<WriteFrame>,
    acknowledged: Option<u64>,
    touched: Instant,
}
impl Socket {
    pub(crate) async fn connect(endpoint: &Endpoint, token: &str) -> std::io::Result<Self> {
        let host = endpoint.authority.as_str();
        let path = endpoint.path.as_str();
        if host.is_empty()
            || host.chars().any(|c| c.is_control() || c.is_whitespace())
            || !path.starts_with('/')
            || path.chars().any(|c| c.is_control() || c.is_whitespace())
            || token.is_empty()
            || token.chars().any(|c| c.is_control())
        {
            return Err(std::io::Error::other("invalid runner endpoint or token"));
        }
        let socket =
            crate::client_net::connect(&endpoint.host, endpoint.port, endpoint.encrypted).await?;
        let mut random = Random::open()?;
        let mut nonce = Vec::new();
        nonce.try_reserve_exact(16).map_err(std::io::Error::other)?;
        nonce.resize(16, 0);
        random.fill(&mut nonce).map_err(std::io::Error::other)?;
        let key = &spin_security::encode_base64(&nonce, true).map_err(std::io::Error::other)?;
        let request = text(format_args!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\nAuthorization: Bearer {token}\r\n\r\n")).map_err(std::io::Error::other)?;
        Ok(Self {
            socket,
            expected: ws::accept(key).map_err(std::io::Error::other)?,
            headers: Vec::new(),
            remaining: Vec::new(),
            ready: false,
            decoder: Decoder::new(Role::Client, spin_domain::protocol::MAX_MESSAGE_BYTES),
            writing: Some(WriteFrame {
                bytes: request.into_bytes(),
                offset: 0,
                ticket: None,
                started: Instant::now(),
            }),
            acknowledged: None,
            touched: Instant::now(),
        })
    }
    pub(crate) fn ready(&self) -> bool {
        self.ready
    }
    pub(crate) fn idle_writer(&self) -> bool {
        self.writing.is_none()
    }
    pub(crate) fn acknowledged(&mut self) -> Option<u64> {
        self.acknowledged.take()
    }
    pub(crate) fn send(
        &mut self,
        opcode: u8,
        bytes: &[u8],
        ticket: Option<u64>,
        random: &mut Random,
    ) -> std::io::Result<bool> {
        if !self.ready || self.writing.is_some() {
            return Ok(false);
        }
        let mut mask = [0; 4];
        random.fill(&mut mask).map_err(std::io::Error::other)?;
        self.writing = Some(WriteFrame {
            bytes: ws::encode(opcode, bytes, Some(mask)).map_err(std::io::Error::other)?,
            offset: 0,
            ticket,
            started: Instant::now(),
        });
        Ok(true)
    }
    pub(crate) fn poll(&mut self, context: &mut Context<'_>) -> std::io::Result<Option<Event>> {
        if self.touched.elapsed() > Duration::from_secs(if self.ready { 90 } else { 20 }) {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        if let Some(frame) = &mut self.writing {
            if frame.started.elapsed() > Duration::from_secs(20) {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            if frame.offset < frame.bytes.len() {
                let end = frame.bytes.len().min(frame.offset + (64 << 10));
                match self
                    .socket
                    .poll_write(context, &frame.bytes[frame.offset..end])
                {
                    Poll::Ready(Ok(0)) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Poll::Ready(Ok(n)) => {
                        frame.offset += n;
                        crate::executor::progress();
                    }
                    Poll::Pending => {}
                    Poll::Ready(Err(error)) => return Err(crate::client_net::error(error)),
                }
            }
            if frame.offset == frame.bytes.len() {
                match self.socket.poll_flush(context) {
                    Poll::Ready(Ok(())) => {
                        self.acknowledged = frame.ticket;
                        self.writing = None;
                    }
                    Poll::Pending => {}
                    Poll::Ready(Err(error)) => return Err(crate::client_net::error(error)),
                }
            }
        }

        if self.ready
            && let Some(event) = self.decoder.next_event().map_err(std::io::Error::other)?
        {
            return Ok(Some(event));
        }
        if !self.remaining.is_empty() {
            let count = self
                .decoder
                .push(&self.remaining)
                .map_err(std::io::Error::other)?;
            if count == 0 {
                return Err(std::io::Error::other("runner decoder made no progress"));
            }
            self.remaining.drain(..count);
            return self.decoder.next_event().map_err(std::io::Error::other);
        }
        let mut bytes = [0; 8192];
        let n = match self.socket.poll_read(context, &mut bytes) {
            Poll::Ready(Ok(0)) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Poll::Ready(Ok(n)) => n,
            Poll::Pending => return Ok(None),
            Poll::Ready(Err(error)) => return Err(crate::client_net::error(error)),
        };
        crate::executor::progress();
        self.touched = Instant::now();
        let incoming = if self.ready {
            &bytes[..n]
        } else {
            if n > (32_usize << 10).saturating_sub(self.headers.len()) {
                return Err(std::io::Error::other("runner response headers too large"));
            }
            self.headers.try_reserve(n).map_err(std::io::Error::other)?;
            self.headers.extend_from_slice(&bytes[..n]);
            let Some(end) = self
                .headers
                .windows(4)
                .position(|s| s == b"\r\n\r\n")
                .map(|n| n + 4)
            else {
                return Ok(None);
            };
            validate_headers(&self.headers[..end], &self.expected)?;
            self.ready = true;
            &self.headers[end..]
        };
        let count = self.decoder.push(incoming).map_err(std::io::Error::other)?;
        if count != incoming.len() {
            self.remaining
                .try_reserve(incoming.len() - count)
                .map_err(std::io::Error::other)?;
            self.remaining.extend_from_slice(&incoming[count..]);
        }
        if self.ready {
            self.headers.clear();
        }
        self.decoder.next_event().map_err(std::io::Error::other)
    }
}
fn validate_headers(bytes: &[u8], expected: &str) -> std::io::Result<()> {
    let value = std::str::from_utf8(bytes).map_err(std::io::Error::other)?;
    let mut lines = value.split("\r\n");
    let first = lines.next().unwrap_or("");
    // Alleen een geweigerd token is fataal; een bezette identiteit (409) komt
    // na PONG_WAIT_MS vanzelf vrij en wordt door de runner-lus opnieuw geprobeerd.
    match first.split_whitespace().nth(1) {
        Some("401" | "403") => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "runner token rejected by the server; check SPIN_WORKER_TOKEN",
            ));
        }
        Some("409") => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "runner identity is already connected from another process",
            ));
        }
        _ => {}
    }
    if !first.starts_with("HTTP/1.1 101 ") && first != "HTTP/1.1 101" {
        return Err(std::io::Error::other("runner WebSocket handshake rejected"));
    }
    let mut upgrade = false;
    let mut connection = false;
    let mut accept = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| std::io::Error::other("invalid runner handshake header"))?;
        if name.eq_ignore_ascii_case("Sec-WebSocket-Accept") {
            if accept || value.trim() != expected {
                return Err(std::io::Error::other("invalid runner WebSocket accept"));
            }
            accept = true;
        }
        if name.eq_ignore_ascii_case("Upgrade") {
            upgrade |= value.trim().eq_ignore_ascii_case("websocket");
        }
        if name.eq_ignore_ascii_case("Connection") {
            connection |= value
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("upgrade"));
        }
        if name.eq_ignore_ascii_case("Sec-WebSocket-Extensions")
            || name.eq_ignore_ascii_case("Sec-WebSocket-Protocol")
        {
            return Err(std::io::Error::other(
                "unsolicited runner WebSocket extension",
            ));
        }
    }
    if accept && upgrade && connection {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "incomplete runner WebSocket handshake",
        ))
    }
}
pub(crate) struct Endpoint {
    pub(crate) authority: String,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) path: String,
    pub(crate) encrypted: bool,
}
/// Een HTTPS-URL wordt nooit verlaagd naar plaintext of naar een andere identiteit.
pub(crate) fn endpoint(url: &str) -> std::io::Result<Endpoint> {
    let (scheme, tail) = url
        .trim()
        .split_once("://")
        .ok_or_else(|| std::io::Error::other("invalid runner URL"))?;
    let encrypted = match scheme {
        "https" | "wss" => true,
        "http" | "ws" => false,
        _ => return Err(std::io::Error::other("unsupported runner URL scheme")),
    };
    let tail = tail.split(['?', '#']).next().unwrap_or("");
    let (authority, prefix) = tail.split_once('/').unwrap_or((tail, ""));
    if authority.is_empty()
        || authority.contains('@')
        || tail.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(std::io::Error::other("invalid runner URL"));
    }
    let (host, port) = if let Some(ipv6) = authority.strip_prefix('[') {
        let (host, suffix) = ipv6
            .split_once(']')
            .ok_or_else(|| std::io::Error::other("invalid runner IPv6 address"))?;
        let port = if suffix.is_empty() {
            ""
        } else {
            suffix
                .strip_prefix(':')
                .ok_or_else(|| std::io::Error::other("invalid runner port"))?
        };
        (host, port)
    } else {
        authority.rsplit_once(':').unwrap_or((authority, ""))
    };
    if host.is_empty() {
        return Err(std::io::Error::other("runner URL has no host"));
    }
    let port = if port.is_empty() {
        if encrypted { 443 } else { 80 }
    } else {
        port.parse::<u16>().map_err(std::io::Error::other)?
    };
    if port == 0 {
        return Err(std::io::Error::other("invalid runner port"));
    }
    let path = if prefix.trim_end_matches('/').is_empty() {
        try_string("/api/runner/ws")
    } else {
        text(format_args!(
            "/{}/api/runner/ws",
            prefix.trim_end_matches('/')
        ))
    }
    .map_err(std::io::Error::other)?;
    Ok(Endpoint {
        authority: try_string(authority).map_err(std::io::Error::other)?,
        host: try_string(host).map_err(std::io::Error::other)?,
        port,
        path,
        encrypted,
    })
}
