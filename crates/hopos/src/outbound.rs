//! Uitgaand verkeer van de native server: Lean's `WebDial` over de kale
//! TCP-dial van applib (DNS uit de env), met de Mozilla-wortels van leantls,
//! de wandklok van de app en verse platformentropie per handshake.
use crate::platform::Random;
use applib::tcp::{Dialer, TcpConn};
use core::time::Duration;
use spin_security::Entropy;

/// De webdialer van de app; alleen de logregel bij een mislukte verbinding is van Spin.
pub(crate) struct Dial(Dialer);
impl Dial {
    pub(crate) fn new() -> Self {
        Self(Dialer::new(applib::EXEC.get(), Duration::from_secs(10)))
    }
}
fn unix_seconds() -> Option<u64> {
    Some(applib::app()?.wall_ns()? / 1_000_000_000)
}
/// Een kernel zonder seed geeft geen entropie, en dan ook geen handshake.
fn entropy() -> Option<leantls::Entropy> {
    let mut seed = [0; leantls::Entropy::LEN];
    Random::open(applib::app()?).ok()?.fill(&mut seed).ok()?;
    Some(leantls::Entropy::new(core::mem::replace(
        &mut seed,
        [0; leantls::Entropy::LEN],
    )))
}
impl leanhttp::Dial for Dial {
    type Conn = leanhttps::Link<TcpConn>;
    fn is_encrypted(&self) -> bool {
        true
    }
    async fn dial(&mut self, target: leanhttp::Target<'_>) -> leanhttp::Result<Self::Conn> {
        // Per dial een verse WebDial over de geleende Dialer: zo zijn beide
        // redenen leesbaar (DNS of TCP van applib, TLS van leanhttps).
        let mut web =
            leanhttps::WebDial::new(&mut self.0, leantls::MOZILLA_ROOTS, unix_seconds, entropy);
        let result = web.dial(target).await;
        let tls = web.last_error();
        if let Err(error) = &result {
            match (tls, self.0.last_error()) {
                (Some(tls), _) => applib::log!(
                    "SPIN_OUTBOUND_FAILED host={} error={error:?} reason=tls: {tls}",
                    target.host
                ),
                (None, Some(tcp)) => applib::log!(
                    "SPIN_OUTBOUND_FAILED host={} error={error:?} reason={tcp}",
                    target.host
                ),
                (None, None) => {
                    applib::log!("SPIN_OUTBOUND_FAILED host={} error={error:?}", target.host)
                }
            }
        }
        result
    }
}
