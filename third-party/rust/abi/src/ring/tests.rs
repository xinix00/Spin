//! Host-tests voor het ringprotocol, geport uit `ring_test.go`.
//!
//! De backing is hier een host-buffer: `dev` heeft op de host gewone
//! vluchtige toegang en no-op-barrières. Dit bewijst de record-, wrap- en
//! verdedigingslogica; de barrière-plaatsing bewijst het board.

use super::*;

/// Een ring op een host-buffer. De `Vec` houdt het geheugen levend zolang
/// de test loopt; de ring kent alleen een `Pa`.
struct Host {
    _mem: Vec<u64>,
    g: Geom,
    w: Writer,
    r: Reader,
}

fn new_ring(size: u64) -> Host {
    let mut mem = vec![0u64; ((DATA_OFF + size) / 8 + 1) as usize];
    let base = Pa(mem.as_mut_ptr() as usize as u64);
    init(base, size).unwrap();
    Host {
        _mem: mem,
        g: Geom { base, size },
        w: Writer::open(base, size).unwrap(),
        r: Reader::open(base, size).unwrap(),
    }
}

fn read(h: &mut Host, buf: &mut [u8]) -> Option<(Kind, Vec<u8>)> {
    h.r.read_into(buf)
        .map(|rec| (rec.kind, rec.payload.to_vec()))
}

#[test]
fn roundtrip() {
    let mut h = new_ring(512);
    let mut buf = [0u8; 512];
    for n in [0usize, 1, 7, 8, 9, 15, 16, 63] {
        let p = vec![n as u8; n];
        h.w.write(Kind::LOG, &p).unwrap();
        let (kind, got) = read(&mut h, &mut buf).unwrap();
        assert_eq!((kind, got), (Kind::LOG, p), "roundtrip {n} bytes");
    }
    assert!(
        read(&mut h, &mut buf).is_none(),
        "lege ring leverde een record"
    );
}

#[test]
fn write_notify_alleen_op_lege_overgang() {
    let mut h = new_ring(512);
    assert!(h.w.write(Kind::LOG, b"een").unwrap(), "eerste write wekt");
    assert!(
        !h.w.write(Kind::LOG, b"twee").unwrap(),
        "tweede write wekt niet"
    );
    let mut buf = [0u8; 512];
    for _ in 0..2 {
        assert!(read(&mut h, &mut buf).is_some(), "record ontbreekt");
    }
    assert!(
        h.w.write(Kind::LOG, b"drie").unwrap(),
        "write na drain wekt"
    );
}

#[test]
fn fifo_en_types() {
    let mut h = new_ring(512);
    // 3 en 4 zijn de vervallen RPC-soorten: de ring draagt ze nog gewoon,
    // alleen schrijft geen kant ze meer.
    let kinds = [
        Kind::LOG,
        Kind::new(3).unwrap(),
        Kind::new(4).unwrap(),
        Kind::FRAME,
    ];
    for i in 0..8usize {
        let p = vec![i as u8; i + 1];
        h.w.write(kinds[i % kinds.len()], &p).unwrap();
    }
    let mut buf = [0u8; 512];
    for i in 0..8usize {
        let (kind, got) = read(&mut h, &mut buf).unwrap();
        assert_eq!(kind, kinds[i % kinds.len()], "record {i}");
        assert_eq!(got.len(), i + 1);
        assert_eq!(got[0], i as u8);
    }
}

#[test]
fn head_uitlijning() {
    let mut h = new_ring(512);
    for n in [0usize, 1, 7, 8, 9] {
        let before = h.g.head();
        h.w.write(Kind::LOG, &vec![0; n]).unwrap();
        assert_eq!(h.g.head(), before + REC_HDR + align8(n as u64), "n={n}");
    }
}

