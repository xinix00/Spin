//! De toetsen van het mengen: ChaCha20 tegen RFC 8439, het seqlock van de
//! page, en dat elke invoer de stroom verandert.

use super::*;
use crate::contract::{CTRL_ENV_DATA, CTRL_ENV_LEGACY_MAX, CTRL_ENV_LEN};
use crate::ctrl::Env;
use crate::ctrl::tests::Page;
use abi::hopabi::rng_source_word;
use core::cell::Cell;

/// Een klok die steeds 100 ns verder staat: geen jitter.
fn steady() -> impl FnMut() -> u64 {
    let t = Cell::new(0u64);
    move || {
        t.set(t.get() + 100);
        t.get()
    }
}

/// Een page met zaad `fill` in generatie `generation` uit bron `src`.
fn seeded(fill: u8, generation: u64, src: u8) -> Page {
    let mut p = Page::new();
    for i in 0..4u64 {
        p.put(CTRL_RNG_SEED + 8 * i, u64::from_le_bytes([fill; 8]));
    }
    p.put(CTRL_RNG_SOURCE, rng_source_word(src));
    p.put(CTRL_RNG_GEN, generation);
    p
}

/// RFC 8439 §2.3.2: sleutel 00..1f, teller 1, nonce 00000009 0000004a
/// 00000000.
#[test]
fn chacha20_block_matches_rfc8439() {
    let mut key = [0u32; 8];
    for (i, k) in key.iter_mut().enumerate() {
        let b = (i * 4) as u8;
        *k = u32::from_le_bytes([b, b + 1, b + 2, b + 3]);
    }
    let out = block(&key, 1, [0x0900_0000, 0x4a00_0000, 0]);
    assert_eq!(
        out,
        [
            0xe4e7_f110,
            0x1559_3bd1,
            0x1fdd_0f50,
            0xc471_20a3,
            0xc7f4_d1c7,
            0x0368_c033,
            0x9aaa_2204,
            0x4e6c_d4c3,
            0x4664_82d2,
            0x09aa_9f07,
            0x05d7_c214,
            0xa202_8bd9,
            0xd19c_12b5,
            0xb94e_16de,
            0xe883_d0cb,
            0x4e3c_50a2,
        ]
    );
}

#[test]
fn a_seed_is_read_only_under_the_seqlock() {
    // Een geveegde page (een oude kern): geen zaad.
    assert!(read_seed(&Page::new().ctrl()).is_none());
    // Een geldig zaad.
    let p = seeded(0x5a, 4, RNG_SRC_SOC);
    let s = read_seed(&p.ctrl()).unwrap();
    assert_eq!(s.bytes, [0x5a; 32]);
    assert_eq!((s.generation, s.origin), (4, Origin::Soc));
    // De kern schrijft net (oneven): geen zaad, ook niet na herhalen.
    let mut p = seeded(0x5a, 5, RNG_SRC_SOC);
    assert!(read_seed(&p.ctrl()).is_none());
    // Een generatie zonder magic in het bronwoord is geen zaad.
    p.put(CTRL_RNG_GEN, 6);
    p.put(CTRL_RNG_SOURCE, u64::from(RNG_SRC_SOC));
    assert!(read_seed(&p.ctrl()).is_none());
    // Het zaad zegt niets in een log.
    let s = read_seed(&seeded(0x5a, 2, RNG_SRC_RNDR).ctrl()).unwrap();
    assert!(!format!("{s:?}").contains("5a"));
}

/// Een oude kern met een env van 0xEA8 bytes: de tekst loopt over het
/// RNG-blok, dat is geen zaad, en de env blijft heel.
#[test]
fn a_long_env_of_an_old_kernel_is_no_seed() {
    let mut p = Page::new();
    let blob = vec![b'x'; CTRL_ENV_LEGACY_MAX as usize];
    dev::copy_in(p.ctrl().addr(CTRL_ENV_DATA), &blob);
    p.put(CTRL_ENV_LEN, blob.len() as u64);
    assert!(read_seed(&p.ctrl()).is_none());
    assert_eq!(Env::read(&p.ctrl()).len(), blob.len());
    // Op een nieuwe kern (magic aanwezig) is die lengte te lang.
    p.put(CTRL_RNG_SOURCE, rng_source_word(RNG_SRC_JITTER));
    assert!(Env::read(&p.ctrl()).is_empty());
}

