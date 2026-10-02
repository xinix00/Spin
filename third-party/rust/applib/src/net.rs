//! Frame-niveau netwerk van een app: de [`Nic`] over de eigen frame-ringen
//! naar de L2-switch van de kern, en de RX-pomp met de deurbel.
//!
//! Het twee-methode-device (`netdev::Device`) waaraan in Go elke
//! stack-wissel hing (gVisor, lneto, leannet: elke wissel raakte alleen
//! `up.go`). De stack zelf staat in [`crate::appnet`] en gebruikt precies
//! dit; een app die rauwe frames wil ook.
//!
//! Het interne net is deterministisch: kern op .1, slot i op .(i+1)/24, MAC
//! `02:00:00:00:00:<slot>`. Er wordt niets geresolved; beide kanten leiden het
//! uit het slotnummer af.
//!
//! Dit module bezit de TX-producer en de RX-consument van de app. De
//! deurbel-drempel op de control-page is van de idle ([`crate::sleep`]).

use crate::app::App;
use crate::contract::{NET_MTU, NET_RING_DATA_CAP, slot_ip4, slot_mac};
use crate::log;
use crate::ring::{Corrupt, Kind, Peek, Reader, Writer};
use crate::rt::Exec;
use crate::sleep::{self, RxDoor};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use core::time::Duration;
use netdev::{Device, Mac, TxError};
use sync::{Either, Signal, select, yield_now};

/// De MTU van het slot-LAN (geen draad, geen bitfouten).
pub const MTU: usize = NET_MTU;

/// Hoe lang [`Nic::transmit_wait`] een volle TX-ring de tijd geeft: een
/// korte lokale burst krijgt tegendruk in plaats van stil verlies, en een
/// verdwenen switch blijft een gewone device-fout in plaats van een hang.
pub const TX_BACKPRESSURE: Duration = Duration::from_millis(10);

/// Transmit trof de ring vol en wachtte.
pub static TX_WAITS: AtomicU64 = AtomicU64::new(0);
/// Na [`TX_BACKPRESSURE`] alsnog opgegeven.
pub static TX_DROPS: AtomicU64 = AtomicU64::new(0);
/// De pomp werd vóór zijn timer gewekt (de bel).
pub static PUMP_EARLY: AtomicU64 = AtomicU64::new(0);
/// De poll-timer van de pomp liep af.
pub static PUMP_TIMER: AtomicU64 = AtomicU64::new(0);
/// Kicks naar de OS-core (HVC #6) na een leeg-naar-niet-leeg op de TX-ring.
/// Eén per burst, niet per frame: de switch van de kern leest de ring leeg
/// zodra hij wakker is.
///
/// GEMETEN 29-09, `tools/qemu-test.sh` (virt, 4 cores, zes appspike-runs per
/// kant): `dial_us` gemiddeld 3025 zonder en 2922 met de kick (binnen de
/// ruis: na de SYN wacht de app, en die idle-yield kickte al), `flush_us`
/// 1548 zonder en 1193 met (-23%: daar publiceert de app en rekent hij door
/// tot zijn flush). 3 à 4 kicks tot en met de dial.
pub static TX_KICKS: AtomicU64 = AtomicU64::new(0);

/// Het interne IPv4 van slot `slot` (big-endian).
#[must_use]
pub const fn slot_ip(slot: u64) -> [u8; 4] {
    slot_ip4(slot).to_be_bytes()
}

/// Het adres van de kern op het slot-LAN: de gateway.
#[must_use]
pub const fn host_ip() -> [u8; 4] {
    slot_ip(0)
}

/// De MAC van slot `slot` (de kern is slot 0).
#[must_use]
pub const fn mac_of(slot: u64) -> Mac {
    Mac(slot_mac(slot))
}

/// De NIC van een app: de twee frame-ringen in zijn staart.
pub struct Nic {
    tx: Writer,
    rx: Reader,
    rx_peek: Peek,
    mac: Mac,
}