#[test]
fn vol_en_drain() {
    let mut h = new_ring(128);
    let p = [0u8; 24]; // need = 32
    for i in 0..4 {
        h.w.write(Kind::LOG, &p)
            .unwrap_or_else(|e| panic!("write {i}: {e}"));
    }
    assert!(matches!(
        h.w.write(Kind::LOG, &p),
        Err(Error::RingFull { .. })
    ));
    let mut buf = [0u8; 128];
    assert!(read(&mut h, &mut buf).is_some(), "volle ring leverde niets");
    h.w.write(Kind::LOG, &p).unwrap();
}

#[test]
fn fits_en_te_groot() {
    let mut h = new_ring(128); // grens: REC_HDR + align8(n) <= 64, dus n <= 56
    assert!(h.w.fits(56));
    assert!(!h.w.fits(57));
    assert!(matches!(
        h.w.write(Kind::LOG, &[0; 57]),
        Err(Error::RecordTooLarge { need: 72, max: 64 })
    ));
}

#[test]
fn wrap_met_pad() {
    let mut h = new_ring(128);
    let mut buf = [0u8; 128];
    for _ in 0..3 {
        // Head en tail naar 96.
        h.w.write(Kind::LOG, &[0xAA; 24]).unwrap();
        read(&mut h, &mut buf).unwrap();
    }
    let p40 = [0xBBu8; 40]; // need 48 > contig 32: PAD en wrap
    h.w.write(Kind::FRAME, &p40).unwrap();
    let (kind, got) = read(&mut h, &mut buf).unwrap();
    assert_eq!((kind, got.as_slice()), (Kind::FRAME, &p40[..]));
    assert_eq!((h.g.head(), h.g.tail()), (96 + 32 + 48, 96 + 32 + 48));
}

#[test]
fn wrap_exact_passend() {
    let mut h = new_ring(128);
    let mut buf = [0u8; 128];
    for _ in 0..3 {
        h.w.write(Kind::LOG, &[0; 24]).unwrap();
        read(&mut h, &mut buf).unwrap();
    }
    h.w.write(Kind::LOG, &[0; 24]).unwrap(); // need 32 == contig 32
    assert_eq!(h.g.head(), 128, "geen PAD");
    assert_eq!(read(&mut h, &mut buf).unwrap().1.len(), 24);
}

#[test]
fn corrupt_header_buiten_publicatie() {
    let mut h = new_ring(128);
    dev::write64(h.g.at(0), 100 | 1 << 32); // len 100, need 112
    h.g.set_head(16); // maar slechts 16 gepubliceerd
    let mut buf = [0u8; 128];
    assert!(read(&mut h, &mut buf).is_none());
    assert!(matches!(h.r.corrupt(), Some(Corrupt::BadHeader { .. })));
    h.g.set_head(h.g.tail()); // "herstel" door de producer
    h.w.write(Kind::LOG, &[1]).unwrap();
    assert!(read(&mut h, &mut buf).is_none(), "corrupte ring leefde op");
}

#[test]
fn corrupt_header_over_de_rand() {
    let mut h = new_ring(128);
    let mut buf = [0u8; 128];
    for _ in 0..3 {
        // Tail naar 96; contig = 32.
        h.w.write(Kind::LOG, &[0; 24]).unwrap();
        read(&mut h, &mut buf).unwrap();
    }
    dev::write64(h.g.at(96), 40 | 1 << 32); // need 48 > 32
    h.g.set_head(96 + 48);
    assert!(read(&mut h, &mut buf).is_none());
    assert!(h.r.is_corrupt());
}

#[test]
fn corrupt_groter_dan_buf() {
    let mut h = new_ring(512);
    h.w.write(Kind::LOG, &[0; 64]).unwrap();
    assert!(read(&mut h, &mut [0u8; 16]).is_none());
    assert!(h.r.is_corrupt());
}

#[test]
fn corrupt_weggelopen_head() {
    let mut h = new_ring(128);
    h.g.set_head(1 << 62);
    assert!(read(&mut h, &mut [0u8; 128]).is_none());
    assert!(matches!(h.r.corrupt(), Some(Corrupt::HeadAhead { .. })));
}

