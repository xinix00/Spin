//! De losse instructies van een app-core: de teller, de slaap, de yield en
//! de exit. Op ARM64 en RISC-V echte instructies; elders een stub-module met dezelfde
//! signaturen, zodat de logica eromheen op de host test (handboek §7: `cfg`
//! op module-niveau).
//!
//! Waarom dit in applib staat en niet in `cpu`: een app-image is board-loos
//! (de kooi ís het board, `board/hopslot` in de Go-boom), en deze zes
//! instructies zijn alles wat een gekooide core van zijn silicium ziet.

pub(crate) use imp::*;

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod imp {
    use core::arch::asm;

    /// De vrijlopende teller (CNTVCT_EL0: virtueel, trapt nooit op EL1 en
    /// telt door in WFE).
    #[inline]
    pub(crate) fn counter() -> u64 {
        let v: u64;
        // SAFETY: een lees van een systeemregister zonder bijwerkingen.
        unsafe {
            asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack, preserves_flags))
        };
        v
    }

    /// De frequentie van de teller (CNTFRQ_EL0, gezet door de firmware).
    #[inline]
    pub(crate) fn counter_hz() -> u64 {
        let v: u64;
        // SAFETY: een lees van een systeemregister zonder bijwerkingen.
        unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }

    /// CNTFRQ_EL0 is de waarheid op arm64: het woord van de kern (dat
    /// hetzelfde getal draagt) verandert niets.
    #[inline]
    pub(crate) fn set_counter_hz(_hz: u64) {}

    /// Op arm64 is de kick altijd HVC #6: niets over te nemen.
    #[inline]
    pub(crate) fn adopt_kick(_mode: u64) {}

    /// ID_AA64MMFR0_EL1: bits 63:60 zijn FEAT_ECV.
    #[inline]
    pub(crate) fn mmfr0() -> u64 {
        let v: u64;
        // SAFETY: een ID-register lezen heeft geen bijwerkingen.
        unsafe {
            asm!("mrs {}, id_aa64mmfr0_el1", out(reg) v, options(nomem, nostack, preserves_flags))
        };
        v
    }

    /// Zet CNTKCTL_EL1 (de event-stream van de generic timer).
    #[inline]
    pub(crate) fn set_cntkctl(v: u64) {
        // SAFETY: CNTKCTL_EL1 regelt alleen de event-stream en de EL0-toegang
        // tot de teller; een app op EL1 mag hem zetten en er hangt geen
        // geheugen van af.
        unsafe {
            asm!("msr cntkctl_el1, {}", "isb", in(reg) v, options(nomem, nostack, preserves_flags))
        };
    }

    /// Eén WFE; geeft de tikken die hij werkelijk duurde.
    #[inline]
    pub(crate) fn wfe() -> u64 {
        let a = counter();
        // SAFETY: WFE wacht op een event (SEV, de event-stream, een
        // interrupt) en raakt geen geheugen of registers.
        unsafe { asm!("wfe", options(nomem, nostack, preserves_flags)) };
        counter().wrapping_sub(a)
    }

    /// De coöperatieve yield naar de EL2-switcher (HVC #1), met de wektijd
    /// in x1: vóór die tellerstand hoeft de rotatie ons niet te hervatten.
    /// Geeft de idle-wall-tijd in tikken (mede-bewoner plus slaap).
    ///
    /// De Go-versie bewaarde V8-V15 en FPCR zelf omdat de switcher op EL2
    /// met de MMU uit geen FP-store naar Device-geheugen kan doen. Deze runtime
    /// bewaart bij hardwarefloat dezelfde callee-saved staat op zijn eigen
    /// Normal-WB-stack. Bij softfloat raakt de compiler die registers niet.
    #[inline]
    pub(crate) fn hvc_yield(deadline: u64) -> u64 {
        let a = counter();
        // SAFETY: de switcher bewaart en herstelt onze GP- en
        // systeemregisters en hervat ons na de HVC; `clobber_abi("C")` laat
        // de compiler alle caller-saved registers als verloren beschouwen,
        // dus ook als de switcher er een omgooit, is dat geen fout.
        unsafe {
            #[cfg(target_abi = "softfloat")]
            asm!("hvc #1", in("x1") deadline, clobber_abi("C"), options(nostack));
            #[cfg(not(target_abi = "softfloat"))]
            asm!(
                "sub sp, sp, #80",
                "stp d8, d9, [sp, #0]",
                "stp d10, d11, [sp, #16]",
                "stp d12, d13, [sp, #32]",
                "stp d14, d15, [sp, #48]",
                "mrs x9, fpcr",
                "mrs x10, fpsr",
                "stp x9, x10, [sp, #64]",
                "hvc #1",
                "ldp x9, x10, [sp, #64]",
                "msr fpcr, x9",
                "msr fpsr, x10",
                "ldp d8, d9, [sp, #0]",
                "ldp d10, d11, [sp, #16]",
                "ldp d12, d13, [sp, #32]",
                "ldp d14, d15, [sp, #48]",
                "add sp, sp, #80",
                in("x1") deadline, clobber_abi("C"),
            );
        }
        counter().wrapping_sub(a)
    }

    /// De expliciete bel naar de OS-core (HVC #6): de switcher van deze
    /// core stuurt de kick-SGI als de kern op dat moment geen SEV hoort (hij
    /// draait een bewoner of slaapt in WFI; sched-blok 0 zegt het), en
    /// hervat ons meteen. Op de OS-core zelf is het een yield naar nu: de
    /// kern draait zijn ronde en geeft de core terug.
    #[inline]
    pub(crate) fn hvc_kick_os() {
        // SAFETY: HVC #6 trapt naar de EL2-switcher; die gebruikt alleen
        // x2/x3 als klad, zet x0..x3 terug uit zijn scratch en keert met ERET
        // terug naar de instructie hierna (`switch.rs`, `.Lkickos`). Op de
        // OS-core bewaart de rotatie de hele ctx en hervat hem daar ook
        // (`oscore.rs`, `settle`). Geen `nomem`: de publicatie op de ring
        // moet vóór de trap staan (de aanroeper deed `dev::notify`, een DSB).
        unsafe { asm!("hvc #6", options(nostack, preserves_flags)) };
    }

    /// Wekt de sibling-core van deze app met affiniteit `aff` (HVC #4): de
    /// switcher zoekt hem in de vertrouwde keten van de eenheid, zet zijn
    /// wek-latch en zijn wektijd op nu, en hervat ons meteen. Een sibling die
    /// nog draait, ziet de latch bij zijn volgende yield (de lost wakeup van
    /// 04-09). De SEV erna wekt een switcher die in WFE slaapt.
    #[inline]
    pub(crate) fn hvc_wake(aff: u64) {
        // SAFETY: HVC #4 trapt naar de EL2-switcher, die alleen x0..x3 als
        // klad gebruikt en ze uit zijn scratch terugzet (`switch.rs`,
        // `.Lwake`), en met ERET terugkeert naar de instructie hierna. De
        // keten die hij afloopt is vertrouwd (de kern zette hem), dus `aff`
        // kan alleen een context van déze app raken. Geen `nomem`: wat de
        // aanroeper voor de sibling klaarzette, moet vóór de wek zichtbaar
        // zijn.
        unsafe {
            asm!("hvc #4", "sev", in("x0") aff, options(nostack, preserves_flags));
        }
    }

    /// De MPIDR-affiniteit van deze core (aff0..aff2), zoals de switcher hem
    /// in het wekdoel zet. Op EL1 is dit VMPIDR_EL2, en de trampolines zetten
    /// die op de echte MPIDR.
    #[inline]
    pub(crate) fn core_id() -> u64 {
        let v: u64;
        // SAFETY: een lees van een systeemregister zonder bijwerkingen.
        unsafe { asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v & 0xFF_FFFF
    }

    /// Geeft de core aan de kern terug (HVC #0 naar de EL2-parkeerlus).
    /// PSCI CPU_OFF was op de Pi 5-stockfirmware een deur zonder terugweg;
    /// de kern bezit zijn cores en ze gaan nooit terug naar de firmware.
    pub(crate) fn park_exit() -> ! {
        loop {
            // SAFETY: HVC #0 trapt naar de EL2-vectoren van de kern, die de
            // core parkeert; de status staat al op de control-page. Keert
            // in de praktijk niet terug, en doet hij dat toch, dan opnieuw.
            unsafe { asm!("hvc #0", options(nomem, nostack)) };
        }
    }
}