impl Nic {
    /// Opent de frame-ringen van `app`.
    pub fn open(app: &App) -> Result<Self, abi::Error> {
        let t = app.tail();
        Ok(Self {
            tx: Writer::open(t.net_tx(), NET_RING_DATA_CAP)?,
            rx: Reader::open(t.net_rx(), NET_RING_DATA_CAP)?,
            rx_peek: Peek::new(t.net_rx(), NET_RING_DATA_CAP),
            mac: mac_of(app.slot()),
        })
    }

    /// Een NIC over willekeurige ringen (tests, een lokale lus); `rx_peek`
    /// kijkt naar dezelfde ring als `rx`.
    #[must_use]
    pub fn over(tx: Writer, rx: Reader, rx_peek: Peek, mac: Mac) -> Self {
        Self {
            tx,
            rx,
            rx_peek,
            mac,
        }
    }

    /// Hangt de deurbel aan: de idle van de app-core wapent vanaf nu
    /// `CtrlRXDoor` en belt `bell` zodra er RX ligt. Alleen wie de RX-ring
    /// leegleest mag dit (zie [`crate::sleep`]).
    pub fn watch_rx(&self, bell: &'static Signal) {
        sleep::watch_rx(RxDoor {
            peek: self.rx_peek,
            bell,
        });
    }

    /// Eén poging. Vol is [`TxError::Full`], met een bel erbij: dat maakt de
    /// vol-naar-ruimte-race level-triggered zonder architectuurkennis.
    pub fn try_transmit(&mut self, frame: &[u8]) -> Result<(), TxError> {
        if frame.is_empty() || !self.tx.fits(frame.len()) {
            return Err(TxError::Size(frame.len()));
        }
        match self.tx.write(Kind::FRAME, frame) {
            Ok(was_empty) => {
                if was_empty {
                    // De SEV wekt een kern in WFE; de kick een kern die een
                    // bewoner draait of in WFI slaapt (Go: `dev.Notify`, dat
                    // op de M4 beide deed). Zonder kick hoorde de kern een
                    // app die na zijn publicatie blijft rekenen pas op zijn
                    // failsafe van 1 ms of op de idle-yield van de app.
                    dev::notify();
                    crate::arch::hvc_kick_os();
                    TX_KICKS.fetch_add(1, Relaxed);
                }
                Ok(())
            }
            Err(abi::Error::RingFull { .. }) => {
                dev::notify();
                Err(TxError::Full)
            }
            Err(abi::Error::RecordTooLarge { .. }) => Err(TxError::Size(frame.len())),
            // Onmogelijke indexen: de switch beschrijft ze, en er valt niets
            // meer te herstellen tot de kern het slot herstart.
            Err(_) => Err(TxError::Dead),
        }
    }

    /// Zet een frame op de TX-ring en wacht bij een volle ring hooguit
    /// [`TX_BACKPRESSURE`] op ruimte, met een yield per poging. `now` is de
    /// klok in nanoseconden.
    pub async fn transmit_wait(&mut self, frame: &[u8], now: fn() -> u64) -> Result<(), TxError> {
        let budget = u64::try_from(TX_BACKPRESSURE.as_nanos()).unwrap_or(u64::MAX);
        let deadline = now().saturating_add(budget);
        loop {
            match self.try_transmit(frame) {
                Err(TxError::Full) => {
                    TX_WAITS.fetch_add(1, Relaxed);
                    if now() > deadline {
                        TX_DROPS.fetch_add(1, Relaxed);
                        return Err(TxError::Full);
                    }
                    yield_now().await;
                }
                other => return other,
            }
        }
    }

    /// De reden als de RX-ring corrupt verklaard is.
    #[must_use]
    pub fn rx_corruption(&self) -> Option<Corrupt> {
        self.rx.corrupt()
    }
}

impl Device for Nic {
    fn transmit(&mut self, frame: &[u8]) -> Result<(), TxError> {
        self.try_transmit(frame)
    }

