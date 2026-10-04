//! Het MMIO-primitief van HopOS: de enige plek met rauwe pointers naar
//! device-geheugen.
//!
//! Alles buiten de eigen RAM-declaratie (slot-partities, control-pages,
//! ringen, registers) gaat hier doorheen: gealigneerde, vluchtige toegang
//! per woord, en de barrières en het cache-onderhoud eromheen. Op de
//! ontwikkelmachine zijn `push`, `pull`, `mb` en `notify` no-ops, zodat
//! logica-crates daar testen; de plaatsing van de barrières bewijst het
//! board.
//!
//! Wie `dev` een adres geeft dat niet uit `layout` komt, heeft een bug: dit
//! is de afspraak die de API veilig houdt zonder `unsafe` bij de aanroeper
//! (handboek §5). Het vertrouwen zit in de layout, niet hier.
//!
//! Device-nGnRnE-geheugen eist natuurlijk gealigneerde toegang; een 64-bit
//! store op een niet-8-gealigneerd adres abort. De bulk-helpers doen daarom
//! een byte-proloog tot 8-alignment, dan 8-byte-woorden, dan een
//! byte-epiloog (bytes zijn per definitie gealigneerd).
//!
//! En de ene wachtlus van elke driver: [`poll_until`] (een register tot het
//! goed staat, hoogstens zo lang) en [`delay`], op de klok die het board de
//! driver geeft.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

#[cfg(target_arch = "aarch64")]
pub mod mem;

use core::cell::UnsafeCell;
use core::ptr;

/// Een fysiek adres, zoals de layout het uitdeelt.
///
/// Een `Pa` is een getal, geen pointer: hij draagt geen provenance en geen
/// lifetime. De omzetting naar een pointer gebeurt uitsluitend in deze
/// crate.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[repr(transparent)]
pub struct Pa(pub u64);

impl Pa {
    /// Telt `off` bytes op, zonder omloop: een adres dat omloopt is een bug.
    #[must_use]
    pub const fn add(self, off: u64) -> Pa {
        Pa(self.0.wrapping_add(off))
    }

    /// Het adres als `usize`, voor rekenwerk met lengtes.
    #[must_use]
    pub const fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Is dit adres een veelvoud van `align`?
    #[must_use]
    pub const fn is_aligned(self, align: u64) -> bool {
        self.0.is_multiple_of(align)
    }
}

/// De cacheline-maat waarop `push` en `pull` werken. 64 bytes op elke core
/// die HopOS draait (A55 tot en met de M4, en de C906).
pub const LINE: u64 = 64;

macro_rules! access {
    ($read:ident, $write:ident, $t:ty) => {
        /// Gealigneerde vluchtige lees op fysiek adres `pa`.
        #[must_use]
        #[inline]
        pub fn $read(pa: Pa) -> $t {
            debug_assert!(pa.is_aligned(core::mem::size_of::<$t>() as u64));
            // SAFETY: `pa` komt uit de layout en is gealigneerd (zie de
            // crate-doc); een vluchtige lees van een gemapt device-adres
            // heeft geen andere voorwaarde.
            unsafe { ptr::read_volatile(pa.as_usize() as *const $t) }
        }

        /// Gealigneerde vluchtige schrijf op fysiek adres `pa`.
        #[inline]
        pub fn $write(pa: Pa, v: $t) {
            debug_assert!(pa.is_aligned(core::mem::size_of::<$t>() as u64));
            // SAFETY: zie `read`; de schrijf raakt precies één gealigneerd
            // woord dat de layout aan de aanroeper gaf.
            unsafe { ptr::write_volatile(pa.as_usize() as *mut $t, v) }
        }
    };
}

access!(read8, write8, u8);
access!(read16, write16, u16);
access!(read32, write32, u32);
access!(read64, write64, u64);

