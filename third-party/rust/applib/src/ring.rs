//! De SPSC-ring van de hop-ABI, zoals een app hem gebruikt: [`Writer`] voor
//! de outbox en de TX-frame-ring, [`Reader`] voor de RX-frame-ring, en
//! [`Peek`] voor de deurbel.
//!
//! De ring zelf is `abi::ring`: één producer en één consument, lock-vrij
//! met monotone indexen, met het cache-onderhoud in de ring en niet bij de
//! gebruiker (handboek §5). Hier staat alleen wat de app-kant erbij nodig
//! heeft: een leesblik op de kop die niet de consument is.

use dev::Pa;

pub use abi::ring::{Coherence, Corrupt, Kind, Reader, Record, Writer, init};

/// Een leesblik op de kop van een ring zonder hem te consumeren: wat de
/// deurbel nodig heeft (head als wek-drempel, pending als wek-besluit).
///
/// Geen [`Reader`]: die is van de pomp, en de idle mag hem niet lenen. De
/// blik opent bij elke toets een tweede `Reader` op dezelfde ring en vraagt
/// alleen `head_pending`; dat leest twee woorden en schrijft niets.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Peek {
    base: Pa,
    cap: u64,
}

impl Peek {
    /// Een blik op de ring op `base` met capaciteit `cap` (uit de layout).
    #[must_use]
    pub const fn new(base: Pa, cap: u64) -> Self {
        Self { base, cap }
    }

    /// De producer-index en of er ongelezen records liggen. Een ring die
    /// niet te openen is, heeft niets.
    #[must_use]
    pub fn head_pending(&self) -> (u64, bool) {
        Reader::open(self.base, self.cap).map_or((0, false), |r| r.head_pending())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Een ring over een gewone buffer, zoals de kern hem klaarzet.
    pub(crate) struct Backing(Vec<u64>);

    impl Backing {
        pub(crate) fn new(cap: u64) -> Self {
            let b = Self(vec![0u64; ((abi::ring::DATA_OFF + cap) / 8) as usize]);
            init(b.pa(), cap).unwrap();
            b
        }
        pub(crate) fn pa(&self) -> Pa {
            Pa(self.0.as_ptr() as usize as u64)
        }
    }

    #[test]
    fn peek_sees_what_the_producer_published() {
        let b = Backing::new(256);
        let mut w = Writer::open(b.pa(), 256).unwrap();
        let p = Peek::new(b.pa(), 256);
        assert_eq!(p.head_pending(), (0, false));
        assert_eq!(w.write(Kind::FRAME, b"abc"), Ok(true));
        assert_eq!(p.head_pending(), (16, true));
        let mut r = Reader::open(b.pa(), 256).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(r.read_into(&mut buf).map(|rec| rec.payload.len()), Some(3));
        assert_eq!(p.head_pending(), (16, false));
    }

    #[test]
    fn peek_on_a_bad_backing_has_nothing() {
        assert_eq!(Peek::new(Pa(0x1004), 256).head_pending(), (0, false));
    }
}