#[test]
fn write_bij_onzinnige_tail() {
    let mut h = new_ring(128);
    h.g.set_tail(1 << 63);
    assert!(matches!(
        h.w.write(Kind::LOG, &[1]),
        Err(Error::RingIndices { .. })
    ));
}

/// Een kleine deterministische bron voor de fuzz-port: geen crate, en een
/// falende ronde is met zijn zaad te herhalen.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
}

/// De Go-fuzz `FuzzReadInto` als vaste rondes: willekeurige ringinhoud en
/// een willekeurige head mogen de lezer nooit laten panieken, hangen of
/// meer records laten leveren dan er ooit in de buffer passen.
#[test]
fn fuzz_read_into() {
    const SIZE: u64 = 128;
    let mut rng = Lcg(0x5eed);
    let mut cases: Vec<(Vec<u8>, u64)> = vec![
        (vec![], 0),
        (vec![0; SIZE as usize], 1 << 62),
        (vec![100, 0, 0, 0, 1, 0, 0, 0], 16),
    ];
    for _ in 0..2000 {
        let n = (rng.next() % (SIZE + 1)) as usize;
        let data = (0..n).map(|_| rng.next() as u8).collect();
        let head = match rng.next() % 3 {
            0 => rng.next() % (2 * SIZE),
            1 => (rng.next() % SIZE) & !7,
            _ => rng.next(),
        };
        cases.push((data, head));
    }
    for (data, head) in cases {
        let mut h = new_ring(SIZE);
        dev::copy_in(h.g.at(0), &data);
        h.g.set_head(head);
        let mut buf = [0u8; SIZE as usize];
        let delivered = (0..4 * SIZE)
            .take_while(|_| h.r.read_into(&mut buf).is_some())
            .count();
        assert!(
            delivered < (4 * SIZE) as usize,
            "blijft leveren (head={head:#x})"
        );
    }
}

#[test]
fn open_uses_owner_capacity() {
    for wire in [0u64, 8, 512, u64::MAX] {
        let mut h = new_ring(128);
        dev::write64(h.g.base.add(SIZE_OFF), wire);
        h.w = Writer::open(h.g.base, 128).unwrap();
        h.r = Reader::open(h.g.base, 128).unwrap();
        assert!(!h.w.fits(64), "wire size {wire:#x} changed owner capacity");
        for _ in 0..20 {
            h.w.write(Kind::LOG, b"bounded").unwrap();
            let mut buf = [0u8; 32];
            assert_eq!(read(&mut h, &mut buf).unwrap().1, b"bounded");
        }
    }
}

#[test]
fn open_rejects_invalid_owner_range() {
    for (base, size) in [
        (0x1000u64, 0u64),
        (0x1000, 7),
        (0x1000, 17),
        (0x1001, 128),
        (!7, 128),
        (0x1000, !7),
    ] {
        assert!(
            Writer::open(Pa(base), size).is_err(),
            "open({base:#x}, {size:#x})"
        );
        assert!(Reader::open(Pa(base), size).is_err());
        assert!(init(Pa(base), size).is_err());
    }
}

#[test]
fn head_pending_snapshots() {
    const CAP: u64 = 128;
    for (name, head, tail, pending) in [
        ("empty", 64u64, 64u64, false),
        ("unread", 80, 64, true),
        ("full", 192, 64, true),
        // De waarnemer leest H=64; producer publiceert en consument draineert
        // tot T=80 vóór de waarnemer tail leest. Twee lezingen, niet atomair.
        ("old-head-new-tail", 64, 80, false),
        ("outside-capacity", 200, 64, false),
        ("pending-across-counter-wrap", 8, u64::MAX - 7, true),
        ("stale-head-across-counter-wrap", u64::MAX - 7, 8, false),
    ] {
        let h = new_ring(CAP);
        h.g.set_head(head);
        h.g.set_tail(tail);
        // De lezer leest zijn eigen tail bij het openen (daarna houdt hij hem
        // zelf bij), dus een lezer die de gezette tail ziet.
        let r = Reader::open(h.g.base, h.g.size).unwrap();
        assert_eq!(r.head_pending(), (head, pending), "{name}");
    }
}

