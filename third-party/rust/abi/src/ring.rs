//! De SPSC-ring van de slot-ABI over gedeeld geheugen: één schrijver en één
//! lezer per richting, lock-vrij met monotone indexen.
//!
//! Indeling (alle velden 64-bit, gealigneerd):
//!
//! ```text
//! +0x00 head   producer-index (bytes, monotoon oplopend)
//! +0x10 size   datacapaciteit in bytes (door de kern gezet bij init)
//! +0x40 tail   consumer-index
//! +0x80 data   [size]u8, circulair
//! ```
//!
//! Records: een kop van 8 bytes `{len: u32, kind: u32}` plus payload,
//! opgevuld tot een 8-voud. Een record wrapt nooit: past hij niet meer
//! aaneengesloten, dan vult een PAD-record de staart en begint het record
//! vooraan.
//!
//! De twee kanten zijn twee typen: [`Writer`] (de producer) en [`Reader`]
//! (de consument). Elk heeft één eigenaar; wie een ring wil lezen, is de
//! lezer of vraagt het de lezer. De barrières en het cache-onderhoud zitten
//! hier en nergens anders (handboek §5): `push` na het schrijven van een
//! record en vóór het ophogen van de kop, `pull` vóór het lezen.
//!
//! Wat hier NIET staat: waar een ring woont (dat is [`crate::layout`]) en wie
//! gewekt wordt (dat beslist de aanroeper met de `bool` van
//! [`Writer::write`]).
//!
//! Vervallen uit Go: de recordtypes 3 en 4 (de mailbox-RPC; sinds Go-ABI 6
//! loopt elke call over [`crate::systemapi`]), de heap-ring `New` (geen heap
//! in de ABI), en de `coherent`/`peeked`-snelpaden: `dev` kent nog geen
//! register van gecachte vensters, dus deze ring doet altijd `push`/`pull`.
//! Dat is correct op elk ijzer; de prijs (gemeten 03-09 in Go: drie DSB's
//! per frame op een coherente ring) komt terug als meting erom vraagt.

use crate::{Error, Result};
use core::fmt;
use dev::Pa;

/// De offset van het head-woord in de ringkop.
///
/// Head en tail liggen elk in hun EIGEN cacheline, en dat is een harde eis:
/// op een architectuur zonder coherente harten schrijft de producer bij zijn
/// cache-clean de hele regel terug, inclusief zijn verouderde kopie van
/// tail. Gemeten 30-07 op de LicheeRV: kleine frames kwamen door, maar
/// zodra beide kanten tegelijk hamerden (een download van 5 MB) stond de
/// ring binnen één segment stil.
pub const HEAD_OFF: u64 = 0x00;
/// De offset van het maatwoord: informatief, zie [`Writer::open`].
pub const SIZE_OFF: u64 = 0x10;
/// De offset van het tail-woord, in zijn eigen cacheline.
pub const TAIL_OFF: u64 = 0x40;
/// De offset van de data.
pub const DATA_OFF: u64 = 0x80;
/// De maat van een recordkop.
pub const REC_HDR: u64 = 8;

const _: () = assert!(HEAD_OFF / dev::LINE != TAIL_OFF / dev::LINE);
const _: () = assert!(SIZE_OFF / dev::LINE != TAIL_OFF / dev::LINE);
const _: () = assert!(TAIL_OFF + 8 <= DATA_OFF && DATA_OFF.is_multiple_of(dev::LINE));

/// De soort van een record. Nul is PAD en komt nooit bij een gebruiker.
///
/// # Invariants
///
/// `self.0 != 0`.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Kind(u32);

impl Kind {
    /// App naar kern (outbox): een logregel.
    pub const LOG: Kind = Kind(1);
    /// Frame-ringen: één rauw Ethernet-frame.
    pub const FRAME: Kind = Kind(5);

    /// De opvulling tot het einde van de databuffer.
    const PAD: u32 = 0;

    /// Een soort uit een rauw getal, of `None` voor PAD (0). De getallen 3
    /// en 4 (de vervallen mailbox-RPC) zijn geldig maar worden door geen
    /// kant meer geschreven; een lezer die ze ziet, logt een verdwaald
    /// record.
    #[must_use]
    pub const fn new(raw: u32) -> Option<Kind> {
        if raw == Self::PAD {
            return None;
        }
        // INVARIANT: niet PAD.
        Some(Kind(raw))
    }

