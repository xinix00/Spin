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
//! in de ABI), en het `peeked`-snelpad. Go's `coherent` kwam terug als
//! [`Coherence`] (01-10), maar dan als eigenschap van de ring in plaats van
//! een register van gecachte vensters in `dev`: elke kant weet alleen hoe
//! híj de ring mapt, en zegt dat in zijn eigen regel van de ringkop
//! ([`PRODUCER_WB_OFF`], [`CONSUMER_WB_OFF`]). De payload gaat zonder
//! onderhoud zodra beide kanten het beloven. De standaard ([`Writer::open`],
//! [`Reader::open`]) belooft niets en houdt het onderhoud, dat op elk ijzer
//! correct is.

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
/// De offset van het write-back-woord van de producer ([`WB_WORD`]), in de
/// regel van head: alleen de producer schrijft die regel.
pub const PRODUCER_WB_OFF: u64 = 0x08;
/// De offset van het maatwoord: informatief, zie [`Writer::open`].
pub const SIZE_OFF: u64 = 0x10;
/// De offset van het tail-woord, in zijn eigen cacheline.
pub const TAIL_OFF: u64 = 0x40;
/// De offset van het write-back-woord van de consument, in de regel van
/// tail: alleen de consument schrijft die regel.
pub const CONSUMER_WB_OFF: u64 = 0x48;
/// Het woord waarmee een kant belooft dat hij de ring Normal write-back
/// inner shareable mapt, op een core in het coherente domein van de
/// tegenpartij ([`Coherence::Hardware`]). [`init`] wist beide woorden; een
/// kant van vóór dit woord (01-10) laat dus 0 staan en belooft niets.
pub const WB_WORD: u64 = u64::from_le_bytes(*b"HOPRB-WB");
/// De offset van de data.
pub const DATA_OFF: u64 = 0x80;
/// De maat van een recordkop.
pub const REC_HDR: u64 = 8;

const _: () = assert!(HEAD_OFF / dev::LINE != TAIL_OFF / dev::LINE);
const _: () = assert!(SIZE_OFF / dev::LINE != TAIL_OFF / dev::LINE);
const _: () = assert!(PRODUCER_WB_OFF / dev::LINE == HEAD_OFF / dev::LINE);
const _: () = assert!(CONSUMER_WB_OFF / dev::LINE == TAIL_OFF / dev::LINE);
const _: () = assert!(CONSUMER_WB_OFF + 8 <= DATA_OFF);
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

/// Hoe déze kant de ring mapt, gekozen bij het openen. De payload gaat
/// zonder cache-onderhoud alleen als beide kanten [`Coherence::Hardware`]
/// zijn: elke kant zet dan [`WB_WORD`] in zijn eigen regel van de ringkop,
/// en kijkt per record naar het woord van de tegenpartij (dat staat in de
/// regel die hij voor de index toch al leest, dus het kost niets). Zo
/// schakelt een kant van de kern vanzelf om zodra een app met zijn stage-1
/// aan de ring opent, en blijft hij bij onderhoud voor een app die dat niet
/// belooft (de MMU uit, een oude applib, RISC-V).
///
/// De indexwoorden (head en tail) doen in beide vormen hun `push`/`pull`:
/// ze zijn de publicatie, het zijn twee regels per record, en de RX-kop
/// van een app wordt door de EL2-switcher met de MMU uit gelezen
/// (`CTX_RING_HEAD_PA`, cpu/src/el2/switch.rs). Het verschil zit in het
/// werk per byte.
///
/// Een kant die liegt, schaadt alleen zichzelf: de tegenpartij kopieert
/// dan oude bytes van of naar zíjn ring, en de kern controleert een frame
/// pas op zijn eigen kopie.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Coherence {
    /// Belooft niets. Cache-onderhoud per regel van [`dev::LINE`] bytes over
    /// kop en payload (`dc cvac` na een schrijf, `dc civac` vóór een lees op
    /// arm64; de T-Head-ops op de C906) en vluchtige 8-byte-woorden. Correct
    /// tegen elke mapping van de tegenpartij: Device (de pool van de kern op
    /// Apple), Normal-NC, een app met de MMU uit, een niet-coherent hart.
    Maintained,
    /// Deze kant mapt de ring Normal write-back inner shareable op een core
    /// in het coherente domein van arm64; op RISC-V: deze kant draait op
    /// hetzelfde hart als de tegenpartij (een bewoner van de OS-core en de
    /// kern, `hopabi::CTRL_HART`), dus in één cache. Belooft de tegenpartij hetzelfde,
    /// dan zien de cores elkaars caches en is onderhoud loos werk; erger,
    /// een `dc civac` vóór de lees gooit de regels uit de gedeelde L2, zodat
    /// de kopie erna uit DRAM komt (O6N 01-10: app naar app van 545 naar
    /// 1050 MB/s toen ook de kern-kant zo kopieerde). De kopie is dan een
    /// gewone `memcpy` ([`dev::copy_in_normal`]).
    ///
    /// Wat een tegenpartij zonder belofte aan onderhoud blijft doen, is
    /// onschadelijk zolang ze werkelijk write-back mapt: haar `dc civac` op
    /// inner-shareable geheugen is een broadcast die onze vuile regel eerst
    /// naar het geheugen schrijft, en haar `dc cvac` laat de regel geldig in
    /// haar cache. De barrières blijven in beide vormen: ordening tussen
    /// payload en index is geen cache-vraag.
    Hardware,
}