#[test]
fn the_same_inputs_give_the_same_stream() {
    let p = seeded(7, 2, RNG_SRC_RNDR);
    let mut a = Rng::with_page(p.ctrl(), steady());
    let mut b = Rng::with_page(p.ctrl(), steady());
    assert_eq!(a.array::<64>(), b.array::<64>());
    assert_eq!(a.origin(), Origin::Rndr);
    assert!(a.origin().is_hardware());
    assert_eq!(a.generation(), 2);
}

#[test]
fn every_input_changes_the_stream() {
    let base = Rng::with_page(seeded(7, 2, RNG_SRC_RNDR).ctrl(), steady()).next_u64();
    // Ander zaad.
    let other = Rng::with_page(seeded(8, 2, RNG_SRC_RNDR).ctrl(), steady()).next_u64();
    assert_ne!(base, other);
    // Geen zaad: een andere stroom, en de bron zegt het.
    let mut none = Rng::with_page(Page::new().ctrl(), steady());
    assert_eq!(none.origin(), Origin::None);
    assert_ne!(none.next_u64(), base);
    // Andere jitter.
    let u = Cell::new(0u64);
    let jitter = || {
        u.set(u.get() + 100 + (u.get() / 100) % 3);
        u.get()
    };
    let j = Rng::with_page(seeded(7, 2, RNG_SRC_RNDR).ctrl(), jitter).next_u64();
    assert_ne!(j, base);
    // Een roersel.
    let mut s = Rng::with_page(seeded(7, 2, RNG_SRC_RNDR).ctrl(), steady());
    s.stir(&42u64.to_le_bytes());
    assert_ne!(s.next_u64(), base);
    // Mengen kent de lengte: "ab" is niet "ab\0".
    assert_ne!(
        Rng::from_seed(b"ab", steady()).next_u64(),
        Rng::from_seed(b"ab\0", steady()).next_u64()
    );
}

#[test]
fn draws_do_not_repeat() {
    let mut r = Rng::from_seed(b"seed", steady());
    let a: [u8; 32] = r.array();
    let b: [u8; 32] = r.array();
    assert_ne!(a, b);
    // Een trekking van 100 bytes is geen herhaling van blokken.
    let mut big = [0u8; 100];
    r.fill(&mut big);
    assert_ne!(big[..32], big[32..64]);
    assert_ne!(big[..4], [0; 4]);
}

#[test]
fn a_new_generation_is_mixed_in() {
    let mut p = seeded(7, 2, RNG_SRC_JITTER);
    let mut a = Rng::with_page(p.ctrl(), steady());
    let mut b = Rng::with_page(p.ctrl(), steady());
    assert!(!a.reseed(), "dezelfde generatie: niets te mengen");
    // De kern legt vers zaad.
    for i in 0..4u64 {
        p.put(CTRL_RNG_SEED + 8 * i, 0x1111 * (i + 1));
    }
    p.put(CTRL_RNG_SOURCE, rng_source_word(RNG_SRC_RNDR));
    p.put(CTRL_RNG_GEN, 4);
    // `fill` herzaait vanzelf, `b` heeft er geen page meer bij.
    b.page = None;
    assert_ne!(a.next_u64(), b.next_u64());
    assert_eq!((a.generation(), a.origin()), (4, Origin::Rndr));
    assert_eq!(b.generation(), 2);
}

#[test]
fn the_line_carries_its_marker() {
    let line = |o, g| Announce(o, g).to_string();
    assert!(line(Origin::None, 0).ends_with("HOPOS_APP_RNG_NONE"));
    assert!(line(Origin::Jitter, 2).ends_with("HOPOS_APP_RNG source=jitter"));
    assert!(line(Origin::Soc, 2).ends_with("HOPOS_APP_RNG source=hardware kind=soc"));
    assert!(line(Origin::Rndr, 2).contains("(rndr, gen 2)"));
}