/// Kopieert `src` naar device-geheugen op `dst`: byte-proloog tot
/// 8-alignment, dan 8-byte-woorden, dan de staart per byte.
pub fn copy_in(dst: Pa, src: &[u8]) {
    let mut p = dst;
    let mut s = src;
    while !s.is_empty() && !p.is_aligned(8) {
        write8(p, s[0]);
        p = p.add(1);
        s = &s[1..];
    }
    let mut words = s.chunks_exact(8);
    for w in &mut words {
        let mut b = [0u8; 8];
        b.copy_from_slice(w);
        write64(p, u64::from_le_bytes(b));
        p = p.add(8);
    }
    for &b in words.remainder() {
        write8(p, b);
        p = p.add(1);
    }
}

/// Kopieert device-geheugen op `src` naar `dst`; zelfde stappen als
/// `copy_in`.
pub fn copy_out(dst: &mut [u8], src: Pa) {
    let mut p = src;
    let mut d = dst;
    while !d.is_empty() && !p.is_aligned(8) {
        d[0] = read8(p);
        p = p.add(1);
        d = &mut d[1..];
    }
    let mut words = d.chunks_exact_mut(8);
    for w in &mut words {
        w.copy_from_slice(&read64(p).to_le_bytes());
        p = p.add(8);
    }
    for b in words.into_remainder() {
        *b = read8(p);
        p = p.add(1);
    }
}

/// Kopieert `src` naar gedeeld geheugen op `dst` met een gewone `memcpy`,
/// zonder vluchtige woorden: de compiler en de core mogen bundelen en
/// vooruit lezen. Alleen voor geheugen dat op dit adres Normal gemapt is
/// (op Device is een samengevoegde of ongealigneerde toegang een fault) en
/// waar de tegenpartij tijdens de kopie niet in leest of schrijft, zoals een
/// ringrecord vóór zijn publicatie. Voor al het andere: [`copy_in`].
#[inline]
pub fn copy_in_normal(dst: Pa, src: &[u8]) {
    // SAFETY: `dst` komt uit de layout (zie de crate-doc) en is daar voor
    // `src.len()` bytes van de aanroeper; die belooft bovendien dat het
    // bereik Normal gemapt is en dat niemand anders het tijdens de kopie
    // aanraakt. `src` is een geldige slice in eigen geheugen, dus de twee
    // overlappen niet.
    unsafe { ptr::copy_nonoverlapping(src.as_ptr(), dst.as_usize() as *mut u8, src.len()) }
}

/// Kopieert gedeeld Normal-geheugen op `src` naar `dst` met een gewone
/// `memcpy`; dezelfde voorwaarden als [`copy_in_normal`]: Normal gemapt, en
/// de tegenpartij schrijft er tijdens de kopie niet in (een gepubliceerd
/// ringrecord dat de lezer nog niet vrijgaf).
#[inline]
pub fn copy_out_normal(dst: &mut [u8], src: Pa) {
    // SAFETY: zie `copy_in_normal`; `dst` is een geldige, eigen slice en
    // overlapt dus niet met het gedeelde bereik.
    unsafe { ptr::copy_nonoverlapping(src.as_usize() as *const u8, dst.as_mut_ptr(), dst.len()) }
}

/// Leent `n` bytes gedeeld Normal-geheugen op `pa` als slice aan `f`,
/// zonder kopie: een ringrecord dat de lezer in plaats leest. Alleen voor
/// geheugen dat op dit adres Normal gemapt is en dat zolang niet hergebruikt
/// wordt (een gepubliceerd record dat de lezer nog niet vrijgaf). Een
/// producer op een andere core kan er tijdens `f` toch in schrijven; wie hem
/// niet vertrouwt, kopieert in `f` en toetst de kopie. Voor Device-geheugen:
/// [`copy_out`].
#[inline]
pub fn view<T>(pa: Pa, n: usize, f: impl FnOnce(&[u8]) -> T) -> T {
    // SAFETY: `pa` komt uit de layout (zie de crate-doc) en is daar voor `n`
    // bytes van de aanroeper, die belooft dat het bereik Normal gemapt is
    // en zolang de slice leeft (alleen binnen `f`) niet hergebruikt wordt.
    // De enige andere schrijver is de producer in een andere partitie, op
    // een andere core en buiten dit programma: geen Rust-schrijver waartegen
    // het geheugenmodel een race kent. Wat hij doet, kan een byte tussen
    // twee loads laten veranderen, nooit een toegang buiten het bereik.
    f(unsafe { core::slice::from_raw_parts(pa.as_usize() as *const u8, n) })
}