    /// Het rauwe getal.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Rondt `n` op naar een 8-voud (verzadigend: een reus past toch nergens).
const fn align8(n: u64) -> u64 {
    n.saturating_add(7) & !7
}

/// Toetst een backing uit de layout: de ring vertrouwt alleen de maat van
/// zijn eigenaar, nooit het maatwoord in gedeeld geheugen.
fn check_backing(base: Pa, size: u64) -> Result {
    let fits = base
        .0
        .checked_add(DATA_OFF)
        .and_then(|d| d.checked_add(size))
        .is_some();
    if size < 2 * REC_HDR || !size.is_multiple_of(8) || !base.is_aligned(8) || !fits {
        return Err(Error::RingBacking { base: base.0, size });
    }
    Ok(())
}

/// Maakt een lege ring met datacapaciteit `size` klaar op `base`. De kern
/// roept dit aan vóór de app start; `size` is een 8-voud.
pub fn init(base: Pa, size: u64) -> Result {
    check_backing(base, size)?;
    dev::clear(base, DATA_OFF as usize);
    dev::write64(base.add(SIZE_OFF), size);
    // De HELE kop publiceren, niet alleen het maatwoord: head en tail liggen
    // elk in hun eigen cacheline, dus een push van 8 bytes liet die twee
    // ongepubliceerd en een niet-coherente tegenpartij las dan wat er
    // toevallig in DRAM stond in plaats van de nul van `clear`.
    dev::push(base, DATA_OFF as usize);
    dev::mb();
    Ok(())
}

/// De gedeelde geometrie van één ring.
#[derive(Copy, Clone, Debug)]
struct Geom {
    base: Pa,
    size: u64,
}

impl Geom {
    fn head(self) -> u64 {
        dev::pull(self.base.add(HEAD_OFF), 8);
        dev::read64(self.base.add(HEAD_OFF))
    }

    fn tail(self) -> u64 {
        dev::pull(self.base.add(TAIL_OFF), 8);
        dev::read64(self.base.add(TAIL_OFF))
    }

    fn set_head(self, v: u64) {
        dev::write64(self.base.add(HEAD_OFF), v);
        dev::push(self.base.add(HEAD_OFF), 8);
    }

    fn set_tail(self, v: u64) {
        dev::write64(self.base.add(TAIL_OFF), v);
        dev::push(self.base.add(TAIL_OFF), 8);
    }

    /// Het adres van byte-index `off` in de data.
    fn at(self, off: u64) -> Pa {
        self.base.add(DATA_OFF + off % self.size)
    }
}

/// De producer-kant van een ring.
#[derive(Debug)]
pub struct Writer {
    g: Geom,
}

impl Writer {
    /// Koppelt aan een door [`init`] klaargezette ring met de capaciteit van
    /// de eigenaar. `base` en `size` komen uit de vertrouwde layout: het
    /// maatwoord in gedeeld geheugen kan de tegenpartij wijzigen en mag onze
    /// fysieke grenzen nooit bepalen.
    pub fn open(base: Pa, size: u64) -> Result<Writer> {
        check_backing(base, size)?;
        Ok(Writer {
            g: Geom { base, size },
        })
    }

    /// Past een record met payload van `n` bytes ooit in deze ring?
    /// [`Writer::write`] weigert records groter dan de halve buffer blijvend,
    /// dus wie herprobeert tot het lukt, toetst dit eerst.
    #[must_use]
    pub fn fits(&self, n: usize) -> bool {
        REC_HDR + align8(n as u64) <= self.g.size / 2
    }

    /// Schrijft één recordkop en publiceert hem meteen. ÉLKE kop loopt
    /// hierlangs, een echt record én een PAD: in Go had de PAD-kop geen
    /// push, en een download van 5 MB stopte op 926360 bytes, precies bij
    /// de eerste wrap van de 978944-byte RX-ring (LicheeRV, 31-07).
    fn put_hdr(&mut self, off: u64, len: u64, kind: u32) {
        let at = self.g.at(off);
        dev::write64(at, len | u64::from(kind) << 32);
        dev::push(at, REC_HDR as usize);
    }

