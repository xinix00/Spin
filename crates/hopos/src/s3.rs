//! Replica's S3 over Lean: `leans3http::Http` boven de webdialer, op de klok
//! van applib. Termijnen, herkansingen en de body-grens zijn die van Lean.
use crate::outbound::Dial;
use alloc::rc::Rc;
use core::{future::Future, time::Duration};
pub(crate) type Network = leans3http::Http<Dial, Clock>;
/// De monotone klok en het timerwiel van de app.
pub(crate) struct Clock;
impl leans3http::Clock for Clock {
    fn now(&self) -> Duration {
        Duration::from_nanos(applib::clock::now_ns())
    }
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
        applib::EXEC.get().after(duration)
    }
}
/// Eén S3-verbinding.
pub(crate) fn network() -> Network {
    leans3http::Http::new(Dial::new(), Clock)
}
/// Een verbinding die de ontvangen bytes in de hersteltelling van een tenant
/// telt. Alleen data-delen: manifests, current en de lease lezen bij elke
/// start, en dat is geen herstel.
pub(crate) fn counting(restore: Rc<spin_runtime::Restore>) -> Network {
    network().observe(move |target, bytes| {
        if target.contains("/data/") {
            restore
                .downloaded
                .set(restore.downloaded.get().saturating_add(bytes as u64));
        }
    })
}
/// Logt de HTTP-reden van een mislukte S3-aanroep op een eigen verbinding.
pub(crate) fn report(network: &Network) {
    if let Some(error) = network.last_error() {
        applib::log!("SPIN_S3_TRANSPORT_FAILED error={error:?}");
    }
}