/// Elke combinatie van [`Coherence`] aan de twee kanten spreekt hetzelfde
/// protocol: dezelfde bytes, dezelfde PAD-wraps, dezelfde indexen. Op de host
/// zijn `push`/`pull` no-ops, dus dit bewijst de vorm, niet de cache; het
/// woord in de ringkop toetst `hardware_needs_both_words`.
#[test]
fn coherence_mixes_round_trip_over_wraps() {
    use Coherence::{Hardware, Maintained};
    for (wc, rc) in [
        (Maintained, Maintained),
        (Hardware, Hardware),
        (Hardware, Maintained),
        (Maintained, Hardware),
    ] {
        let mut h = new_ring(256);
        h.w = Writer::open_with(h.g.base, 256, wc).unwrap();
        h.r = Reader::open_with(h.g.base, 256, rc).unwrap();
        let mut buf = [0u8; 256];
        let mut sent = 0u64;
        // Oneven lengtes laten head over elke 8-uitlijning en de rand lopen.
        for i in 0..200usize {
            let n = (i * 37) % 113;
            let p: Vec<u8> = (0..n).map(|j| (i ^ j) as u8).collect();
            h.w.write(Kind::FRAME, &p).unwrap();
            sent += 1;
            let (kind, got) = read(&mut h, &mut buf).unwrap();
            assert_eq!((kind, got), (Kind::FRAME, p), "{wc:?}->{rc:?} record {i}");
        }
        assert_eq!(sent, 200);
        assert_eq!(h.g.head(), h.g.tail(), "{wc:?}->{rc:?}");
        assert!(read(&mut h, &mut buf).is_none());
    }
}

/// Een coherente lezer verdedigt zich net zo tegen een verzonnen kop.
#[test]
fn hardware_reader_still_refuses_a_bad_header() {
    let mut h = new_ring(128);
    h.r = Reader::open_with(h.g.base, 128, Coherence::Hardware).unwrap();
    h.w.write(Kind::LOG, &[1; 8]).unwrap();
    dev::write64(h.g.at(0), 200 | 1 << 32); // len 200 > buf en > gepubliceerd
    assert!(read(&mut h, &mut [0u8; 64]).is_none());
    assert!(matches!(h.r.corrupt(), Some(Corrupt::BadHeader { .. })));
}

/// Alleen een kant die [`Coherence::Hardware`] opent, zet zijn woord, elk in
/// zijn eigen regel; een kant zonder belofte laat de kop ongemoeid.
#[test]
fn hardware_needs_both_words() {
    let h = new_ring(128);
    let word = |off| dev::read64(h.g.base.add(off));
    assert_eq!((word(PRODUCER_WB_OFF), word(CONSUMER_WB_OFF)), (0, 0));
    let _w = Writer::open_with(h.g.base, 128, Coherence::Maintained).unwrap();
    let _r = Reader::open_with(h.g.base, 128, Coherence::Maintained).unwrap();
    assert_eq!((word(PRODUCER_WB_OFF), word(CONSUMER_WB_OFF)), (0, 0));
    let _w = Writer::open_with(h.g.base, 128, Coherence::Hardware).unwrap();
    assert_eq!((word(PRODUCER_WB_OFF), word(CONSUMER_WB_OFF)), (WB_WORD, 0));
    let _r = Reader::open_with(h.g.base, 128, Coherence::Hardware).unwrap();
    assert_eq!(word(CONSUMER_WB_OFF), WB_WORD);
    assert_eq!(h.g.size, word(SIZE_OFF));
    // Een verse init wist de beloftes weer.
    init(h.g.base, 128).unwrap();
    assert_eq!((word(PRODUCER_WB_OFF), word(CONSUMER_WB_OFF)), (0, 0));
}