/// Leent `n` bytes gedeeld Normal-geheugen op `pa` als schrijfbare slice
/// aan `f`: een ringrecord dat de producer in plaats vult. Dezelfde
/// voorwaarden als [`view`], plus: niemand leest er tijdens `f` (de ruimte
/// voorbij head, vóór de publicatie).
#[inline]
pub fn view_mut<T>(pa: Pa, n: usize, f: impl FnOnce(&mut [u8]) -> T) -> T {
    // SAFETY: als `view`; de aanroeper belooft bovendien dat niemand anders
    // het bereik leest of schrijft zolang de slice leeft (alleen binnen `f`).
    f(unsafe { core::slice::from_raw_parts_mut(pa.as_usize() as *mut u8, n) })
}

/// Wist `len` bytes device-geheugen vanaf `pa`.
pub fn clear(pa: Pa, len: usize) {
    let mut p = pa;
    let mut n = len;
    while n > 0 && !p.is_aligned(8) {
        write8(p, 0);
        p = p.add(1);
        n -= 1;
    }
    while n >= 8 {
        write64(p, 0);
        p = p.add(8);
        n -= 8;
    }
    while n > 0 {
        write8(p, 0);
        p = p.add(1);
        n -= 1;
    }
}

/// Eén getypeerd MMIO-register in een `#[repr(C)]`-registerblok.
///
/// Een driver beschrijft zijn blok als struct van `Reg<T>`-velden op de
/// offsets uit de datasheet, met een `const`-assertie per offset, en krijgt
/// er een verwijzing naar via [`regs`]. Zo staat er nergens een magisch
/// getal op de aanroepplek.
#[repr(transparent)]
pub struct Reg<T: Copy>(UnsafeCell<T>);

// SAFETY: een `Reg` is een venster op een device-register; elke toegang is
// vluchtig en het device serialiseert zelf. Meerdere lezers en schrijvers
// zijn precies wat MMIO is.
unsafe impl<T: Copy> Sync for Reg<T> {}

impl<T: Copy> Reg<T> {
    /// Vluchtige lees.
    #[must_use]
    #[inline]
    pub fn read(&self) -> T {
        // SAFETY: `self` ligt in een registerblok dat `regs` uit een
        // layout-adres maakte; het adres is gemapt en gealigneerd.
        unsafe { ptr::read_volatile(self.0.get()) }
    }

    /// Vluchtige schrijf.
    #[inline]
    pub fn write(&self, v: T) {
        // SAFETY: zie `read`.
        unsafe { ptr::write_volatile(self.0.get(), v) }
    }

    /// Lees, pas aan, schrijf terug.
    #[inline]
    pub fn update(&self, f: impl FnOnce(T) -> T) {
        self.write(f(self.read()));
    }
}

/// Geeft een registerblok `R` op fysiek adres `pa`.
///
/// # Safety
///
/// `pa` moet de basis zijn van een gemapt device-blok dat de indeling van
/// `R` heeft en dat zolang het programma draait blijft bestaan. `R` bestaat
/// uitsluitend uit [`Reg`]-velden en opvulling.
#[must_use]
pub unsafe fn regs<R>(pa: Pa) -> &'static R {
    debug_assert!(pa.is_aligned(core::mem::align_of::<R>() as u64));
    // SAFETY: door de voorwaarden van deze functie.
    unsafe { &*(pa.as_usize() as *const R) }
}

