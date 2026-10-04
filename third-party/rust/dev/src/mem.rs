//! De lussen achter `memcpy` en `memcmp` voor arm64: wie het symbool
//! levert (applib voor een app, hopos voor de kern), kiest per aanroep het
//! snelle of het trage pad, want alleen hij weet of zijn geheugen Normal is.
//!
//! # Waarom (01-10, de puller van de Pi 4)
//!
//! Elk arm64-target van ons is `aarch64-unknown-none-softfloat`:
//! `+strict-align`. De voorgebouwde `compiler_builtins` houdt zich daaraan,
//! en zijn `memcpy` leest bij een ongelijke uitlijning van bron en doel elk
//! woord als acht `ldrb` plus schuiven (~1 cyclus per byte); zijn `memcmp`
//! is een bytelus (~2 cycli per byte). Op het pad van een frame is bijna
//! elke kopie ongelijk uitgelijnd: de TCP-payload staat op offset 54 en de
//! stroompositie in de ringen van leannet is willekeurig. Go deed hier
//! `memmove` met `ldp` en NEON (OLD/metal/dev/dev.go).
//!
//! Op Normal geheugen met de uitlijncontrole uit (SCTLR.A, bij de app
//! `SCTLR_EL1_CLEAN` van cpu/src/el2/oscore.rs, bij de kern cpu/src/boot.rs
//! en board/uefi/src/el2.rs) is een ongealigneerde `ldp`/`stp` gewoon
//! toegestaan, en de A72, A76 en A720 doen hem op volle snelheid binnen een
//! cacheline. Dat is het snelle pad: 64 bytes per ronde, de staart als één
//! overlappende 16-byte-kopie vanaf het eind (zoals elke `memcpy` met
//! ongealigneerde toegang, ook die van musl en Linux op arm64).
//!
//! Op Device (de MMU uit) abort elke ongealigneerde toegang. Daar, en voor
//! korte stukken, het trage pad: hele woorden als alles op 8 staat, anders
//! bytes. Beide alleen natuurlijk gealigneerd.
//!
//! De lussen staan in asm: een lus in Rust herkent LLVM als `memcpy` en
//! roept dan zichzelf aan.

use core::arch::asm;

/// Kopieert `n >= 16` bytes, ongealigneerd: alleen op Normal geheugen.
///
/// # Safety
///
/// `src` en `dst` zijn geldig voor `n >= 16` bytes, overlappen niet, en
/// liggen allebei in Normal geheugen met de uitlijncontrole uit.
#[inline(always)]
pub unsafe fn copy_fast(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: de aanroeper belooft twee geldige, niet-overlappende bereiken
    // van `n >= 16` bytes op Normal geheugen. De lus schrijft alleen binnen
    // [dst, dst + n) en leest alleen binnen [src, src + n); de laatste 16
    // bytes overlappen wat de lus al schreef, met dezelfde bytes.
    unsafe {
        asm!(
            "add {se}, {s}, {n}",
            "add {de}, {d}, {n}",
            // Zolang er meer dan 64 over zijn: 64 per ronde, vier paren
            // lezen vóór het eerste schrijven.
            "1: cmp {n}, #64",
            "b.ls 2f",
            "ldp {a}, {b}, [{s}]",
            "ldp {c}, {e}, [{s}, #16]",
            "ldp {f}, {g}, [{s}, #32]",
            "ldp {h}, {i}, [{s}, #48]",
            "add {s}, {s}, #64",
            "stp {a}, {b}, [{d}]",
            "stp {c}, {e}, [{d}, #16]",
            "stp {f}, {g}, [{d}, #32]",
            "stp {h}, {i}, [{d}, #48]",
            "add {d}, {d}, #64",
            "sub {n}, {n}, #64",
            "b 1b",
            // 16 < n <= 64: per 16 van voren.
            "2: cmp {n}, #16",
            "b.ls 3f",
            "ldp {a}, {b}, [{s}], #16",
            "stp {a}, {b}, [{d}], #16",
            "sub {n}, {n}, #16",
            "b 2b",
            // De laatste 16 vanaf het eind.
            "3: ldp {a}, {b}, [{se}, #-16]",
            "stp {a}, {b}, [{de}, #-16]",
            d = inout(reg) dst => _,
            s = inout(reg) src => _,
            n = inout(reg) n => _,
            se = out(reg) _,
            de = out(reg) _,
            a = out(reg) _,
            b = out(reg) _,
            c = out(reg) _,
            e = out(reg) _,
            f = out(reg) _,
            g = out(reg) _,
            h = out(reg) _,
            i = out(reg) _,
            options(nostack),
        );
    }
}