/// Het record in de ring zelf schrijven en lezen spreekt hetzelfde protocol
/// als de kopie: over elke wrap, in elke combinatie met `write` en
/// `read_into`, met PAD-records ertussen.
#[test]
fn in_place_round_trips_over_wraps() {
    let mut h = new_ring(256);
    let mut buf = [0u8; 256];
    for i in 0..300usize {
        let n = (i * 29) % 100 + 1;
        let p: Vec<u8> = (0..n).map(|j| (i * 3 + j) as u8).collect();
        if i % 2 == 0 {
            let got = h.w.write_with(Kind::FRAME, 100, |dst| {
                dst[..n].copy_from_slice(&p);
                n
            });
            assert!(matches!(got, Ok(Some(true))), "record {i}: {got:?}");
        } else {
            h.w.write(Kind::FRAME, &p).unwrap();
        }
        if i % 3 == 0 {
            let (kind, got) = read(&mut h, &mut buf).unwrap();
            assert_eq!((kind, got), (Kind::FRAME, p), "record {i}");
        } else {
            let got = h.r.read_with(100, |k, s| (k, s.to_vec())).unwrap();
            assert_eq!(got, (Kind::FRAME, p), "record {i}");
        }
    }
    assert_eq!(h.g.head(), h.g.tail());
}

/// Schrijft `f` niets, dan is er geen record; is er geen plaats voor `max`,
/// dan wordt `f` niet geroepen en blijft de ring zoals hij was.
#[test]
fn in_place_write_of_nothing_or_too_much() {
    let mut h = new_ring(256);
    assert!(matches!(h.w.write_with(Kind::FRAME, 64, |_| 0), Ok(None)));
    assert_eq!(h.g.head(), 0);
    h.w.write(Kind::FRAME, &[7; 100]).unwrap();
    h.w.write(Kind::FRAME, &[8; 100]).unwrap();
    let mut called = false;
    let r = h.w.write_with(Kind::FRAME, 40, |_| {
        called = true;
        1
    });
    assert!(matches!(r, Err(Error::RingFull { .. })) && !called, "{r:?}");
    // Meer dan `max` telt als `max`.
    let mut buf = [0u8; 256];
    let _ = read(&mut h, &mut buf);
    let _ = read(&mut h, &mut buf);
    assert!(matches!(
        h.w.write_with(Kind::FRAME, 16, |_| 999),
        Ok(Some(true))
    ));
    assert_eq!(read(&mut h, &mut buf).unwrap().1.len(), 16);
}

/// Een lezer in plaats weigert een kop boven zijn `max` net als `read_into`.
#[test]
fn in_place_reader_refuses_a_header_above_max() {
    let mut h = new_ring(256);
    h.w.write(Kind::FRAME, &[1; 40]).unwrap();
    assert!(h.r.read_with(32, |_, _| ()).is_none());
    assert!(matches!(h.r.corrupt(), Some(Corrupt::BadHeader { .. })));
}

/// De eigen index komt van de eigen kant (03-10): een tegenpartij die tail
/// (of head) in gedeeld geheugen overschrijft, laat de lezer niet opnieuw
/// lezen of de schrijver niet over ongelezen records heen schrijven.
#[test]
fn own_index_is_kept_by_its_owner() {
    let mut h = new_ring(256);
    assert!(h.w.write(Kind::FRAME, &[1; 8]).unwrap());
    let mut buf = [0u8; 64];
    assert_eq!(h.r.read_into(&mut buf).map(|r| r.payload.len()), Some(8));
    // Een verzonnen tail terug naar 0: de lezer leest het record niet nog eens.
    h.g.set_tail(0);
    assert!(h.r.read_into(&mut buf).is_none());
    // De schrijver gaat door vanaf zijn eigen head, niet vanaf een
    // verzonnen head in gedeeld geheugen; de lezer vindt dat record achter
    // het eerste.
    h.g.set_head(0);
    assert!(h.w.write(Kind::FRAME, &[2; 8]).is_ok());
    let r = h.r.read_into(&mut buf).map(|r| r.payload.to_vec());
    assert_eq!(r.as_deref(), Some(&[2u8; 8][..]));
}
