//! De bouwer van de stage-1 op de host: de attributen per gebied, de
//! grenzen, en de glas-toevoeging in dezelfde tabellen.

use super::tables::*;
use super::*;
use abi::layout::{ABI_TAIL, CTRL_STRIDE, LINK_BASE, LINK_TEXT_OFF};

/// De tabelpagina's van de tests.
struct Pages(std::vec::Vec<[u64; 512]>);

impl Default for Pages {
    fn default() -> Self {
        Pages(std::vec![[0; 512]; TABLE_PAGES])
    }
}

impl TableMem for Pages {
    fn get(&self, page: usize, idx: usize) -> u64 {
        self.0[page][idx]
    }
    fn set(&mut self, page: usize, idx: usize, v: u64) {
        self.0[page][idx] = v;
    }
}

/// Een wandeling zoals de walker hem doet: de laatste entry (blok of
/// pagina) voor `va`, of `None` als stage 1 hem niet vertaalt.
fn walk(t: &Tables<Pages>, va: u64) -> Option<u64> {
    let l1 = t.mem.get(0, (va / GIB) as usize);
    if l1 & 0b11 == DESC_BLOCK {
        return Some(l1);
    }
    if l1 & 0b11 != DESC_TABLE {
        return None;
    }
    let p2 = ((l1 & OA) - t.base) / PAGE;
    let l2 = t.mem.get(p2 as usize, ((va % GIB) / BLOCK) as usize);
    match l2 & 0b11 {
        DESC_BLOCK => Some(l2),
        DESC_TABLE => {
            let p3 = ((l2 & OA) - t.base) / PAGE;
            let e = t.mem.get(p3 as usize, ((va % BLOCK) / PAGE) as usize);
            (e & 0b11 == DESC_PAGE).then_some(e)
        }
        _ => None,
    }
}

fn attr(e: u64) -> u64 {
    (e >> ATTR_SHIFT) & 0b111
}

/// Hop op de Pi 5 (30-09): een partitie van 64 MiB, dus 62 MiB RAM en de
/// staart van 2 MiB erboven.
const HOP_RAM: u64 = (64 << 20) - ABI_TAIL;

#[test]
fn every_region_gets_its_attribute_and_identity() {
    let p = Plan::new(LINK_BASE, HOP_RAM).unwrap();
    let tail = LINK_BASE + HOP_RAM;
    assert_eq!(p.tables, (LINK_BASE, LINK_BASE + LINK_TEXT_OFF));
    assert_eq!(p.ram, (LINK_BASE + LINK_TEXT_OFF, tail));
    assert_eq!(p.ctrl, (tail, tail + CTRL_STRIDE));
    assert_eq!(p.rings, (tail + CTRL_STRIDE, tail + ABI_TAIL));
    let t = Tables::of(Pages::default(), &p).unwrap();
    // L1, L2 voor GB 1, een L3 voor de tabelruimte onderin het eerste blok,
    // een L3 voor de control-page bovenin.
    assert_eq!(t.used, 4);

    // De tabellen zelf: Normal-NC, niet uitvoerbaar, identiteit.
    let e = walk(&t, LINK_BASE + 0x3000).unwrap();
    assert_eq!(e & OA, LINK_BASE + 0x3000);
    assert_eq!(attr(e), IDX_NC);
    assert_ne!(e & PXN, 0);
    // Het image: write-back, uitvoerbaar op EL1, niet op EL0.
    let e = walk(&t, LINK_BASE + LINK_TEXT_OFF).unwrap();
    assert_eq!(e & OA, LINK_BASE + LINK_TEXT_OFF);
    assert_eq!(attr(e), IDX_WB);
    assert_eq!(e & PXN, 0);
    assert_ne!(e & UXN, 0);
    assert_eq!(e & SH_INNER, SH_INNER);
    // De heap in het midden: een heel blok.
    let e = walk(&t, LINK_BASE + (32 << 20)).unwrap();
    assert_eq!(e & 0b11, DESC_BLOCK);
    assert_eq!(attr(e), IDX_WB);
    // De stack, de laatste pagina onder de staart.
    let e = walk(&t, tail - PAGE).unwrap();
    assert_eq!(attr(e), IDX_WB);
    assert_eq!(e & PXN, 0);
    // De control-page: Normal-NC (de switcher schrijft er met de MMU uit
    // in), niet uitvoerbaar.
    let e = walk(&t, tail).unwrap();
    assert_eq!(e & OA, tail);
    assert_eq!(e & 0b11, DESC_PAGE);
    assert_eq!(attr(e), IDX_NC);
    assert_ne!(
        attr(e),
        IDX_DEVICE,
        "Device is where the alignment trap was"
    );
    assert_ne!(e & PXN, 0);
    let e = walk(&t, tail + CTRL_STRIDE - 8).unwrap();
    assert_eq!(attr(e), IDX_NC);
    // De outbox direct erboven, en de laatste pagina van de RX-ring:
    // write-back, niet uitvoerbaar.
    let e = walk(&t, tail + CTRL_STRIDE).unwrap();
    assert_eq!(attr(e), IDX_WB);
    assert_ne!(e & PXN, 0);
    let e = walk(&t, tail + ABI_TAIL - PAGE).unwrap();
    assert_eq!(attr(e), IDX_WB);
    assert_ne!(e & PXN, 0);
    // Niets erbuiten: voorbij de staart, onder het linkvenster, de kern.
    assert_eq!(walk(&t, tail + ABI_TAIL), None, "past the tail");
    assert_eq!(walk(&t, LINK_BASE - PAGE), None, "under the window");
    assert_eq!(walk(&t, 0x4000_0000), None, "the kern is not ours");
    assert_eq!(walk(&t, FB_GLASS), None, "the glass waits for fb::map");
}

