//! Niet-blokkerende client-WebSocket. Eén uitgaand frame en één decoder per socket.
use crate::{client_net::Transport, storage::Random};
use leanhttp::{AsyncRead, AsyncWrite};
use spin_core::websocket::{self as ws, Decoder, Event, Role};
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
        let mut random = Random::open()?;
        let mut nonce = Vec::new();
        nonce.try_reserve_exact(16).map_err(std::io::Error::other)?;
        nonce.resize(16, 0);
        random.fill(&mut nonce).map_err(std::io::Error::other)?;
        let key = &spin_security::encode_base64(&nonce, true).map_err(std::io::Error::other)?;
        let request = ws::upgrade_request(endpoint, key, token).map_err(std::io::Error::other)?;
        let socket =
            crate::client_net::connect(&endpoint.host, endpoint.port, endpoint.encrypted).await?;
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
    // Alleen een geweigerd token is fataal; een bezette identiteit (409) komt
    // na PONG_WAIT_MS vanzelf vrij en wordt door de runner-lus opnieuw geprobeerd.
    match ws::check_upgrade(bytes, expected) {
        Ok(ws::Upgrade::Accepted) => Ok(()),
        Ok(ws::Upgrade::Denied) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "runner token rejected by the server; check SPIN_WORKER_TOKEN",
        )),
        Ok(ws::Upgrade::Busy) => Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "runner identity is already connected from another process",
        )),
        Err(_) => Err(std::io::Error::other("runner WebSocket handshake rejected")),
    }
}
pub(crate) use ws::Endpoint;
/// Een HTTPS-URL wordt nooit verlaagd naar plaintext of naar een andere identiteit.
pub(crate) fn endpoint(url: &str) -> std::io::Result<Endpoint> {
    ws::endpoint(url).map_err(std::io::Error::other)
}