#[cfg(all(target_os = "none", target_arch = "riscv64"))]
mod imp {
    //! RISC-V (supervisor mode onder de M-mode-switcher van de kern,
    //! `cpu::riscv::switch`). Dezelfde zes werkwoorden als op ARM, andere
    //! letters: de teller is de TIME-CSR, de yield en de exit zijn `ecall`
    //! met a7 = 0 (wektijd in a0) en a7 = 1. Er is geen event-register en
    //! een `wfi` van een bewoner wekt nooit (de switcher laat hem met mie = 0
    //! draaien): elke wacht is een yield (Go, cpu/idle/idle_riscv64.go:
    //! "de ecall is zijn enige route naar een wfi").
    use core::arch::asm;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

    /// De timebase van de TIME-CSR tot de control-page iets anders zegt: de
    /// 10 MHz van QEMU virt. RISC-V heeft geen register waaruit hij volgt
    /// (ARM heeft CNTFRQ_EL0); de LicheeRV telt 25 MHz, en zonder het woord
    /// van de kern liep de klok van een app daar 2,5x te traag (Go,
    /// board/hopslot: "komt er een tweede board, dan op de control-page").
    const DEFAULT_TIMEBASE_HZ: u64 = 10_000_000;

    /// De timebase van deze app: [`DEFAULT_TIMEBASE_HZ`], of wat de kern op
    /// de control-page zette (`CTRL_TIMEBASE_HZ`, via
    /// [`crate::clock::adopt_timebase`]). Eén schrijver, vóór de eerste
    /// klok-lees; daarna alleen lezers.
    static TIMEBASE_HZ: AtomicU64 = AtomicU64::new(DEFAULT_TIMEBASE_HZ);

