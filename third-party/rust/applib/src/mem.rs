//! De symbolen `memcpy`, `memcmp` en `bcmp` voor een app op arm64; de
//! lussen staan in [`dev::mem`], met het waarom.
//!
//! Sinds 30-09 draait elke app met zijn stage-1 aan ([`crate::mmu`]): al
//! zijn RAM en zijn ringen zijn Normal. Het snelle pad alleen dan
//! ([`crate::mmu::normal`]): de bouwer in `_start` draait met de MMU uit
//! (hij doet geen `memcpy`, maar een onbedoelde kopie valt dan op het trage
//! pad), en een app waarvan de kern de stage-1 weigerde
//! (`HOPOS_APP_NO_MMU`) ziet alles als Device.
//!
//! Onze symbolen zijn sterk, die van `compiler_builtins` zwak: de linker
//! kiest deze. `memmove` en `memset` blijven van `compiler_builtins`; ze
//! zitten niet op een heet pad.

use dev::mem::{cmp_fast, cmp_slow, copy_fast, copy_slow};

/// `memcpy(3)`.
///
/// # Safety
///
/// Het contract van `memcpy`: twee geldige, niet-overlappende bereiken.
#[unsafe(no_mangle)]
#[inline(never)]
pub(crate) unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    // SAFETY: het contract van `memcpy`; het snelle pad alleen op Normal
    // geheugen (stage-1 aan) en vanaf 16 bytes.
    unsafe {
        if n >= 16 && crate::mmu::normal() {
            copy_fast(dst, src, n);
        } else {
            copy_slow(dst, src, n);
        }
    }
    dst
}

/// `memcmp(3)`.
///
/// # Safety
///
/// Het contract van `memcmp`: twee geldige bereiken van `n` bytes.
#[unsafe(no_mangle)]
#[inline(never)]
pub(crate) unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    // SAFETY: het contract van `memcmp`; het snelle pad alleen op Normal
    // geheugen en vanaf 8 bytes.
    unsafe {
        if n >= 8 && crate::mmu::normal() {
            cmp_fast(a, b, n)
        } else {
            cmp_slow(a, b, n)
        }
    }
}

/// `bcmp(3)`: LLVM maakt er een van een `memcmp` die alleen op nul toetst.
///
/// # Safety
///
/// Als [`memcmp`].
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    // SAFETY: als `memcmp`.
    unsafe { memcmp(a, b, n) }
}
