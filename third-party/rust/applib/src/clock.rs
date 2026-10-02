//! De klok van een app: de generic timer in nanoseconden, de wektijd voor de
//! yield, en de event-stream die elke WFE begrenst.
//!
//! De teller is gedeeld over alle cores; de kern synct de wandklok (SNTP)
//! en zet de offset op de control-page, dus die geldt één op één.

use crate::arch;
use crate::ctrl::Ctrl;
use core::time::Duration;

const NS: u128 = 1_000_000_000;

/// Tellertikken naar nanoseconden, zonder overloop (via u128).
#[must_use]
pub fn ticks_to_ns(ticks: u64, hz: u64) -> u64 {
    if hz == 0 {
        return 0;
    }
    u64::try_from(u128::from(ticks) * NS / u128::from(hz)).unwrap_or(u64::MAX)
}

/// Nanoseconden naar tellertikken, zonder overloop.
#[must_use]
pub fn ns_to_ticks(ns: u64, hz: u64) -> u64 {
    u64::try_from(u128::from(ns) * u128::from(hz) / NS).unwrap_or(u64::MAX)
}

/// Neemt de timebase over die de kern op de control-page zette
/// (`CTRL_TIMEBASE_HZ`) en geeft de timebase waarmee de klok vanaf nu
/// rekent. Eén keer, in de main-schil, vóór de eerste klok-lees.
///
/// Op RISC-V is het woord de enige bron (de TIME-CSR telt 10 MHz op QEMU
/// virt en 25 MHz op de LicheeRV, en er is geen register dat het zegt); op
/// arm64 blijft CNTFRQ_EL0 de waarheid. 0 op de page (een kern van vóór het
/// woord) laat de default van de architectuur staan.
pub fn adopt_timebase(ctrl: &Ctrl) -> u64 {
    arch::set_counter_hz(ctrl.get(abi::hopabi::CTRL_TIMEBASE_HZ));
    arch::counter_hz()
}

/// Monotone nanoseconden sinds de teller begon: de klok van de executor.
#[must_use]
pub fn now_ns() -> u64 {
    ticks_to_ns(arch::counter(), arch::counter_hz())
}

/// De frequentie van de teller in Hz: wat één tik van [`now_ns`] en van de
/// `CtrlIdle`-teller op de control-page waard is.
///
/// Een app die zijn eigen idle-tikken ([`crate::Ctrl::idle_ticks`]) naast
/// de wandklok legt (het idle-percentage van de BURN-rol in `apps/bench`),
/// rekent met dit getal, niet met een aanname: de Pi telt op 54 MHz, QEMU
/// op 62,5, de Altra op 25 en de M4 op 1 GHz.
#[must_use]
pub fn hz() -> u64 {
    arch::counter_hz()
}

/// De langste slaap die een wektijd uitdrukt: een uur is "nooit" genoeg,
/// een kick komt eerder. Geklemd omdat een onbegrensde deadline maal de
/// frequentie overloopt tot een willekeurige wektijd (gemeten in de
/// Go-governor: negatieve en vroege wektijden op een SMP-app).
pub const MAX_SLEEP: Duration = Duration::from_secs(3600);

/// De tellerstand waarop een slaap tot `until` (ns, `None` = geen timer)
/// hoort te eindigen, gezien vanaf `now` (ns) en tellerstand `counter`.
/// 0 betekent "nu": de deadline is al voorbij.
#[must_use]
pub fn wake_at(now: u64, until: Option<u64>, counter: u64, hz: u64) -> u64 {
    let max = u64::try_from(MAX_SLEEP.as_nanos()).unwrap_or(u64::MAX);
    let d = match until {
        None => max,
        Some(u) if u <= now => return 0,
        Some(u) => (u - now).min(max),
    };
    counter.saturating_add(ns_to_ticks(d, hz))
}