    /// Eén frame uit de RX-ring, rechtstreeks in `buf`: geen allocatie en
    /// geen extra kopie. Records van een ander type worden overgeslagen.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let rec = self.rx.read_into(buf)?;
            if rec.kind == Kind::FRAME {
                return Some(rec.payload.len());
            }
        }
    }

    fn mac(&self) -> Mac {
        self.mac
    }
}

/// De slaapstand van de RX-pomp: `lo` (de scherpe slaap), `hi` (de cap) en
/// `hold` (zoveel lege rondes blijft hij op `lo`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RxPoll {
    /// De slaap direct na verkeer.
    pub lo: Duration,
    /// Het plafond waarnaar hij verdubbelt als het stil is.
    pub hi: Duration,
    /// Lege rondes op `lo` voor hij gaat verdubbelen.
    pub hold: u32,
}

impl RxPoll {
    /// De default, "300us:1s:4". GEMETEN (schedbench 29-08): de deurbel
    /// draagt de latency (koud p50 0,8 ms bij een cap van tien seconden),
    /// dus de cap is alleen het vangnet voor een gedoofde bel. 1 s en niet
    /// meer: de heartbeat wekt elke app toch al, dus een grotere cap levert
    /// nul wekken minder op en begrenst een bel-storing op 1 s.
    pub const DEFAULT: Self = Self {
        lo: Duration::from_micros(300),
        hi: Duration::from_secs(1),
        hold: 4,
    };

    /// Leest de stand uit de env (`RXPOLL`): `""` is de default, `"300us"` een
    /// vaste slaap (het gedrag van vóór 29-08), `"300us:5ms"` NAPI-achtig
    /// verdubbelen, `"300us:5ms:8"` idem met acht lege rondes op `lo`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        if s.is_empty() {
            return Self::DEFAULT;
        }
        let mut p = Self {
            lo: Self::DEFAULT.lo,
            hi: Self::DEFAULT.lo,
            hold: 0,
        };
        let mut f = s.split(':');
        if let Some(d) = f.next().and_then(parse_duration).filter(|d| !d.is_zero()) {
            p.lo = d;
            p.hi = d;
        }
        if let Some(d) = f.next().and_then(parse_duration).filter(|&d| d >= p.lo) {
            p.hi = d;
        }
        if let Some(n) = f
            .next()
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|&n| n > 0)
        {
            p.hold = n;
        }
        p
    }

    /// De volgende slaap na `empty` lege rondes, vanaf `d`.
    #[must_use]
    pub fn next(&self, d: Duration, empty: u32) -> Duration {
        if empty > self.hold {
            d.saturating_mul(2).min(self.hi)
        } else {
            d
        }
    }
}

/// Een duur als `300us`, `5ms`, `1s` of `250ns`; de vormen die `RXPOLL`
/// gebruikt. `None` bij iets anders.
#[must_use]
pub fn parse_duration(s: &str) -> Option<Duration> {
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().ok()?;
    match unit {
        "ns" => Some(Duration::from_nanos(n)),
        "us" | "\u{b5}s" => Some(Duration::from_micros(n)),
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        _ => None,
    }
}

/// De kleinste window-scale-shift waarmee een venster van `max_buf` bytes te
/// adverteren is (RFC 7323, plafond 14). Voor de stack-config.
#[must_use]
pub fn ws_shift_for(max_buf: u64) -> u8 {
    let mut shift = 0u8;
    while shift < 14 && (0xffffu64 << shift) < max_buf {
        shift += 1;
    }
    shift
}

