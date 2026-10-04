//! De codec-client: een stream door de hardwaredecoder van de node, zonder
//! dat er één beeld over de verbinding gaat (Go: de codec-kant van
//! `app/applib`).
//!
//! Een 4K-beeld in P010 is 24 MB; bij 24 fps is dat meer dan het slot-LAN
//! draagt. De calls dragen dus AANWIJZINGEN: een buffer is een stuk van de
//! eigen partitie, genoemd als afstand vanaf `RamStart`
//! ([`crate::App::ram_start`]) en hele pagina's van 4 KB. De kern toetst het,
//! hangt het in de page tables van de codec, en doet het cache-onderhoud;
//! de app schrijft bitstream in haar eigen geheugen en leest er beelden uit.
//!
//! Alle ops zijn niet idempotent (twee keer dezelfde feed is twee happen
//! bitstream), dus deze client herhaalt nooit ([`Client::call_once`]). Na een
//! kern-flip bestaan de sessies niet meer; een call geeft dan `NotFound` en
//! de app opent opnieuw.
//!
//! Een [`Session`] is een getal, geen eigenaar: laat de app hem vallen
//! zonder [`Session::close`], dan sluit de kern hem pas als de app stopt.
//!
//! # Examples
//!
//! ```no_run
//! use applib::codec::{Codec, Config, Direction, Event, Flags, Kind, Pixel, Session};
//! use applib::sys::{Client, Dial, Result, Timer};
//! use core::time::Duration;
//!
//! /// Eén hap HEVC op 1 MB in de partitie, twaalf beeldbuffers erachter.
//! async fn decode<D: Dial, T: Timer>(c: &mut Client<D, T>, len: u64) -> Result {
//!     let cfg = Config {
//!         codec: Codec::Hevc,
//!         dir: Direction::Decode,
//!         pixel: Pixel::P010,
//!         width: 0,
//!         height: 0,
//!     };
//!     let s = Session::open(c, &cfg).await?;
//!     s.feed(c, 1 << 20, 1 << 20, len, Flags::EOS, 0).await?;
//!     let mut evs = [Event::default(); 32];
//!     loop {
//!         let n = s.poll(c, &mut evs, Duration::from_secs(1)).await?;
//!         for e in evs.iter().take(n) {
//!             match e.kind {
//!                 // De maat is bekend: beeldbuffers van e.size aanbieden.
//!                 Kind::Format => {
//!                     for i in 0..12 {
//!                         s.offer(c, (4 << 20) + i * e.size, e.size).await?;
//!                     }
//!                 }
//!                 // Een beeld op e.off; na gebruik opnieuw aanbieden.
//!                 Kind::Produced => s.offer(c, e.off, e.size).await?,
//!                 Kind::Done | Kind::Fault => return s.close(c).await,
//!                 _ => {}
//!             }
//!         }
//!     }
//! }
//! ```

use crate::contract::STATUS_OK;
use crate::sys::{Client, Dial, Error, Req, Result, Timer};
use abi::hopabi::codec::{BufArgs, EVENT_LEN, FeedArgs, OpenArgs};
use abi::hopabi::{OP_CODEC_CLOSE, OP_CODEC_FEED, OP_CODEC_OFFER, OP_CODEC_OPEN, OP_CODEC_POLL};
use core::time::Duration;
pub use driver_codec::{Codec, Config, Direction, Flags, Pixel};

/// Eén event en zijn soort: de vorm van het contract. `off` is de afstand
/// vanaf `RamStart`, `pixel` het getal van [`Pixel`] (`Pixel::from_raw`).
pub use abi::hopabi::codec::{Event, Kind};

/// De timeout van een codec-call. Een open laadt firmware (een paar honderd
/// KB van de NVMe); de rest is een beurt van de driver.
pub const CODEC_TIMEOUT: Duration = Duration::from_secs(10);

/// Hoeveel events één poll hoogstens oplevert (de kern stopt bij 32).
pub const MAX_EVENTS: usize = 32;

/// Een open codec-sessie: het handvat van de kern.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Session {
    handle: u32,
}

/// Eén call zonder pad en zonder herhaling.
async fn call<D: Dial, T: Timer>(
    c: &mut Client<D, T>,
    op: u8,
    off: u64,
    n: u64,
    data: &[u8],
    dst: &mut [u8],
) -> Result<(u64, usize)> {
    let req = Req {
        op,
        seq: 0,
        off,
        n,
        path: "",
        data,
    };
    let (r, len) = c.call_once(req, dst, CODEC_TIMEOUT).await?;
    if r.status != STATUS_OK {
        return Err(Error::Protocol("codec status"));
    }
    Ok((r.size, len))
}