/// Kopieert `n` bytes met alleen natuurlijk gealigneerde toegang: hele
/// woorden als bron, doel en lengte op 8 staan, anders bytes. Veilig op
/// Device.
///
/// # Safety
///
/// `src` en `dst` zijn geldig voor `n` bytes en overlappen niet.
#[inline(always)]
pub unsafe fn copy_slow(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: als `copy_fast`; elke toegang ligt binnen de twee bereiken en
    // een woord alleen als alles op 8 staat.
    unsafe {
        asm!(
            "orr {t}, {d}, {s}",
            "orr {t}, {t}, {n}",
            "tst {t}, #7",
            "b.ne 2f",
            "1: cbz {n}, 3f",
            "ldr {t}, [{s}], #8",
            "str {t}, [{d}], #8",
            "sub {n}, {n}, #8",
            "b 1b",
            "2: cbz {n}, 3f",
            "ldrb {t:w}, [{s}], #1",
            "strb {t:w}, [{d}], #1",
            "sub {n}, {n}, #1",
            "b 2b",
            "3:",
            d = inout(reg) dst => _,
            s = inout(reg) src => _,
            n = inout(reg) n => _,
            t = out(reg) _,
            options(nostack),
        );
    }
}

/// Vergelijkt `n >= 8` bytes per paar woorden, ongealigneerd: alleen op
/// Normal geheugen. Negatief, nul of positief zoals `memcmp`.
///
/// # Safety
///
/// `a` en `b` zijn geldig voor `n >= 8` bytes en liggen allebei in Normal
/// geheugen met de uitlijncontrole uit.
#[inline(always)]
pub unsafe fn cmp_fast(a: *const u8, b: *const u8, n: usize) -> i32 {
    let r: i32;
    // SAFETY: de aanroeper belooft twee geldige bereiken van `n >= 8` bytes
    // op Normal geheugen; elke load ligt erbinnen (de staart is het laatste
    // woord, dat overlapt met wat al gelijk bleek).
    unsafe {
        asm!(
            "add {ae}, {a}, {n}",
            "add {be}, {b}, {n}",
            // 16 per ronde.
            "1: cmp {n}, #16",
            "b.lo 2f",
            "ldp {x}, {x2}, [{a}], #16",
            "ldp {y}, {y2}, [{b}], #16",
            "sub {n}, {n}, #16",
            "cmp {x}, {y}",
            "b.ne 4f",
            "cmp {x2}, {y2}",
            "b.eq 1b",
            "mov {x}, {x2}",
            "mov {y}, {y2}",
            "b 4f",
            // Nog 8 tot 15: één woord van voren.
            "2: cmp {n}, #8",
            "b.lo 6f",
            "ldr {x}, [{a}], #8",
            "ldr {y}, [{b}], #8",
            "sub {n}, {n}, #8",
            "cmp {x}, {y}",
            "b.ne 4f",
            // Minder dan 8: het laatste woord vanaf het eind.
            "6: cbz {n}, 3f",
            "ldr {x}, [{ae}, #-8]",
            "ldr {y}, [{be}, #-8]",
            "cmp {x}, {y}",
            "b.ne 4f",
            "3: mov {r:w}, #0",
            "b 5f",
            // Het eerste verschil in geheugenvolgorde is de hoogste byte na
            // `rev`: dan beslist een vergelijking zonder teken.
            "4: rev {x}, {x}",
            "rev {y}, {y}",
            "cmp {x}, {y}",
            "cset {r:w}, hi",
            "csinv {r:w}, {r:w}, wzr, hs",
            "5:",
            a = inout(reg) a => _,
            b = inout(reg) b => _,
            n = inout(reg) n => _,
            ae = out(reg) _,
            be = out(reg) _,
            x = out(reg) _,
            y = out(reg) _,
            x2 = out(reg) _,
            y2 = out(reg) _,
            r = out(reg) r,
            options(nostack, readonly),
        );
    }
    r
}

