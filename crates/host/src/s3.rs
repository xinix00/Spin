//! Replica's S3 over Lean op de host: `leans3http::Http` boven de
//! host-webdialer; spiegel van hopos/s3.rs.
use crate::{executor, outbound};
use replica_sqlite::asynchronous::{Cancelled, Suspend};
use std::{
    future::{Future, poll_fn},
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    task::Poll,
    time::{Duration, Instant},
};
/// Bytes die van S3 binnenkwamen; het openingsscherm toont ze tijdens een herstel.
pub static DOWNLOADED: AtomicU64 = AtomicU64::new(0);
/// Eén S3-verbinding; termijnen, herkansingen en de body-grens zijn die van Lean.
pub type Network = leans3http::Http<outbound::Dial, Clock>;
/// De monotone klok van de host.
pub struct Clock(Instant);
impl leans3http::Clock for Clock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
        pause(duration)
    }
}
/// Een verbinding die de data-delen in [`DOWNLOADED`] telt. Manifests,
/// current en de lease lezen bij elke start, en dat is geen herstel.
pub(crate) fn network() -> Network {
    leans3http::Http::new(outbound::dial(), Clock(Instant::now())).observe(|target, bytes| {
        if target.contains("/data/") {
            DOWNLOADED.fetch_add(bytes as u64, Relaxed);
        }
    })
}
/// De host blokkeert de eigenaar-thread op de future; er is geen aparte C-stack.
/// Blokkeert op de host-executor waar Replica wacht.
pub struct Block;
impl Suspend for Block {
    fn wait<F: Future>(&self, future: F) -> Result<F::Output, Cancelled> {
        Ok(executor::block_on(future))
    }
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