    /// Plaatst een record. `Ok(true)` betekent dat de ring vóór deze schrijf
    /// leeg was: de producer maakte de overgang leeg naar niet-leeg en moet
    /// de tegenpartij één keer wekken. Dat is het doorbell-contract van elke
    /// gebruiker; hoe er gewekt wordt, weet de aanroeper.
    pub fn write(&mut self, kind: Kind, p: &[u8]) -> Result<bool> {
        let size = self.g.size;
        let need = REC_HDR + align8(p.len() as u64);
        if need > size / 2 {
            return Err(Error::RecordTooLarge {
                need,
                max: size / 2,
            });
        }
        let (mut head, tail) = (self.g.head(), self.g.tail());
        let used = head.wrapping_sub(tail);
        if used > size {
            // Onmogelijke indexen (een malafide consument): niets schrijven.
            return Err(Error::RingIndices { head, tail });
        }
        let was_empty = head == tail;

        let contig = size - head % size;
        if need > contig {
            // Een verzonnen head (de indexen leven in geheugen dat de andere
            // kant kan beschrijven) laat `contig - REC_HDR` underflowen.
            if contig < REC_HDR {
                return Err(Error::RingIndices { head, tail });
            }
            if size - used < contig + need {
                return Err(Error::RingFull {
                    need: contig + need,
                    free: size - used,
                });
            }
            self.put_hdr(head, contig - REC_HDR, Kind::PAD);
            head = head.wrapping_add(contig);
        }
        let used = head.wrapping_sub(tail);
        if size - used < need {
            return Err(Error::RingFull {
                need,
                free: size - used,
            });
        }

        self.put_hdr(head, p.len() as u64, kind.0);
        let at = self.g.at(head).add(REC_HDR);
        dev::copy_in(at, p);
        dev::push(at, align8(p.len() as u64) as usize);
        dev::mb(); // Payload gepubliceerd vóór de index.
        self.g.set_head(head.wrapping_add(need));
        Ok(was_empty)
    }
}

/// Waarom een lezer zijn ring corrupt verklaarde, met de meting van dát
/// moment. Zonder de getallen is een corrupte ring van buiten niet te
/// onderscheiden van een lege, en dat onderscheid was de jacht van 17-08
/// (boot 9: slot-TX leest eeuwig leeg terwijl de app schrijft).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Corrupt {
    /// Meer gepubliceerd dan de buffer groot is: een verzonnen head.
    HeadAhead {
        /// De producer-index.
        head: u64,
        /// De consumer-index.
        tail: u64,
        /// De capaciteit.
        size: u64,
    },
    /// De recordkop zelf ligt voorbij de datarand: een verzonnen tail.
    HeaderPastEdge {
        /// De consumer-index.
        tail: u64,
        /// De capaciteit.
        size: u64,
    },
    /// Een kop die buiten de gepubliceerde bytes, de datarand of de buffer
    /// van de lezer claimt.
    BadHeader {
        /// Het kopwoord.
        hdr: u64,
        /// De producer-index.
        head: u64,
        /// De consumer-index.
        tail: u64,
        /// De capaciteit.
        size: u64,
        /// De buffer van de lezer.
        buf: usize,
    },
}

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::HeadAhead { head, tail, size } => write!(
                f,
                "head-tail>size (head={head:#x} tail={tail:#x} size={size:#x})"
            ),
            Self::HeaderPastEdge { tail, size } => {
                write!(f, "header past data edge (tail={tail:#x} size={size:#x})")
            }
            Self::BadHeader {
                hdr,
                head,
                tail,
                size,
                buf,
            } => write!(
                f,
                "impossible header (hdr={hdr:#x} len={} kind={} head={head:#x} tail={tail:#x} size={size:#x} buf={buf})",
                hdr as u32,
                hdr >> 32
            ),
        }
    }
}

/// Eén gelezen record: de soort en de payload in de buffer van de lezer.
#[derive(Debug, PartialEq, Eq)]
pub struct Record<'b> {
    /// De soort.
    pub kind: Kind,
    /// De payload.
    pub payload: &'b [u8],
}

/// De stand van een ring in getallen, voor een lezer die "pending" ziet
/// maar niets kan lezen.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Snapshot {
    /// De producer-index.
    pub head: u64,
    /// De consumer-index.
    pub tail: u64,
    /// De capaciteit.
    pub size: u64,
    /// Het kopwoord op de tail (0 als er niets ligt of het niet te lezen
    /// is).
    pub hdr: u64,
    /// De corrupt-reden.
    pub corrupt: Option<Corrupt>,
}

impl fmt::Display for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "head={:#x} tail={:#x} size={:#x} hdr@tail={:#x}",
            self.head, self.tail, self.size, self.hdr
        )?;
        match self.corrupt {
            Some(c) => write!(f, " corrupt={c}"),
            None => Ok(()),
        }
    }
}

/// De consument-kant van een ring.
///
/// De ringinhoud komt van de producer en is onvertrouwd. Een producer mag de
/// consument nooit tot een reuzenkopie of een eindeloze PAD-skip verleiden;
/// wat niet klopt, verklaart de ring corrupt, en een corrupte ring levert
/// definitief niets meer. De enige uitweg is een verse [`init`] door de kern
/// (slot-herstart).
#[derive(Debug)]
pub struct Reader {
    g: Geom,
    corrupt: Option<Corrupt>,
}