impl Session {
    /// Opent een sessie. Bij decode zijn breedte en hoogte een hint; de maat
    /// komt terug als [`Kind::Format`].
    pub async fn open<D: Dial, T: Timer>(c: &mut Client<D, T>, cfg: &Config) -> Result<Session> {
        let a = OpenArgs {
            codec: cfg.codec as u8,
            dir: cfg.dir as u8,
            pixel: cfg.pixel as u8,
            width: u16::try_from(cfg.width).unwrap_or(u16::MAX),
            height: u16::try_from(cfg.height).unwrap_or(u16::MAX),
        }
        .encode();
        let (h, _) = call(c, OP_CODEC_OPEN, 0, 0, &a, &mut []).await?;
        let handle = u32::try_from(h).map_err(|_| Error::Protocol("codec handle"))?;
        Ok(Session { handle })
    }

    /// Voert `filled` bytes bitstream in uit de buffer `[off, off + len)`.
    /// De buffer is van het ijzer tot hij als [`Kind::Consumed`] terugkomt.
    pub async fn feed<D: Dial, T: Timer>(
        &self,
        c: &mut Client<D, T>,
        off: u64,
        len: u64,
        filled: u64,
        flags: Flags,
        tag: u64,
    ) -> Result {
        let a = FeedArgs {
            handle: self.handle,
            flags: flags.0,
            filled,
            tag,
        }
        .encode();
        call(c, OP_CODEC_FEED, off, len, &a, &mut [])
            .await
            .map(|_| ())
    }

    /// Biedt een lege buffer aan voor een resultaat.
    pub async fn offer<D: Dial, T: Timer>(
        &self,
        c: &mut Client<D, T>,
        off: u64,
        len: u64,
    ) -> Result {
        let a = BufArgs {
            handle: self.handle,
        }
        .encode();
        call(c, OP_CODEC_OFFER, off, len, &a, &mut [])
            .await
            .map(|_| ())
    }

    /// Haalt alles op wat klaarstaat (hoogstens `dst.len()` en
    /// [`MAX_EVENTS`]); geeft het aantal. Ligt er niets, dan wacht de kern
    /// tot er iets is, hoogstens `wait` (en een seconde): de app slaapt op
    /// het antwoord in plaats van zelf rond te kijken. `Duration::ZERO`
    /// antwoordt meteen.
    pub async fn poll<D: Dial, T: Timer>(
        &self,
        c: &mut Client<D, T>,
        dst: &mut [Event],
        wait: Duration,
    ) -> Result<usize> {
        let a = BufArgs {
            handle: self.handle,
        }
        .encode();
        let ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        let mut raw = [0u8; MAX_EVENTS * EVENT_LEN];
        let (n, len) = call(c, OP_CODEC_POLL, 0, ms, &a, &mut raw).await?;
        let got = usize::try_from(n).unwrap_or(0).min(len / EVENT_LEN);
        let mut k = 0;
        for (i, d) in dst.iter_mut().enumerate().take(got) {
            *d = raw
                .get(i * EVENT_LEN..(i + 1) * EVENT_LEN)
                .and_then(|b| Event::decode(b).ok())
                .ok_or(Error::Protocol("codec event"))?;
            k += 1;
        }
        Ok(k)
    }

    /// Sluit de sessie; aangeboden buffers zijn daarna weer van de app.
    pub async fn close<D: Dial, T: Timer>(self, c: &mut Client<D, T>) -> Result {
        let a = BufArgs {
            handle: self.handle,
        }
        .encode();
        call(c, OP_CODEC_CLOSE, 0, 0, &a, &mut []).await.map(|_| ())
    }

