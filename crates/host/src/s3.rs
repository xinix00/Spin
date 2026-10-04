//! SigV4 van Replica/Lean over de host-TLS-dialer, zonder redirects; spiegel van hopos/s3.rs.
use crate::{executor, outbound::Dial};
use leans3::IoError;
use replica_sqlite::asynchronous::{Cancelled, Suspend};
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::{Context, Poll},
    time::{Duration, Instant},
};
/// Eén herbruikbare verbinding; Replica serialiseert zijn verzoeken.
pub(crate) struct Network {
    client: leanhttp::Client<Dial>,
    started: Instant,
}
pub(crate) struct Response {
    status: u16,
    reason: String,
    header: leanhttp::Header,
    body: Vec<u8>,
    offset: usize,
}
/// De host blokkeert de eigenaar-thread op de future; er is geen aparte C-stack.
pub(crate) struct Block;
impl Suspend for Block {
    fn wait<F: Future>(&self, future: F) -> Result<F::Output, Cancelled> {
        Ok(executor::block_on(future))
    }
}
fn error(error: leanhttp::Error) -> IoError {
    eprintln!("SPIN_S3_TRANSPORT_FAILED error={error:?}");
    IoError::Other("S3 HTTP transport failed")
}
/// Wacht zonder timer: block_on pollt iedere executorronde opnieuw.
async fn pause(duration: Duration) {
    let Some(end) = Instant::now().checked_add(duration) else {
        return;
    };
    poll_fn(|_| {
        if Instant::now() >= end {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await
}
impl leans3::Transport for Network {
    type Response = Response;
    async fn send(&mut self, request: leans3::Request<'_, '_>) -> Result<Response, IoError> {
        let body = match &request.body {
            leans3::Body::None => None,
            leans3::Body::Bytes(bytes) => Some(*bytes),
            leans3::Body::Stream { .. } => {
                return Err(IoError::Other("Replica requires bounded segment uploads"));
            }
        };
        let url = format!(
            "{}://{}{}",
            if request.https { "https" } else { "http" },
            request.host,
            request.target
        );
        for attempt in 1..=4 {
            let result = self.once(&request, body, &url).await;
            let retry = request.method == "GET"
                && match &result {
                    Ok(response) => matches!(response.status, 429 | 500 | 502 | 503 | 504),
                    Err(error) => matches!(
                        error,
                        leanhttp::Error::Connect
                            | leanhttp::Error::Io(_)
                            | leanhttp::Error::Eof
                            | leanhttp::Error::UnexpectedEof
                    ),
                };
            if !retry || attempt == 4 {
                return result.map_err(error);
            }
            eprintln!("SPIN_S3_READ_RETRY attempt={attempt}");
            pause(Duration::from_secs(1 << (attempt - 1))).await;
        }
        Err(IoError::Other("S3 retry budget exhausted"))
    }
}
impl Network {
    pub(crate) fn new() -> Self {
        let mut client = leanhttp::Client::new(Dial);
        client.pool.max_idle_per_host = 1;
        client.pool.max_idle_total = 1;
        Self {
            client,
            started: Instant::now(),
        }
    }
    async fn once(
        &mut self,
        request: &leans3::Request<'_, '_>,
        body: Option<&[u8]>,
        url: &str,
    ) -> leanhttp::Result<Response> {
        let mut header = leanhttp::Header::new();
        for item in request.headers {
            header.set(item.name, &item.value)?;
        }
        let started = self.started;
        let deadline = Instant::now().checked_add(Duration::from_secs(60));
        let mut future = pin!(async {
            let mut inner = self
                .client
                .send(
                    leanhttp::Call {
                        method: request.method,
                        url,
                        header,
                        body,
                        header_timeout: Some(Duration::from_secs(30)),
                        no_follow: true,
                        ..Default::default()
                    },
                    started.elapsed(),
                )
                .await?;
            if inner.status >= 400 && inner.status != 404 {
                eprintln!("SPIN_S3_HTTP_STATUS status={}", inner.status);
            }
            let body = inner.read_to_end(replica_core::segment::MAX_BYTES).await?;
            let response = Response {
                status: inner.status,
                reason: std::mem::take(&mut inner.reason),
                header: std::mem::take(&mut inner.header),
                body,
                offset: 0,
            };
            self.client.finish(inner, started.elapsed()).await;
            Ok::<_, leanhttp::Error>(response)
        });
        poll_fn(|cx| {
            if deadline.is_none_or(|end| Instant::now() >= end) {
                Poll::Ready(Err(leanhttp::Error::Io(leanhttp::IoError::TimedOut)))
            } else {
                future.as_mut().poll(cx)
            }
        })
        .await
    }
}
impl leans3::AsyncRead for Response {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<Result<usize, IoError>> {
        let this = self.get_mut();
        let rest = this.body.get(this.offset..).unwrap_or_default();
        let n = bytes.len().min(rest.len());
        bytes[..n].copy_from_slice(&rest[..n]);
        this.offset += n;
        Poll::Ready(Ok(n))
    }
}
impl leans3::Response for Response {
    fn status(&self) -> u16 {
        self.status
    }
    fn reason(&self) -> &str {
        &self.reason
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.header.get(name)
    }
    fn content_length(&self) -> Option<u64> {
        Some(self.body.len() as u64)
    }
}