impl Reader {
    /// Koppelt aan een door [`init`] klaargezette ring; zie
    /// [`Writer::open`].
    pub fn open(base: Pa, size: u64) -> Result<Reader> {
        check_backing(base, size)?;
        Ok(Reader {
            g: Geom { base, size },
            corrupt: None,
        })
    }

    /// De corrupt-reden, als de ring dood is.
    #[must_use]
    pub fn corrupt(&self) -> Option<Corrupt> {
        self.corrupt
    }

    /// Is de ring corrupt verklaard?
    #[must_use]
    pub fn is_corrupt(&self) -> bool {
        self.corrupt.is_some()
    }

    /// Zet de reden; de eerste wint, want de vervolgstaat van een corrupte
    /// ring is geen nieuwe informatie.
    fn mark(&mut self, why: Corrupt) {
        if self.corrupt.is_none() {
            self.corrupt = Some(why);
        }
    }

    /// De producer-index en of er ongelezen records liggen: de twee
    /// ingrediënten van de doorbell (de wek-drempel en het wek-besluit).
    ///
    /// Een waarnemer kan head lezen vóór een andere core de records
    /// consumeert en tail voorbij die head schuift. Alleen een begrensde
    /// voorwaartse afstand telt dus als werk; de onvoorwaardelijke afstand
    /// houdt de omloop van de teller heel.
    #[must_use]
    pub fn head_pending(&self) -> (u64, bool) {
        let h = self.g.head();
        let n = h.wrapping_sub(self.g.tail());
        (h, n != 0 && n <= self.g.size)
    }

    /// Haalt het volgende record en kopieert de payload in `buf`, die door
    /// de lezer hergebruikt wordt (geen allocatie per record). `None` als de
    /// ring leeg of corrupt is. PAD-records worden overgeslagen. `buf` moet
    /// minstens één maximaal record kunnen dragen.
    pub fn read_into<'b>(&mut self, buf: &'b mut [u8]) -> Option<Record<'b>> {
        if self.corrupt.is_some() {
            return None;
        }
        let size = self.g.size;
        loop {
            let (head, tail) = (self.g.head(), self.g.tail());
            if head == tail {
                return None;
            }
            let avail = head.wrapping_sub(tail);
            // Meer gepubliceerd dan de buffer groot is kan alleen met een
            // verzonnen head, en die zou de skip-lus hieronder miljarden
            // ronden gunnen (livelock op de kern-core).
            if avail > size {
                self.mark(Corrupt::HeadAhead { head, tail, size });
                return None;
            }
            // De kop zelf moet vóór de datarand liggen, en dat moet vóór de
            // lees vaststaan: een verzonnen tail (size-7) las anders 1-7
            // bytes voorbij de databuffer.
            if tail % size > size - REC_HDR {
                self.mark(Corrupt::HeaderPastEdge { tail, size });
                return None;
            }
            dev::mb(); // Index gezien, dan pas de payload.

            let at = self.g.at(tail);
            dev::pull(at, REC_HDR as usize);
            let hdr = dev::read64(at);
            let len = u64::from(hdr as u32);
            let raw = (hdr >> 32) as u32;
            let need = REC_HDR + align8(len);
            if need > avail || need > size - tail % size || len > buf.len() as u64 {
                self.mark(Corrupt::BadHeader {
                    hdr,
                    head,
                    tail,
                    size,
                    buf: buf.len(),
                });
                return None;
            }
            let Some(kind) = Kind::new(raw) else {
                dev::mb(); // Kop gelezen vóór de ruimte vrijgeven.
                self.g.set_tail(tail.wrapping_add(need));
                continue;
            };
            let n = len as usize;
            let at = at.add(REC_HDR);
            dev::pull(at, n);
            let (payload, _) = buf.split_at_mut(n);
            dev::copy_out(payload, at);
            dev::mb(); // Payload gekopieerd vóór de ruimte vrijgeven.
            self.g.set_tail(tail.wrapping_add(need));
            return Some(Record { kind, payload });
        }
    }

    /// De stand van de ring in getallen.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let (head, tail, size) = (self.g.head(), self.g.tail(), self.g.size);
        let mut hdr = 0;
        if head != tail && tail % size <= size - REC_HDR {
            let at = self.g.at(tail);
            dev::pull(at, REC_HDR as usize);
            hdr = dev::read64(at);
        }
        Snapshot {
            head,
            tail,
            size,
            hdr,
            corrupt: self.corrupt,
        }
    }
}

#[cfg(test)]
mod tests;
