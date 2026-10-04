//! SigV4 van Replica/Lean over de native TLS-dialer, zonder redirects.
use crate::outbound::Dial;
use alloc::{string::String, vec::Vec};
use core::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::{Context, Poll},
    time::Duration,
};
use leans3::IoError;
pub(crate) struct Network(
    leanhttp::Client<Dial>,
    Option<alloc::rc::Rc<spin_runtime::Restore>>,
);
pub(crate) struct Response {
    status: u16,
    reason: String,
    header: leanhttp::Header,
    body: Vec<u8>,
    offset: usize,
}
fn error(error: leanhttp::Error) -> IoError {
    applib::log!("SPIN_S3_TRANSPORT_FAILED error={error:?}");
    IoError::Other("S3 HTTP transport failed")
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
        let url = spin_core::validation::text(format_args!(
            "{}://{}{}",
            if request.https { "https" } else { "http" },
            request.host,
            request.target
        ))
        .map_err(|_| IoError::Other("allocation failed"))?;
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
            applib::log!("SPIN_S3_READ_RETRY attempt={attempt}");
            applib::EXEC
                .get()
                .after(Duration::from_secs(1 << (attempt - 1)))
                .await;
        }
        Err(IoError::Other("S3 retry budget exhausted"))
    }
}
impl Network {
    pub(crate) fn new(dial: Dial) -> Self {
        let mut client = leanhttp::Client::new(dial);
        // Replica serializes requests: one reusable TLS connection is enough.
        client.pool.max_idle_per_host = 1;
        client.pool.max_idle_total = 1;
        Self(client, None)
    }
    /// Telt de ontvangen bytes in de hersteltelling van een tenant.
    pub(crate) fn counting(mut self, restore: alloc::rc::Rc<spin_runtime::Restore>) -> Self {
        self.1 = Some(restore);
        self
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
        let deadline = applib::clock::now_ns().saturating_add(60_000_000_000);
        let mut future = pin!(async {
            let mut inner = self
                .0
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
                    Duration::from_nanos(applib::clock::now_ns()),
                )
                .await?;
            if inner.status >= 400 && inner.status != 404 {
                applib::log!("SPIN_S3_HTTP_STATUS status={}", inner.status);
            }
            // Keep this read future across Pending, including partial chunk headers.
            let body = inner.read_to_end(replica_core::segment::MAX_BYTES).await?;
            // Alleen data-delen tellen: manifests, current en de lease lezen
            // bij elke start, en dat is geen herstel.
            if let Some(restore) = self
                .1
                .as_ref()
                .filter(|_| request.target.contains("/data/"))
            {
                restore
                    .downloaded
                    .set(restore.downloaded.get().saturating_add(body.len() as u64));
            }
            let response = Response {
                status: inner.status,
                reason: core::mem::take(&mut inner.reason),
                header: core::mem::take(&mut inner.header),
                body,
                offset: 0,
            };
            self.0
                .finish(inner, Duration::from_nanos(applib::clock::now_ns()))
                .await;
            Ok::<_, leanhttp::Error>(response)
        });
        poll_fn(|cx| {
            if applib::clock::now_ns() >= deadline {
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
        let n = bytes.len().min(this.body.len() - this.offset);
        bytes[..n].copy_from_slice(&this.body[this.offset..this.offset + n]);
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