/// Zet de belofte van deze kant in zijn eigen regel van de ringkop. Alleen
/// [`Coherence::Hardware`] schrijft: [`init`] wiste het woord al, en een kant
/// zonder belofte raakt de kop dan niet aan (op een niet-coherent hart zou
/// zijn clean van de regel anders een oude kopie terugschrijven).
fn promise(base: Pa, off: u64, local: Coherence) {
    if local == Coherence::Hardware {
        dev::write64(base.add(off), WB_WORD);
        dev::push(base.add(off), 8);
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

    /// Zet de kop en cleant hem bij elk record: een core die op EL2 slaapt
    /// peekt na zijn wekker de kop in DRAM (T30, 04-09: zonder de clean
    /// zakte schrijven van 690 naar 40 MB/s).
    fn set_head(self, v: u64) {
        dev::write64(self.base.add(HEAD_OFF), v);
        dev::push(self.base.add(HEAD_OFF), 8);
    }

    fn set_tail(self, v: u64) {
        dev::write64(self.base.add(TAIL_OFF), v);
        dev::push(self.base.add(TAIL_OFF), 8);
    }

    /// De index van de tegenpartij op `off` (head of tail), met haar belofte
    /// op `wb_off` in dezelfde regel: is deze kant `hw` en belooft zij ook,
    /// dan zien de twee kanten elkaars cache (arm64 inner shareable WB, of
    /// op RISC-V hetzelfde hart, `hopabi::CTRL_HART`) en is een gewone lees
    /// genoeg; anders eerst de regel vers (`pull`). Geeft de index en of de
    /// payload zonder onderhoud kan. 03-10: op de C906 was die pull per
    /// record een `th.dcache.cipa` met `th.sync.is`, aan beide kanten.
    /// Een woord dat de tegenpartij niet schreef, is na [`init`] 0 in de
    /// regel van deze kant, dus zonder belofte valt hij naar de `pull`.
    fn peer(self, off: u64, wb_off: u64, hw: bool) -> (u64, bool) {
        if hw && dev::read64(self.base.add(wb_off)) == WB_WORD {
            return (dev::read64(self.base.add(off)), true);
        }
        dev::pull(self.base.add(off), 8);
        (dev::read64(self.base.add(off)), hw && self.peer_wb(wb_off))
    }

    /// Belooft de tegenpartij write-back? Direct na [`Geom::head`] of
    /// [`Geom::tail`]: haar woord staat in de regel die daar net vers
    /// gelezen is.
    fn peer_wb(self, off: u64) -> bool {
        dev::read64(self.base.add(off)) == WB_WORD
    }

    /// Het adres van byte-index `off` in de data.
    fn at(self, off: u64) -> Pa {
        self.base.add(DATA_OFF + off % self.size)
    }
}

/// Ruimte voor één record, voorbij de gepubliceerde head.
struct Reserved {
    /// Waar de recordkop komt.
    head: u64,
    /// Was de ring leeg vóór deze schrijf (het wek-contract van `write`)?
    was_empty: bool,
    /// Zonder cache-onderhoud (zie [`Coherence`]).
    hw: bool,
}

/// De producer-kant van een ring.
#[derive(Debug)]
pub struct Writer {
    g: Geom,
    local: Coherence,
    /// De eigen index, één keer vers gelezen bij het openen: alleen deze
    /// kant schrijft hem, dus zijn waarde staat hier en niet in een regel
    /// die per record geveegd moet worden (kfifo houdt zijn eigen `in` ook
    /// lokaal). Scheelt per record een `pull` (03-10: op de C906 een
    /// `th.dcache.cipa` en een `th.sync.is`, op arm64 een `dc civac` met twee
    /// `dsb sy`).
    head: u64,
}

impl Writer {
    /// Koppelt aan een door [`init`] klaargezette ring met de capaciteit van
    /// de eigenaar. `base` en `size` komen uit de vertrouwde layout: het
    /// maatwoord in gedeeld geheugen kan de tegenpartij wijzigen en mag onze
    /// fysieke grenzen nooit bepalen.
    pub fn open(base: Pa, size: u64) -> Result<Writer> {
        Self::open_with(base, size, Coherence::Maintained)
    }

    /// Als [`Writer::open`], met de [`Coherence`] van deze kant; die gaat
    /// meteen de ringkop in.
    pub fn open_with(base: Pa, size: u64, local: Coherence) -> Result<Writer> {
        check_backing(base, size)?;
        promise(base, PRODUCER_WB_OFF, local);
        let g = Geom { base, size };
        Ok(Writer {
            g,
            local,
            head: g.head(),
        })
    }

    /// De belofte van deze kant ([`Coherence`]): bij `Hardware` mapt hij de
    /// ring Normal, en mag hij erin werken als in gewoon geheugen.
    #[must_use]
    pub fn coherence(&self) -> Coherence {
        self.local
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
    fn put_hdr(&mut self, off: u64, len: u64, kind: u32, hw: bool) {
        let at = self.g.at(off);
        dev::write64(at, len | u64::from(kind) << 32);
        if !hw {
            dev::push(at, REC_HDR as usize);
        }
    }

    /// Plaatst een record. `Ok(true)` betekent dat de ring vóór deze schrijf
    /// leeg was: de producer maakte de overgang leeg naar niet-leeg en moet
    /// de tegenpartij één keer wekken. Dat is het doorbell-contract van elke
    /// gebruiker; hoe er gewekt wordt, weet de aanroeper.
    pub fn write(&mut self, kind: Kind, p: &[u8]) -> Result<bool> {
        let r = self.reserve(p.len())?;
        let at = self.g.at(r.head).add(REC_HDR);
        if r.hw {
            // Het record ligt voorbij head: de lezer raakt het pas na de
            // publicatie, dus niemand leest mee met de kopie.
            dev::copy_in_normal(at, p);
        } else {
            dev::copy_in(at, p);
        }
        self.publish(&r, kind, p.len());
        Ok(r.was_empty)
    }

    /// Als [`Writer::write`], zonder kopie: `f` vult het record in de ring
    /// zelf. Hij krijgt `max` bytes en geeft hoeveel hij er schreef (meer
    /// dan `max` telt als `max`). `Ok(None)`: `f` schreef niets, er is geen
    /// record. Is er geen plaats voor `max` bytes, dan [`Error::RingFull`]
    /// zonder `f` te roepen; de aanroeper valt dan terug op een eigen buffer
    /// en [`Writer::write`].
    pub fn write_with(
        &mut self,
        kind: Kind,
        max: usize,
        f: impl FnOnce(&mut [u8]) -> usize,
    ) -> Result<Option<bool>> {
        self.write_with_kind(max, |p| Some((kind, f(p))))
    }

    /// Als [`Writer::write_with`], maar `f` kiest de soort pas als het record
    /// er staat (de host-poort van de kern ziet pas aan het frame of het het
    /// slot-LAN of de uplink op moet). `None` of lengte nul: geen record.
    pub fn write_with_kind(
        &mut self,
        max: usize,
        f: impl FnOnce(&mut [u8]) -> Option<(Kind, usize)>,
    ) -> Result<Option<bool>> {
        let r = self.reserve(max)?;
        let at = self.g.at(r.head).add(REC_HDR);
        // Voorbij head en dus van ons tot de publicatie (zie `write`).
        let Some((kind, n)) = dev::view_mut(at, max, f) else {
            return Ok(None);
        };
        let n = n.min(max);
        if n == 0 {
            // Een PAD die `reserve` schreef, ligt ook voorbij head: niet
            // gepubliceerd, en de volgende schrijf zet hem opnieuw.
            return Ok(None);
        }
        self.publish(&r, kind, n);
        Ok(Some(r.was_empty))
    }

    /// Zoekt aaneengesloten ruimte voor een payload van `len` bytes, met een
    /// PAD tot de rand als het record daar niet meer past. Schrijft nog
    /// niets zichtbaars: alles ligt voorbij de gepubliceerde head.
    #[inline(always)]
    fn reserve(&mut self, len: usize) -> Result<Reserved> {
        let size = self.g.size;
        let need = REC_HDR + align8(len as u64);
        if need > size / 2 {
            return Err(Error::RecordTooLarge {
                need,
                max: size / 2,
            });
        }
        let mut head = self.head;
        let (tail, hw) = self
            .g
            .peer(TAIL_OFF, CONSUMER_WB_OFF, self.local == Coherence::Hardware);
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
            self.put_hdr(head, contig - REC_HDR, Kind::PAD, hw);
            head = head.wrapping_add(contig);
        }
        let used = head.wrapping_sub(tail);
        if size - used < need {
            return Err(Error::RingFull {
                need,
                free: size - used,
            });
        }
        Ok(Reserved {
            head,
            was_empty,
            hw,
        })
    }

    /// Zet de kop van het record in [`Writer::reserve`]'s ruimte, maakt de
    /// payload van `len` bytes zichtbaar en publiceert head.
    #[inline(always)]
    fn publish(&mut self, r: &Reserved, kind: Kind, len: usize) {
        self.put_hdr(r.head, len as u64, kind.0, r.hw);
        if !r.hw {
            dev::push(self.g.at(r.head).add(REC_HDR), align8(len as u64) as usize);
        }
        dev::mb(); // Payload gepubliceerd vóór de index.
        self.head = r.head.wrapping_add(REC_HDR + align8(len as u64));
        // De clean van head is voor een lezer zonder cache-blik: de
        // EL2-switcher die met de MMU uit de RX-kop van een slapende app peekt
        // (arm64). Op RISC-V betekent `hw` hetzelfde hart (`CTRL_HART`), en
        // daar peekt alleen de kern zelf, in dezelfde cache.
        if r.hw && cfg!(target_arch = "riscv64") {
            dev::write64(self.g.base.add(HEAD_OFF), self.head);
        } else {
            self.g.set_head(self.head);
        }
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

/// Een getoetst record dat nog in de ring staat ([`Reader::next`]).
struct Next {
    /// De soort (nooit PAD).
    kind: Kind,
    /// De payload.
    at: Pa,
    /// De lengte van de payload.
    len: usize,
    /// De tail na dit record.
    end: u64,
    /// Zonder cache-onderhoud (zie [`Coherence`]).
    hw: bool,
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
    local: Coherence,
    /// De eigen index, zoals [`Writer`] zijn head: één keer vers gelezen,
    /// daarna van deze kant. Een tegenpartij die tail in gedeeld geheugen
    /// overschrijft, verandert zo niets meer aan wat de lezer leest.
    tail: u64,
}

impl Reader {
    /// Koppelt aan een door [`init`] klaargezette ring; zie
    /// [`Writer::open`].
    pub fn open(base: Pa, size: u64) -> Result<Reader> {
        Self::open_with(base, size, Coherence::Maintained)
    }

    /// Als [`Reader::open`], met de [`Coherence`] van deze kant; die gaat
    /// meteen de ringkop in. Ook een lezer die zijn producer niet vertrouwt
    /// (de kern tegenover een app) mag [`Coherence::Hardware`] kiezen: de
    /// kopie gaat naar zijn eigen buffer en leest elk byte één keer, en de
    /// kop leest hij één keer in een lokale waarde vóór hij hem toetst.
    pub fn open_with(base: Pa, size: u64, local: Coherence) -> Result<Reader> {
        check_backing(base, size)?;
        promise(base, CONSUMER_WB_OFF, local);
        let g = Geom { base, size };
        Ok(Reader {
            g,
            corrupt: None,
            local,
            tail: g.tail(),
        })
    }

    /// De belofte van deze kant; zie [`Writer::coherence`].
    #[must_use]
    pub fn coherence(&self) -> Coherence {
        self.local
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
        let (h, _) = self
            .g
            .peer(HEAD_OFF, PRODUCER_WB_OFF, self.local == Coherence::Hardware);
        let n = h.wrapping_sub(self.tail);
        (h, n != 0 && n <= self.g.size)
    }

    /// Haalt het volgende record en kopieert de payload in `buf`, die door
    /// de lezer hergebruikt wordt (geen allocatie per record). `None` als de
    /// ring leeg of corrupt is. PAD-records worden overgeslagen. `buf` moet
    /// minstens één maximaal record kunnen dragen.
    pub fn read_into<'b>(&mut self, buf: &'b mut [u8]) -> Option<Record<'b>> {
        let r = self.next(buf.len())?;
        let (payload, _) = buf.split_at_mut(r.len);
        if r.hw {
            // Gepubliceerd en nog niet vrijgegeven: de producer schrijft
            // hier pas weer na de vrijgave hieronder.
            dev::copy_out_normal(payload, r.at);
        } else {
            dev::pull(r.at, r.len);
            dev::copy_out(payload, r.at);
        }
        self.free(r.end, r.hw);
        Some(Record {
            kind: r.kind,
            payload,
        })
    }

    /// Als [`Reader::read_into`], zonder kopie: `f` leest de payload in de
    /// ring zelf, daarna gaat de ruimte terug. `max` is de grootste payload
    /// die de lezer aanneemt (een grotere kop maakt de ring corrupt, net als
    /// een te kleine `buf`). Een producer kan tijdens `f` in het record
    /// schrijven en verandert dan wat `f` ziet: tegenover een onvertrouwde
    /// producer kopieert `f` en toetst hij de kopie, nooit de ring.
    pub fn read_with<T>(&mut self, max: usize, f: impl FnOnce(Kind, &[u8]) -> T) -> Option<T> {
        let r = self.next(max)?;
        if !r.hw {
            dev::pull(r.at, r.len);
        }
        let out = dev::view(r.at, r.len, |p| f(r.kind, p));
        self.free(r.end, r.hw);
        Some(out)
    }

    /// Het volgende record, getoetst en nog in de ring; PAD-records gaan
    /// meteen terug. `None` als de ring leeg of corrupt is.
    #[inline(always)]
    fn next(&mut self, max: usize) -> Option<Next> {
        if self.corrupt.is_some() {
            return None;
        }
        let size = self.g.size;
        loop {
            let (head, hw) =
                self.g
                    .peer(HEAD_OFF, PRODUCER_WB_OFF, self.local == Coherence::Hardware);
            let tail = self.tail;
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
            if !hw {
                dev::pull(at, REC_HDR as usize);
            }
            let hdr = dev::read64(at);
            let len = u64::from(hdr as u32);
            let raw = (hdr >> 32) as u32;
            let need = REC_HDR + align8(len);
            if need > avail || need > size - tail % size || len > max as u64 {
                self.mark(Corrupt::BadHeader {
                    hdr,
                    head,
                    tail,
                    size,
                    buf: max,
                });
                return None;
            }
            let end = tail.wrapping_add(need);
            let Some(kind) = Kind::new(raw) else {
                self.free(end, hw);
                continue;
            };
            return Some(Next {
                kind,
                at: at.add(REC_HDR),
                len: len as usize,
                end,
                hw,
            });
        }
    }

    /// Geeft de ruimte tot `end` terug aan de producer, ná alles wat de
    /// lezer van het record las. `hw`: beide kanten beloven write-back, dus
    /// de producer leest tail uit de gedeelde cache en is de clean loos (de
    /// enige lezer zonder MMU, de EL2-switcher, kijkt alleen naar head).
    #[inline(always)]
    fn free(&mut self, end: u64, hw: bool) {
        dev::mb();
        self.tail = end;
        if hw {
            dev::write64(self.g.base.add(TAIL_OFF), end);
        } else {
            self.g.set_tail(end);
        }
    }
}

#[cfg(test)]
mod tests;