    /// De TIME-CSR.
    #[inline]
    pub(crate) fn counter() -> u64 {
        let v: u64;
        // SAFETY: `rdtime` leest een teller zonder bijwerkingen; de switcher
        // zet `mcounteren.TM` in `parkenter`, dus in S-mode trapt hij niet.
        unsafe { asm!("rdtime {}", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }

    /// De timebase.
    #[inline]
    pub(crate) fn counter_hz() -> u64 {
        TIMEBASE_HZ.load(Relaxed)
    }

    /// Neemt de timebase van de kern over; 0 (een kern die het woord nog
    /// niet kent) laat de default staan.
    pub(crate) fn set_counter_hz(hz: u64) {
        if hz != 0 {
            TIMEBASE_HZ.store(hz, Relaxed);
        }
    }

    /// Geen ID-register met FEAT_ECV: nul.
    #[inline]
    pub(crate) fn mmfr0() -> u64 {
        0
    }

    /// Geen event-stream: niets te zetten.
    #[inline]
    pub(crate) fn set_cntkctl(_v: u64) {}

    /// Een korte wacht: een yield naar nu (de switcher geeft een buur zijn
    /// beurt en hervat ons), want een `wfi` wekt hier nooit.
    #[inline]
    pub(crate) fn wfe() -> u64 {
        hvc_yield(0)
    }

    /// De coöperatieve yield naar de M-mode-switcher (`ecall`, a7 = 0), met
    /// de wektijd in a0: vóór die tellerstand hoeft de rotatie ons niet te
    /// hervatten. Geeft de idle-wall-tijd in tikken.
    #[inline]
    pub(crate) fn hvc_yield(deadline: u64) -> u64 {
        let a = counter();
        // SAFETY: de switcher bewaart x1..x31 en ons S-regime en hervat ons
        // op de instructie na de `ecall` (mepc + 4). `clobber_abi("C")` laat
        // de compiler alle caller-saved registers als verloren beschouwen,
        // ook de FP-registers, die de switcher niet bewaart.
        unsafe {
            asm!(
                "ecall",
                inout("a0") deadline => _,
                in("a7") 0u64,
                clobber_abi("C"),
                options(nostack),
            );
        }
        counter().wrapping_sub(a)
    }

    /// Hoort de kern de kick (`IDLE_KICK` op de control-page)? Eén
    /// schrijver bij de start ([`adopt_kick`]), daarna alleen lezers.
    static KICK: AtomicBool = AtomicBool::new(false);

    /// Neemt `CTRL_IDLE_MODE` over: kicken alleen als de kern het bit zette
    /// (een switcher van vóór 02-10 neemt a7 = 2 als exit).
    pub(crate) fn adopt_kick(mode: u64) {
        KICK.store(mode & abi::hopabi::IDLE_KICK != 0, Relaxed);
    }

    /// De bel naar de kern na een publicatie op de TX-ring (`ecall`,
    /// a7 = 2; de tegenhanger van HVC #6). Op een app-hart belt de switcher
    /// de OS-core en hervat ons meteen; op de OS-core is het een yield naar
    /// nu (de kern draait zijn ronde). Zonder bel naar de kern hoort hij het
    /// frame pas op zijn failsafe van 1 ms.
    #[inline]
    pub(crate) fn hvc_kick_os() {
        if !KICK.load(Relaxed) {
            return;
        }
        // SAFETY: de switcher raakt alleen t0..t2 aan en zet ze terug uit
        // zijn scratch; de rotatie van de OS-core bewaart x1..x31 en hervat
        // op mepc + 4. `clobber_abi("C")` zoals bij de yield: op de OS-core
        // bewaart de rotatie de f-registers niet. Geen `nomem`: de
        // publicatie op de ring moet vóór de trap staan.
        unsafe {
            asm!("ecall", in("a7") 2u64, clobber_abi("C"), options(nostack));
        }
    }

    /// Geen SMP-apps op riscv64: niets te wekken.
    #[inline]
    pub(crate) fn hvc_wake(_aff: u64) {}

    /// Eén core per slot op riscv64; S-mode kan `mhartid` niet lezen.
    #[inline]
    pub(crate) fn core_id() -> u64 {
        0
    }

    /// Klaar: `ecall` met a7 = 1. De switcher zet ons dood, veegt de cache
    /// en roteert weg; het hart draait door voor de buren.
    pub(crate) fn park_exit() -> ! {
        loop {
            // SAFETY: de exit-ecall keert niet terug (de switcher hervat een
            // dode bewoner nooit); doet hij het toch, dan opnieuw.
            unsafe { asm!("ecall", in("a7") 1u64, options(nomem, nostack)) };
        }
    }
}

#[cfg(not(any(
    all(target_os = "none", target_arch = "aarch64"),
    all(target_os = "none", target_arch = "riscv64")
)))]
mod imp {
    //! Host-stub: dezelfde signaturen, geen ijzer. De teller is een
    //! getal dat de tests zetten; slapen en yielden duren niets.
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

    /// De nep-teller van de host.
    pub(crate) static FAKE_COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn counter() -> u64 {
        FAKE_COUNTER.load(Relaxed)
    }

    pub(crate) fn counter_hz() -> u64 {
        1_000_000_000
    }

    /// De host-teller is een getal van de tests: niets over te nemen.
    pub(crate) fn set_counter_hz(_hz: u64) {}

    /// Geen kern om te kicken: niets over te nemen.
    pub(crate) fn adopt_kick(_mode: u64) {}

    pub(crate) fn mmfr0() -> u64 {
        0
    }

    pub(crate) fn set_cntkctl(_v: u64) {}

    pub(crate) fn wfe() -> u64 {
        0
    }

    pub(crate) fn hvc_yield(_deadline: u64) -> u64 {
        0
    }

    /// Geen kern om te kicken: een no-op.
    pub(crate) fn hvc_kick_os() {}

    /// Geen switcher en geen sibling: een no-op.
    pub(crate) fn hvc_wake(_aff: u64) {}

    /// De host is één core.
    pub(crate) fn core_id() -> u64 {
        0
    }

    pub(crate) fn park_exit() -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}