/// Wacht tot `cond` waar is, hoogstens `ns` nanoseconden op de klok `now`
/// (monotone nanoseconden, de klok die het board de driver gaf): Linux'
/// `read_poll_timeout` uit include/linux/iopoll.h, spinnend. Geeft of
/// `cond` waar werd.
///
/// Na het verlopen kijkt hij nog één keer, zoals Linux: een wachter die zelf
/// laat aan de beurt kwam (een interrupt, een trage console) meldt anders een
/// time-out over een device dat al klaar was. Een wacht met een pauze tussen
/// twee blikken pauzeert in `cond` ([`delay`]).
///
/// Busy-wait: voor de korte wachten van een driver bij de start of in een
/// call die de core toch al heeft; een taak wacht op de executor.
#[must_use]
pub fn poll_until(now: fn() -> u64, ns: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = now().saturating_add(ns);
    loop {
        if cond() {
            return true;
        }
        if now() >= deadline {
            return cond();
        }
        core::hint::spin_loop();
    }
}

/// Spint `ns` nanoseconden op de klok `now` (Linux `ndelay`).
pub fn delay(now: fn() -> u64, ns: u64) {
    let end = now().saturating_add(ns);
    while now() < end {
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// Barrières en cache-onderhoud: per architectuur, op de host een no-op.
// ---------------------------------------------------------------------------

/// Volledige geheugenbarrière.
#[inline]
pub fn mb() {
    arch::mb();
}

/// Publiceert `len` bytes vanaf `pa` naar het geheugen (cache clean), zodat
/// een lezer zonder cache (een DMA-motor, een core met de MMU uit) ze ziet.
/// Aanroepen ná het schrijven en vóór het ophogen van een ringkop.
#[inline]
pub fn push(pa: Pa, len: usize) {
    arch::push(pa, len);
}

/// Haalt `len` bytes vanaf `pa` vers uit het geheugen (cache clean en
/// invalidate), zodat wat een ander schreef zichtbaar wordt. Aanroepen vóór
/// het lezen.
#[inline]
pub fn pull(pa: Pa, len: usize) {
    arch::pull(pa, len);
}

/// De bel: het producer-naar-consumer-signaal na een leeg-naar-niet-leeg
/// overgang. Op ARM64 `dsb sy; sev`, op RISC-V een fence (de kick doet het
/// board), op de host niets. Code boven `dev` kent alleen deze betekenis.
#[inline]
pub fn notify() {
    arch::notify();
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod arch {
    use super::{LINE, Pa};
    use core::arch::asm;

    #[inline]
    pub(super) fn mb() {
        // SAFETY: een barrière heeft geen geheugeneffect buiten de ordening.
        unsafe { asm!("dsb sy", options(nostack, preserves_flags)) }
    }

    #[inline]
    fn lines(pa: Pa, len: usize, op: impl Fn(u64)) {
        let start = pa.0 & !(LINE - 1);
        let end = pa.0.wrapping_add(len as u64);
        let mut a = start;
        while a < end {
            op(a);
            a = a.wrapping_add(LINE);
        }
    }

    #[inline]
    pub(super) fn push(pa: Pa, len: usize) {
        lines(pa, len, |a| {
            // SAFETY: `dc cvac` op een gemapt adres; alleen cache-effect.
            unsafe { asm!("dc cvac, {}", in(reg) a, options(nostack, preserves_flags)) }
        });
        mb();
    }

    #[inline]
    pub(super) fn pull(pa: Pa, len: usize) {
        mb();
        lines(pa, len, |a| {
            // SAFETY: `dc civac` (clean én invalidate): een eigen vuile regel
            // gaat eerst naar het geheugen, dus er raakt niets kwijt.
            unsafe { asm!("dc civac, {}", in(reg) a, options(nostack, preserves_flags)) }
        });
        mb();
    }

    #[inline]
    pub(super) fn notify() {
        // SAFETY: `dsb sy; sev` heeft geen geheugeneffect buiten de ordening.
        unsafe { asm!("dsb sy", "sev", options(nostack, preserves_flags)) }
    }
}

#[cfg(all(target_os = "none", target_arch = "riscv64"))]
mod arch {
    //! RISC-V. De C906 van de LicheeRV is NIET cache-coherent met DMA-masters
    //! (de dwmac) en niet met het andere hart: elke gedeelde buffer gaat door
    //! het T-Head-onderhoud (XuanTie CMO, van vóór Zicbom). Met de feature
    //! `thead` doen `push`/`pull` dat per regel van [`LINE`] bytes; zonder
    //! (QEMU virt, coherent) is een fence de hele ordening.
    //!
    //! De encodings komen 1:1 uit de vendor-kernel
    //! (linux_5.10/arch/riscv/mm/cacheflush.c), met rs1 = a0:
    //!
    //! ```text
    //! th.dcache.cpa  a0 = 0x0295000b   clean op fysiek adres
    //! th.dcache.cipa a0 = 0x02b5000b   clean + invalidate op fysiek adres
    //! th.sync.is        = 0x01b0000b   wacht tot het onderhoud af is
    //! ```
    //!
    //! De PA-varianten, omdat deze ops voor de kern zijn en de kern in
    //! machine mode zonder vertaling draait: daar ís een adres fysiek.
    use super::Pa;
    use core::arch::asm;

    #[inline]
    pub(super) fn mb() {
        // SAFETY: een fence heeft geen geheugeneffect buiten de ordening.
        unsafe { asm!("fence rw, rw", options(nostack, preserves_flags)) }
    }

    #[cfg(feature = "thead")]
    #[inline]
    fn lines(pa: Pa, len: usize, op: impl Fn(u64)) {
        use super::LINE;
        if len == 0 {
            return;
        }
        let start = pa.0 & !(LINE - 1);
        let end = pa.0.wrapping_add(len as u64);
        let mut a = start;
        while a < end {
            op(a);
            a = a.wrapping_add(LINE);
        }
        // SAFETY: `th.sync.is` wacht tot het onderhoud hierboven voltooid
        // is; geen ander effect.
        unsafe { asm!(".4byte 0x01b0000b", options(nostack, preserves_flags)) }
    }

    /// Clean: wat de CPU schreef, staat daarna in DRAM. Ná het schrijven en
    /// vóór het ophogen van een ringkop of het zetten van een OWN-bit.
    #[inline]
    pub(super) fn push(pa: Pa, len: usize) {
        mb();
        #[cfg(feature = "thead")]
        lines(pa, len, |a| {
            // SAFETY: `th.dcache.cpa` op een fysiek adres (machine mode, geen
            // vertaling) schrijft alleen een vuile regel terug.
            unsafe { asm!(".4byte 0x0295000b", in("a0") a, options(nostack, preserves_flags)) }
        });
        #[cfg(not(feature = "thead"))]
        let _ = (pa, len);
    }

    /// Clean + invalidate: wat een ander schreef, wordt daarna vers gelezen.
    /// Clean én invalidate (niet alleen invalidate), zoals `dc civac` op ARM:
    /// een eigen vuile regel gaat eerst naar DRAM, dus er raakt niets kwijt.
    /// Daarom ook de les van 30-07 (de ring die stilviel): wat twee schrijvers
    /// heeft, hoort niet in één regel, want de invalidate van de één gooit de
    /// schrijf van de ander alsnog weg als die tussen clean en lees valt.
    #[inline]
    pub(super) fn pull(pa: Pa, len: usize) {
        mb();
        #[cfg(feature = "thead")]
        lines(pa, len, |a| {
            // SAFETY: `th.dcache.cipa` op een fysiek adres: clean en
            // invalidate van één regel; geen verlies van eigen writes.
            unsafe { asm!(".4byte 0x02b5000b", in("a0") a, options(nostack, preserves_flags)) }
        });
        #[cfg(not(feature = "thead"))]
        let _ = (pa, len);
    }

    #[inline]
    pub(super) fn notify() {
        mb();
    }
}

#[cfg(not(target_os = "none"))]
mod arch {
    //! Host-stubs: het protocol is wat de tests bewijzen, de barrières
    //! bewijst het board.
    use super::Pa;

    pub(super) fn mb() {}
    pub(super) fn push(_pa: Pa, _len: usize) {}
    pub(super) fn pull(_pa: Pa, _len: usize) {}
    pub(super) fn notify() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pa_of(buf: &mut [u8]) -> Pa {
        Pa(buf.as_mut_ptr() as usize as u64)
    }

    #[test]
    fn words_round_trip() {
        let mut buf = [0u8; 64];
        let pa = pa_of(&mut buf);
        write64(pa, 0x0102_0304_0506_0708);
        assert_eq!(read64(pa), 0x0102_0304_0506_0708);
        write32(pa.add(8), 0xdead_beef);
        assert_eq!(read32(pa.add(8)), 0xdead_beef);
        write16(pa.add(12), 0xabcd);
        assert_eq!(read16(pa.add(12)), 0xabcd);
        write8(pa.add(14), 0x42);
        assert_eq!(read8(pa.add(14)), 0x42);
    }

    #[test]
    fn copy_handles_unaligned_head_and_tail() {
        let mut buf = [0u8; 48];
        let base = pa_of(&mut buf);
        let src: Vec<u8> = (1..=21).collect();
        copy_in(base.add(3), &src); // begint 3 bytes scheef, eindigt scheef
        let mut out = vec![0u8; 21];
        copy_out(&mut out, base.add(3));
        assert_eq!(out, src);
        assert_eq!(buf[0..3], [0, 0, 0]);
        assert_eq!(buf[24], 0);
    }

    #[test]
    fn normal_copy_round_trips_at_any_offset() {
        let mut buf = [0u8; 48];
        let base = pa_of(&mut buf);
        let src: Vec<u8> = (1..=21).collect();
        copy_in_normal(base.add(3), &src);
        let mut out = vec![0u8; 21];
        copy_out_normal(&mut out, base.add(3));
        assert_eq!(out, src);
        assert_eq!(buf[0..3], [0, 0, 0]);
        assert_eq!(buf[24], 0);
    }

    #[test]
    fn clear_wipes_exactly_the_range() {
        let mut buf = [0xffu8; 32];
        let base = pa_of(&mut buf);
        clear(base.add(5), 20);
        assert!(buf[..5].iter().all(|&b| b == 0xff));
        assert!(buf[5..25].iter().all(|&b| b == 0));
        assert!(buf[25..].iter().all(|&b| b == 0xff));
    }

    std::thread_local! {
        static NOW: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    }

    /// Een klok die per lees één nanoseconde verspringt.
    fn tick() -> u64 {
        NOW.with(|n| {
            n.set(n.get() + 1);
            n.get()
        })
    }

    fn time() -> u64 {
        NOW.with(core::cell::Cell::get)
    }

    #[test]
    fn poll_until_stops_at_the_first_yes() {
        let mut looks = 0;
        assert!(poll_until(tick, 1_000, || {
            looks += 1;
            looks == 3
        }));
        assert_eq!(looks, 3);
    }

    #[test]
    fn poll_until_gives_up_after_ns_with_one_last_look() {
        let start = time();
        let mut last = 0;
        assert!(!poll_until(tick, 10, || {
            last = time();
            false
        }));
        assert!(
            last >= start + 1 + 10,
            "the last look comes after the deadline"
        );
    }

    #[test]
    fn a_yes_on_the_last_look_counts() {
        let deadline = time() + 1 + 10;
        assert!(poll_until(tick, 10, || time() >= deadline));
    }

    #[test]
    fn poll_until_without_end_does_not_overflow() {
        let mut looks = 0;
        assert!(poll_until(tick, u64::MAX, || {
            looks += 1;
            looks == 5
        }));
    }

    #[test]
    fn delay_spins_at_least_ns() {
        let start = time();
        delay(tick, 25);
        assert!(time() >= start + 1 + 25);
    }

    #[repr(C)]
    struct Block {
        ctrl: Reg<u32>,
        _pad: u32,
        data: Reg<u64>,
    }
    const _: () = assert!(core::mem::offset_of!(Block, data) == 8);

    #[test]
    fn typed_registers() {
        let mut buf = [0u8; 16];
        let base = pa_of(&mut buf);
        // SAFETY: de buffer leeft zolang de test loopt en heeft de indeling
        // van `Block`; voor een test is dat "voor altijd" genoeg.
        let b: &Block = unsafe { regs(base) };
        b.ctrl.write(7);
        b.data.write(9);
        b.ctrl.update(|v| v + 1);
        assert_eq!(read32(base), 8);
        assert_eq!(read64(base.add(8)), 9);
    }
}