/// Het glas van de ramfb van QEMU (`FB_BASE` 0x2000_0000, 1280x800x4).
const FB_GLASS: u64 = 0x2000_0000;

#[test]
fn the_glass_joins_the_same_tables_as_normal_nc() {
    let p = Plan::new(LINK_BASE, HOP_RAM).unwrap();
    let t = Tables::of(Pages::default(), &p).unwrap();
    let used = t.used;
    // Verder in dezelfde tabellen, zoals `map_glass` na de MMU-aan doet.
    let mut t = Tables::resume(t.mem, t.base, used).unwrap();
    let hi = FB_GLASS + 0x3e_8000;
    t.map(FB_GLASS, hi, ATTR_GLASS).unwrap();
    // Een L2 voor GB 0 en een L3 voor de rand van het glas.
    assert_eq!(t.used, used + 2);
    let e = walk(&t, FB_GLASS).unwrap();
    assert_eq!(e & 0b11, DESC_BLOCK);
    assert_eq!(attr(e), IDX_NC);
    assert_ne!(e & PXN, 0);
    let e = walk(&t, hi - PAGE).unwrap();
    assert_eq!(e & OA, hi - PAGE);
    assert_eq!(attr(e), IDX_NC);
    assert_eq!(walk(&t, hi), None, "past the glass");
    // De rest van de map is onveranderd.
    assert_eq!(attr(walk(&t, LINK_BASE + LINK_TEXT_OFF).unwrap()), IDX_WB);
}

#[test]
fn large_codec_partitions_keep_cached_ram_and_exact_tail_boundaries() {
    for gib in [8, 16, 32] {
        for size in [gib * GIB, gib * GIB - ABI_TAIL] {
            let p = Plan::new(LINK_BASE, size).unwrap();
            let t = Tables::of(Pages::default(), &p).unwrap();
            assert!(t.used <= 6, "{} table pages for {size} bytes", t.used);
            for a in (LINK_BASE.next_multiple_of(GIB)..p.ram.1 - GIB).step_by(GIB as usize) {
                let e = walk(&t, a).unwrap();
                assert_eq!(e & OA, a);
                assert_eq!(e & 3, DESC_BLOCK);
                assert_eq!(attr(e), IDX_WB);
                assert_eq!(e & PXN, 0);
            }
            assert_eq!(attr(walk(&t, p.ram.1 - PAGE).unwrap()), IDX_WB);
            assert_eq!(attr(walk(&t, p.ctrl.0).unwrap()), IDX_NC);
            assert_eq!(attr(walk(&t, p.rings.0).unwrap()), IDX_WB);
            assert_eq!(walk(&t, p.rings.1), None);
            assert_eq!(walk(&t, LINK_BASE - PAGE), None);
        }
    }
}

#[test]
fn the_plan_refuses_what_it_cannot_map() {
    assert_eq!(
        Plan::new(LINK_BASE + PAGE, HOP_RAM),
        Err(MmuError::NotAtLinkBase {
            start: LINK_BASE + PAGE
        })
    );
    // Geen ruimte boven de tabellen, een staart die niet op een pagina
    // begint, een staart die omloopt.
    for size in [LINK_TEXT_OFF, HOP_RAM + 8, u64::MAX - LINK_BASE] {
        assert_eq!(
            Plan::new(LINK_BASE, size),
            Err(MmuError::Shape {
                start: LINK_BASE,
                size
            })
        );
    }
    // Een gebied dat niet op een pagina staat, weigert de bouwer.
    let mut t = Tables::new(Pages::default(), LINK_BASE);
    assert_eq!(
        t.map(LINK_BASE + 8, LINK_BASE + PAGE, ATTR_NC),
        Err(MmuError::Tables)
    );
    // Hervatten met een onmogelijke telling ook.
    assert!(Tables::resume(Pages::default(), LINK_BASE, 0).is_err());
    assert!(Tables::resume(Pages::default(), LINK_BASE, TABLE_PAGES + 1).is_err());
}

#[test]
fn a_map_that_does_not_fit_is_refused() {
    // Een glas dat over veertien losse blokranden loopt past niet.
    let mut t = Tables::new(Pages::default(), LINK_BASE);
    let mut r = Ok(());
    for i in 0..20u64 {
        let a = FB_GLASS + i * GIB / 2 + PAGE;
        r = t.map(a, a + PAGE, ATTR_GLASS);
        if r.is_err() {
            break;
        }
    }
    assert_eq!(r, Err(MmuError::Tables));
    assert!(t.used <= TABLE_PAGES);
}

#[test]
fn the_boot_word_round_trips() {
    let (s, n) = (LINK_BASE, HOP_RAM);
    assert_eq!(decode(4, s, n), Ok(4));
    assert_eq!(decode(0, s, n), Err(MmuError::Off));
    for e in [
        MmuError::NotAtLinkBase { start: s },
        MmuError::Shape { start: s, size: n },
        MmuError::Tables,
        MmuError::Off,
    ] {
        assert_eq!(decode(refusal_word(e), s, n), Err(e));
    }
    // De bouwer op de host: het plan klopt, maar er is geen ARM-stage-1.
    assert_eq!(
        decode(__applib_stage1_build(s, n), s, n),
        Err(MmuError::Off)
    );
    assert_eq!(
        decode(__applib_stage1_build(s + PAGE, n), s + PAGE, n),
        Err(MmuError::NotAtLinkBase { start: s + PAGE })
    );
    assert!(MmuError::Tables.to_string().contains("64 KB"));
}