/// De CNTKCTL_EL1-waarde voor een event-stream met een periode van
/// hooguit `max_period`: EVNTEN plus de EVNTI-bit met de grootste periode
/// die daaronder blijft.
///
/// De 0-naar-1-flank van tellerbit EVNTI+1 is het wek-event; bit 15 op de
/// Pi's 54 MHz (1,2 ms) en QEMU's 62,5 MHz (1,05 ms), bit 14 op de Altra's
/// 25 MHz (1,3 ms; een vaste 15 gaf daar 2,6 ms). EVNTI heeft vier bits, en
/// op een GHz-teller is zelfs bit 15 te snel: gemeten 29-08 op de M4 (1 GHz)
/// 65 µs, vijftien keer te vaak wakker. Met FEAT_ECV schuift EVNTIS de
/// keuze 8 bits op; op de M4 wordt dat EVNTI 11, 1,048 ms, 954 wekken per
/// seconde, precies wat er gemeten is.
#[must_use]
pub fn event_stream_bits(hz: u64, has_ecv: bool, max_period: Duration) -> u64 {
    let shift: u32 = if has_ecv && (1u64 << 16) < hz / 2000 {
        8
    } else {
        0
    };
    let limit = max_period.as_nanos();
    let mut i: u32 = 15;
    while i > 4 && (1u128 << (i + 1 + shift)) * NS > u128::from(hz) * limit {
        i -= 1;
    }
    let mut v = (1u64 << 2) | (u64::from(i) << 4); // EVNTEN | EVNTI
    if shift != 0 {
        v |= 1 << 17; // EVNTIS: EVNTI telt in stappen van 256
    }
    v
}

/// Zet de event-stream van deze core aan: elke WFE keert dan binnen
/// ~1,5 ms terug, zodat een gemiste bel hooguit één periode kost.
pub fn start_event_stream() {
    let ecv = (arch::mmfr0() >> 60) & 0xf != 0;
    arch::set_cntkctl(event_stream_bits(
        arch::counter_hz(),
        ecv,
        Duration::from_micros(1500),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_do_not_overflow_on_a_ghz_counter() {
        assert_eq!(ticks_to_ns(62_500_000, 62_500_000), 1_000_000_000);
        assert_eq!(ns_to_ticks(1_000_000, 54_000_000), 54_000);
        // Een uur op 1 GHz past niet in een u64 als je eerst vermenigvuldigt.
        assert_eq!(
            ns_to_ticks(3_600_000_000_000, 1_000_000_000),
            3_600_000_000_000
        );
        assert_eq!(ticks_to_ns(u64::MAX, 1), u64::MAX);
        assert_eq!(ticks_to_ns(5, 0), 0);
    }

    #[test]
    fn wake_at_is_now_for_a_past_deadline_and_clamped_for_none() {
        let hz = 1_000_000_000;
        assert_eq!(wake_at(1_000, Some(500), 7, hz), 0);
        assert_eq!(wake_at(1_000, Some(1_000), 7, hz), 0);
        assert_eq!(wake_at(1_000, Some(3_000), 7, hz), 2_007);
        assert_eq!(wake_at(0, None, 0, hz), 3_600_000_000_000);
        assert_eq!(wake_at(0, Some(u64::MAX), 0, hz), 3_600_000_000_000);
    }

    #[test]
    fn event_stream_picks_the_measured_bits() {
        let p = Duration::from_micros(1500);
        let evnti = |v: u64| (v >> 4) & 0xf;
        assert_eq!(evnti(event_stream_bits(54_000_000, false, p)), 15); // Pi
        assert_eq!(evnti(event_stream_bits(62_500_000, false, p)), 15); // QEMU
        assert_eq!(evnti(event_stream_bits(25_000_000, false, p)), 14); // Altra
        let m4 = event_stream_bits(1_000_000_000, true, p);
        assert_eq!(evnti(m4), 11);
        assert_ne!(m4 & (1 << 17), 0);
        // Zonder ECV blijft een snelle teller op het plafond, zonder EVNTIS.
        let no_ecv = event_stream_bits(1_000_000_000, false, p);
        assert_eq!(evnti(no_ecv), 15);
        assert_eq!(no_ecv & (1 << 17), 0);
        assert_ne!(no_ecv & (1 << 2), 0); // EVNTEN altijd
    }
}
