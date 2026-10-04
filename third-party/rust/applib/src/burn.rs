//! De rekenburst van bench (BURN) en vitals (cpu, smp, burn): de LCG-stap
//! uit de soak van appspike. Puur registerwerk, geen geheugendruk, dus wat
//! je meet is de klok van het hart. Eén definitie, zodat de getallen van de
//! twee apps naast elkaar kunnen.

use core::hint::black_box;

/// Eén burst: 2^19 LCG-stappen (Go: ~0,3 ms op een A76). Wie brandt, geeft
/// na elke burst de core af met een `yield_now`.
pub const BURST: u64 = 1 << 19;

/// De vermenigvuldiger van Knuth's MMIX-LCG, zoals in de Go-vitals.
const LCG_MUL: u64 = 6_364_136_223_846_793_005;

/// `n` LCG-stappen vanaf `acc`. Niet inline, zodat de compiler de lus niet
/// in de meetlus vouwt; het resultaat gaat door `black_box`.
#[inline(never)]
#[must_use]
pub fn lcg(mut acc: u64, n: u64) -> u64 {
    for k in 0..n {
        acc = acc.wrapping_mul(LCG_MUL).wrapping_add(k);
    }
    black_box(acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lcg_is_the_go_step() {
        // Twee stappen met de hand: acc*a + 0, dan *a + 1.
        let a = LCG_MUL;
        let want = 7u64.wrapping_mul(a).wrapping_mul(a).wrapping_add(1);
        assert_eq!(lcg(7, 2), want);
        assert_eq!(lcg(7, 0), 7);
    }
}
