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
        assert_eq!(h.r.head_pending(), (head, pending), "{name}");
    }
}

#[test]
fn snapshot_draagt_de_getallen() {
    let mut h = new_ring(128);
    h.w.write(Kind::LOG, b"x").unwrap();
    let s = h.r.snapshot();
    assert_eq!((s.head, s.tail, s.size, s.hdr), (16, 0, 128, 1 | 1 << 32));
    h.g.set_head(1 << 62);
    let _ = read(&mut h, &mut [0u8; 128]);
    let text = format!("{}", h.r.snapshot());
    assert!(text.contains("corrupt=head-tail>size"), "{text}");
}
