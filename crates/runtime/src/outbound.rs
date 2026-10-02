//! Acht gelijktijdige providerverzoeken, elk met een totale deadline en eigen dialer.
use crate::{Clock, Platform, task};
use alloc::string::String;
use core::{
    future::{Future, poll_fn},
    pin::pin,
    task::{Context, Poll, Waker},
    time::Duration,
};
use spin_server::{Error, NetworkRequest, NetworkResponse, Result, Runtime, Server};
use spin_store::Persistence;
const CAPACITY: usize = 8;
type Reply = (String, Result<NetworkResponse>);
pub(crate) struct Pool {
    pending: [Option<task::Task<'static, Reply>>; CAPACITY],
}
impl Pool {
    pub(crate) fn new() -> Self {
        Self {
            pending: core::array::from_fn(|_| None),
        }
    }
    /// Of er een providerverzoek loopt; die vorderen alleen door pollen.
    pub(crate) fn active(&self) -> bool {
        self.pending.iter().any(Option::is_some)
    }
    pub(crate) fn poll<H: Platform, P: Persistence>(
        &mut self,
        platform: &H,
        server: &mut Server<P>,
        now: &spin_domain::Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        if let Err(error) = server.maintain_network(now, random) {
            H::log(format_args!(
                "SPIN_PROVIDER_MAINTENANCE_FAILED error={error}"
            ));
        }
        let mut context = Context::from_waker(Waker::noop());
        for task in &mut self.pending {
            if let Some(pending) = task {
                if let Poll::Ready((id, reply)) = pending.as_mut().poll(&mut context) {
                    *task = None;
                    if let Err(error) = server.finish_network(&id, reply, now, random) {
                        H::log(format_args!("SPIN_PROVIDER_RESULT_FAILED error={error}"));
                    }
                } else {
                    continue;
                }
            }
            let Some(request) = server.take_network_request() else {
                continue;
            };
            match platform.dial() {
                Ok(dial) => *task = Some(task::task(send(dial, request, platform.clock()))?),
                Err(error) => {
                    if let Err(error) = server.finish_network(&request.id, Err(error), now, random)
                    {
                        H::log(format_args!("SPIN_PROVIDER_RESULT_FAILED error={error}"));
                    }
                }
            }
        }
        Ok(())
    }
}
async fn send(mut dial: impl leanhttp::Dial, request: NetworkRequest, clock: impl Clock) -> Reply {
    let start = clock.millis();
    let result = {
        let mut operation = pin!(async {
            let mut header = leanhttp::Header::new();
            for (key, value) in request.headers.iter() {
                header
                    .set(key, value)
                    .map_err(|_| Error::Http(502, "invalid provider request header"))?;
            }
            let mut response = leanhttp::fetch(
                &mut dial,
                leanhttp::Call {
                    method: &request.method,
                    url: &request.url,
                    header,
                    body: Some(&request.body),
                    header_timeout: Some(Duration::from_secs(20)),
                    no_follow: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| Error::Http(502, "provider transport failed"))?;
            let body = response
                .read_to_end(1 << 20)
                .await
                .map_err(|_| Error::Http(502, "provider response exceeds budget or failed"))?;
            Ok(NetworkResponse {
                status: response.status,
                body,
            })
        });
        poll_fn(|cx| {
            if clock.millis().saturating_sub(start) >= 45_000 {
                Poll::Ready(Err(Error::Http(504, "provider request timed out")))
            } else {
                operation.as_mut().poll(cx)
            }
        })
        .await
    };
    (request.id, result)
}
