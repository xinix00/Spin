//! Het contract tussen een NIC-driver en de rest van de kern.
//!
//! Een driver levert frames en neemt frames aan; wie hem drijft (de
//! RX-pomp en de switch in `net`, of de app-netstack in `applib`) kent
//! alleen dit. De trait is met opzet klein: batching van doorbells
//! (`flush`) en het wek-signaal van de interrupt zijn de twee dingen die
//! de Go-kern na meting nodig bleek te hebben (de RX-lus: één doorbell per
//! burst, en de 10 ms-vangrail op een verloren flank).

#![cfg_attr(not(test), no_std)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use core::fmt;
use sync::Signal;

/// De MTU van het interne net en van de uplink.
pub const MTU: usize = 1500;

/// De grootste frame die een driver hoeft te dragen: MTU plus de
/// Ethernet-kop en een VLAN-tag, naar boven afgerond op een cacheline.
pub const MAX_FRAME: usize = 1536;

/// Een MAC-adres.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Mac(pub [u8; 6]);

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5]
        )
    }
}

/// Waarom een frame niet verzonden is.
#[derive(Debug, PartialEq, Eq)]
pub enum TxError {
    /// De TX-ring zit vol; probeer na een `flush` of een wek opnieuw.
    Full,
    /// Het frame past niet (langer dan [`MAX_FRAME`] of leeg).
    Size(usize),
    /// Het device is weg of stuk; niets helpt meer.
    Dead,
}

impl fmt::Display for TxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("tx ring full"),
            Self::Size(n) => write!(f, "frame of {n} bytes does not fit"),
            Self::Dead => f.write_str("device gone"),
        }
    }
}

/// Een NIC zoals de kern hem ziet.
pub trait Device {
    /// Zet één frame op de TX-ring. De doorbell mag wachten tot [`flush`].
    ///
    /// [`flush`]: Device::flush
    fn transmit(&mut self, frame: &[u8]) -> Result<(), TxError>;

    /// Haalt één ontvangen frame op in `buf`; `None` als de RX-ring leeg is.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize>;

    /// Publiceert uitgestelde doorbells (TX en RX): één keer per burst,
    /// niet per frame.
    fn flush(&mut self) {}

    /// Het MAC-adres.
    fn mac(&self) -> Mac;

    /// Het wek-signaal van de NIC-interrupt, als het board er een bedraadt.
    /// `None` = pollen, met de microslaap uit de Go-kern.
    fn irq(&self) -> Option<&'static Signal> {
        None
    }
}