/// De RX-pomp als taak: leest frames uit de ring en geeft ze aan `deliver`;
/// als het stil is wacht hij op de bel of zijn timer. Keert nooit terug.
///
/// Elke 16 frames een yield: zonder die yield draait de pomp door zolang er
/// frames liggen en komen de ACK's van de stack pas aan de beurt als de
/// zender zijn hele venster kwijt is (20-09, een 1 Gbit-upload: 216.147
/// segmenten in, 298 ACK's uit, 39 MB/s).
pub async fn pump(
    nic: &mut Nic,
    buf: &mut [u8],
    bell: &'static Signal,
    exec: &'static Exec,
    poll: RxPoll,
    mut deliver: impl FnMut(&[u8]),
) {
    nic.watch_rx(bell);
    let mut d = poll.lo;
    let mut empty: u32 = 0;
    let mut delivered: u32 = 0;
    let mut corrupt_logged = false;
    loop {
        if let Some(n) = nic.receive(buf) {
            d = poll.lo;
            empty = 0;
            deliver(buf.get(..n).unwrap_or_default());
            delivered = delivered.wrapping_add(1);
            if delivered.is_multiple_of(16) {
                yield_now().await;
            }
            continue;
        }
        // Een dode ring is stil: niets meer te lezen en toch "pending". Eén
        // regel met de reden, anders is dat een app die "gewoon niet
        // reageert" (de SMP-jacht van 03-09).
        if !corrupt_logged && let Some(why) = nic.rx_corruption() {
            log!("appnet: RX ring corrupt: {why} HOPOS_APPNET_RX_CORRUPT");
            corrupt_logged = true;
        }
        match select(bell.wait(), exec.after(d)).await {
            Either::Left(()) => PUMP_EARLY.fetch_add(1, Relaxed),
            Either::Right(()) => PUMP_TIMER.fetch_add(1, Relaxed),
        };
        empty = empty.saturating_add(1);
        d = poll.next(d, empty);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::tests::Backing;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    fn now_zero() -> u64 {
        0
    }

    // TestTransmitWachtKortOpRuimteInPlaatsVanDrop: een volle ring geeft
    // tegendruk, geen drop; zodra de consument ruimte maakt gaat het frame
    // er alsnog op.
    #[test]
    fn transmit_waits_briefly_for_room_instead_of_drop() {
        let txb = Backing::new(4096);
        let rxb = Backing::new(4096);
        let mut filler = Writer::open(txb.pa(), 4096).unwrap();
        let tx = Writer::open(txb.pa(), 4096).unwrap();
        let rx = Reader::open(rxb.pa(), 4096).unwrap();
        let frame = [0xabu8; 1000];
        let mut filled = 0;
        while filler.write(Kind::FRAME, &frame).is_ok() {
            filled += 1;
        }
        assert!(filled >= 2, "testring vulde al na {filled} frames");

        let mut nic = Nic::over(tx, rx, Peek::new(rxb.pa(), 4096), mac_of(1));
        let mut fut = pin!(nic.transmit_wait(&frame, now_zero));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(fut.as_mut().poll(&mut cx), Poll::Pending); // vol: wachten
        let waits = TX_WAITS.load(Relaxed);
        assert!(waits >= 1);

        // De switch leest de ring leeg.
        let mut sw = Reader::open(txb.pa(), 4096).unwrap();
        let mut buf = [0u8; 1000];
        while sw.read_into(&mut buf).is_some() {}

        assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        let rec = sw.read_into(&mut buf).unwrap();
        assert_eq!((rec.kind, rec.payload.len()), (Kind::FRAME, frame.len()));
    }

    #[test]
    fn transmit_gives_up_after_the_backpressure_window() {
        static CLOCK: AtomicU64 = AtomicU64::new(0);
        fn clock() -> u64 {
            CLOCK.fetch_add(4_000_000, Relaxed) // 4 ms per lees
        }
        let txb = Backing::new(256);
        let rxb = Backing::new(256);
        let mut filler = Writer::open(txb.pa(), 256).unwrap();
        let tx = Writer::open(txb.pa(), 256).unwrap();
        let rx = Reader::open(rxb.pa(), 256).unwrap();
        let mut nic = Nic::over(tx, rx, Peek::new(rxb.pa(), 256), mac_of(1));
        while filler.write(Kind::FRAME, &[1; 100]).is_ok() {}
        let drops = TX_DROPS.load(Relaxed);
        let mut fut = pin!(nic.transmit_wait(&[1; 100], clock));
        let mut cx = Context::from_waker(Waker::noop());
        let mut polls = 0;
        let r = loop {
            polls += 1;
            if let Poll::Ready(r) = fut.as_mut().poll(&mut cx) {
                break r;
            }
        };
        assert_eq!(r, Err(TxError::Full));
        assert!(polls <= 4);
        assert!(TX_DROPS.load(Relaxed) > drops);
    }

    #[test]
    fn receive_reads_frames_and_refuses_bad_sizes() {
        let txb = Backing::new(4096);
        let rxb = Backing::new(4096);
        let mut switch = Writer::open(rxb.pa(), 4096).unwrap();
        let mut nic = Nic::over(
            Writer::open(txb.pa(), 4096).unwrap(),
            Reader::open(rxb.pa(), 4096).unwrap(),
            Peek::new(rxb.pa(), 4096),
            mac_of(2),
        );
        switch.write(Kind::FRAME, &[5; 60]).unwrap();
        let mut buf = [0u8; 1600];
        assert_eq!(nic.receive(&mut buf), Some(60));
        assert_eq!(nic.receive(&mut buf), None);
        assert_eq!(nic.transmit(&[]), Err(TxError::Size(0)));
        assert_eq!(nic.transmit(&[0; 3000]), Err(TxError::Size(3000)));
        assert_eq!(nic.mac(), Mac([2, 0, 0, 0, 0, 2]));
    }

    #[test]
    fn rx_poll_parses_every_form() {
        assert_eq!(RxPoll::parse(""), RxPoll::DEFAULT);
        let fixed = RxPoll::parse("300us");
        assert_eq!(
            (fixed.lo, fixed.hi, fixed.hold),
            (Duration::from_micros(300), Duration::from_micros(300), 0)
        );
        let napi = RxPoll::parse("300us:5ms");
        assert_eq!(
            (napi.lo, napi.hi, napi.hold),
            (Duration::from_micros(300), Duration::from_millis(5), 0)
        );
        let held = RxPoll::parse("1ms:1s:8");
        assert_eq!(
            (held.lo, held.hi, held.hold),
            (Duration::from_millis(1), Duration::from_secs(1), 8)
        );
        // Een hi onder lo telt niet; rommel valt terug op 300 µs vast.
        assert_eq!(RxPoll::parse("5ms:1ms").hi, Duration::from_millis(5));
        assert_eq!(RxPoll::parse("junk").lo, Duration::from_micros(300));
    }

    #[test]
    fn rx_poll_backs_off_after_hold_and_caps() {
        let p = RxPoll::DEFAULT;
        let mut d = p.lo;
        for empty in 1..=4 {
            d = p.next(d, empty);
        }
        assert_eq!(d, p.lo); // de eerste vier lege rondes blijven scherp
        d = p.next(d, 5);
        assert_eq!(d, Duration::from_micros(600));
        for empty in 6..40 {
            d = p.next(d, empty);
        }
        assert_eq!(d, p.hi);
    }

    #[test]
    fn net_plan_and_window_scale() {
        assert_eq!(slot_ip(2), [10, 100, 0, 3]);
        assert_eq!(host_ip(), [10, 100, 0, 1]);
        assert_eq!(mac_of(0), Mac([2, 0, 0, 0, 0, 0]));
        assert_eq!(ws_shift_for(0xffff), 0);
        assert_eq!(ws_shift_for(0x1_0000), 1);
        assert_eq!(ws_shift_for(1 << 20), 5);
        assert_eq!(ws_shift_for(u64::MAX), 14);
    }

    #[test]
    fn durations_in_the_rxpoll_forms() {
        assert_eq!(parse_duration("300us"), Some(Duration::from_micros(300)));
        assert_eq!(parse_duration("5ms"), Some(Duration::from_millis(5)));
        assert_eq!(parse_duration("1s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_duration("7ns"), Some(Duration::from_nanos(7)));
        assert_eq!(parse_duration("5"), None);
        assert_eq!(parse_duration("ms"), None);
        assert_eq!(parse_duration("5h"), None);
    }
}