/// Vergelijkt `n` bytes per byte. Veilig op Device.
///
/// # Safety
///
/// `a` en `b` zijn geldig voor `n` bytes.
#[inline(always)]
pub unsafe fn cmp_slow(a: *const u8, b: *const u8, n: usize) -> i32 {
    let r: i32;
    // SAFETY: als `cmp_fast`, met bytes.
    unsafe {
        asm!(
            "mov {r:w}, #0",
            "1: cbz {n}, 2f",
            "ldrb {x:w}, [{a}], #1",
            "ldrb {y:w}, [{b}], #1",
            "sub {n}, {n}, #1",
            "subs {r:w}, {x:w}, {y:w}",
            "b.eq 1b",
            "2:",
            a = inout(reg) a => _,
            b = inout(reg) b => _,
            n = inout(reg) n => _,
            x = out(reg) _,
            y = out(reg) _,
            r = out(reg) r,
            options(nostack, readonly),
        );
    }
    r
}

#[cfg(test)]
mod tests {
    use super::{cmp_fast, cmp_slow, copy_fast, copy_slow};

    /// Elke lengte tot voorbij twee rondes, op elke combinatie van
    /// uitlijningen: precies `n` bytes, niets ernaast.
    #[test]
    fn copies_every_length_and_alignment() {
        let src: Vec<u8> = (0..320u32).map(|i| (i * 7 + 3) as u8).collect();
        for n in 0..300 {
            for so in 0..8 {
                for d_o in 0..8 {
                    for fast in [false, true] {
                        if fast && n < 16 {
                            continue;
                        }
                        let mut dst = [0xEEu8; 320];
                        let (d, s) = (dst[d_o..].as_mut_ptr(), src[so..].as_ptr());
                        // SAFETY: twee eigen buffers, ruim groot genoeg.
                        unsafe {
                            if fast {
                                copy_fast(d, s, n);
                            } else {
                                copy_slow(d, s, n);
                            }
                        }
                        assert_eq!(dst[d_o..d_o + n], src[so..so + n], "n={n} so={so} do={d_o}");
                        assert!(dst[..d_o].iter().all(|&b| b == 0xEE));
                        assert!(dst[d_o + n..].iter().all(|&b| b == 0xEE));
                    }
                }
            }
        }
    }

    /// Het teken van het eerste verschil, op elke plek en elke lengte; de
    /// bytes erna doen er niet toe.
    #[test]
    fn compares_like_memcmp() {
        let a: Vec<u8> = (0..100u32).map(|i| (i * 13) as u8).collect();
        for n in 0..90 {
            for off in 0..3 {
                let x = &a[off..off + n];
                let mut y = x.to_vec();
                let check = |y: &[u8], want: i32| {
                    // SAFETY: twee eigen slices van `n` bytes.
                    let slow = unsafe { cmp_slow(x.as_ptr(), y.as_ptr(), n) }.signum();
                    assert_eq!(slow, want, "slow n={n} off={off}");
                    if n >= 8 {
                        // SAFETY: idem.
                        let fast = unsafe { cmp_fast(x.as_ptr(), y.as_ptr(), n) }.signum();
                        assert_eq!(fast, want, "fast n={n} off={off}");
                    }
                };
                check(&y, 0);
                for i in 0..n {
                    let keep = y[i];
                    y[i] = keep.wrapping_add(1);
                    if i + 1 < n {
                        y[i + 1] = y[i + 1].wrapping_sub(5);
                    }
                    check(&y, if keep == 0xFF { 1 } else { -1 });
                    y.copy_from_slice(x);
                }
            }
        }
    }
}