    /// Het handvat van de kern.
    #[must_use]
    pub fn handle(&self) -> u32 {
        self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::SYS_HEADER_LEN;
    use crate::sys::{Conn, ConnError};
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    fn block_on<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..1000 {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
        panic!("future bleef hangen");
    }

    /// De kern van de test: wat hij terugstuurt, en wat hij kreeg.
    #[derive(Default)]
    struct Kern {
        rx: VecDeque<u8>,
        sent: Vec<u8>,
        reset_after: Option<usize>,
    }

    struct Pipe(Rc<RefCell<Kern>>);

    impl Conn for Pipe {
        async fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, ConnError> {
            let mut k = self.0.borrow_mut();
            let n = buf.len().min(k.rx.len());
            for b in buf.iter_mut().take(n) {
                *b = k.rx.pop_front().unwrap();
            }
            if n == 0 {
                return Err(ConnError::Reset);
            }
            Ok(n)
        }
        async fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, ConnError> {
            let mut k = self.0.borrow_mut();
            if k.reset_after.is_some_and(|m| k.sent.len() >= m) {
                return Err(ConnError::Reset);
            }
            k.sent.extend_from_slice(buf);
            Ok(buf.len())
        }
    }

    struct PipeDial(Rc<RefCell<Kern>>, u32);

    impl Dial for PipeDial {
        type Conn = Pipe;
        async fn dial(&mut self) -> core::result::Result<Pipe, ConnError> {
            self.1 += 1;
            Ok(Pipe(self.0.clone()))
        }
    }

    struct Now;
    impl Timer for Now {
        fn sleep(&self, _: Duration) -> impl Future<Output = ()> {
            core::future::pending()
        }
    }

    fn answer(k: &Rc<RefCell<Kern>>, seq: u32, status: u16, size: u64, data: &[u8]) {
        let f = crate::sys::tests::resp(seq, status, size, data);
        k.borrow_mut().rx.extend(f);
    }

    /// Het request dat de kern kreeg: op, off, n en data.
    fn last_req(k: &Rc<RefCell<Kern>>) -> (u8, u64, u64, Vec<u8>) {
        let s = &k.borrow().sent;
        let p = &s[SYS_HEADER_LEN..];
        let le = |i: usize| u64::from_le_bytes(p[i..i + 8].try_into().unwrap());
        (p[1], le(8), le(16), p[24..].to_vec())
    }

    #[test]
    fn open_feed_poll_close_speak_the_wire_of_the_kern() {
        let k = Rc::new(RefCell::new(Kern::default()));
        let mut c = Client::new(PipeDial(k.clone(), 0), Now);
        let cfg = Config {
            codec: Codec::Hevc,
            dir: Direction::Decode,
            pixel: Pixel::P010,
            width: 3840,
            height: 2160,
        };
        answer(&k, 1, 0, 7, &[]);
        let s = block_on(Session::open(&mut c, &cfg)).unwrap();
        assert_eq!(s.handle(), 7);
        let (op, _, _, data) = last_req(&k);
        assert_eq!(op, OP_CODEC_OPEN);
        assert_eq!(OpenArgs::decode(&data).unwrap().pixel, Pixel::P010 as u8);

        k.borrow_mut().sent.clear();
        answer(&k, 2, 0, 100, &[]);
        block_on(s.feed(&mut c, 1 << 20, 8192, 100, Flags::EOS, 42)).unwrap();
        let (op, off, n, data) = last_req(&k);
        assert_eq!((op, off, n), (OP_CODEC_FEED, 1 << 20, 8192));
        let f = FeedArgs::decode(&data).unwrap();
        assert_eq!((f.handle, f.flags, f.filled, f.tag), (7, 1, 100, 42));

        let mut w = [0u8; 2 * EVENT_LEN];
        Event {
            kind: Kind::Format,
            size: 24 << 20,
            bytes: 6,
            pixel: 4,
            width: 3840,
            ..Event::default()
        }
        .encode(&mut w[..EVENT_LEN])
        .unwrap();
        Event {
            kind: Kind::Consumed,
            off: 1 << 20,
            size: 8192,
            tag: 42,
            ..Event::default()
        }
        .encode(&mut w[EVENT_LEN..])
        .unwrap();
        k.borrow_mut().sent.clear();
        answer(&k, 3, 0, 2, &w);
        let mut evs = [Event::default(); 4];
        assert_eq!(
            block_on(s.poll(&mut c, &mut evs, Duration::from_millis(250))).unwrap(),
            2
        );
        let (op, _, n, _) = last_req(&k);
        assert_eq!(
            (op, n),
            (OP_CODEC_POLL, 250),
            "the wait travels in n, in ms"
        );
        assert_eq!(
            (evs[0].kind, evs[0].size, evs[0].bytes),
            (Kind::Format, 24 << 20, 6)
        );
        assert_eq!(Pixel::from_raw(evs[0].pixel), Pixel::P010);
        assert_eq!(
            (evs[1].kind, evs[1].off, evs[1].tag),
            (Kind::Consumed, 1 << 20, 42)
        );

        answer(&k, 4, 0, 0, &[]);
        block_on(s.close(&mut c)).unwrap();
    }

    /// Een feed die het transport verliest wordt NIET herhaald: de kern kan
    /// hem al gehad hebben, en twee keer is twee happen bitstream.
    #[test]
    fn a_codec_call_is_never_repeated() {
        let k = Rc::new(RefCell::new(Kern::default()));
        k.borrow_mut().reset_after = Some(0);
        let mut c = Client::new(PipeDial(k.clone(), 0), Now);
        let s = Session { handle: 1 };
        let r = block_on(s.feed(&mut c, 0, 4096, 1, Flags(0), 0));
        assert!(matches!(r, Err(Error::Transport(_))), "{r:?}");
        assert!(k.borrow().sent.is_empty());
    }
}
