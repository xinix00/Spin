//! Een waker-slot dat zonder slot te delen is: [`AtomicWaker`], en een
//! kleine set ervan voor iets waar meerdere taken op wachten.
//!
//! Het ontwerp is dat van `futures::task::AtomicWaker`: een toestandswoord
//! met twee bits (`REGISTERING`, `WAKING`) bewaakt één `Option<Waker>`.
//! Registreren en wekken mogen van twee kanten tegelijk komen (een taak en
//! een ISR, of twee cores); wie het slot niet te pakken krijgt, weet dat de
//! ander het werk doet.

use core::cell::UnsafeCell;
use core::sync::atomic::{
    AtomicBool, AtomicU32, AtomicUsize,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use core::task::Waker;

const WAITING: usize = 0;
const REGISTERING: usize = 0b01;
const WAKING: usize = 0b10;

/// Eén waker-slot, deelbaar tussen een taak en wie hem wekt.
///
/// # Invariants
///
/// Het slot `waker` wordt alleen aangeraakt door wie via `state` het
/// `REGISTERING`- of het `WAKING`-bit heeft gewonnen.
pub struct AtomicWaker {
    state: AtomicUsize,
    waker: UnsafeCell<Option<Waker>>,
}

// SAFETY: de toegang tot `waker` wordt volledig door `state` geserialiseerd
// (zie de invariant); een `Waker` is zelf `Send + Sync`.
unsafe impl Send for AtomicWaker {}
// SAFETY: zie `Send`.
unsafe impl Sync for AtomicWaker {}

impl AtomicWaker {
    /// Een leeg slot.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicUsize::new(WAITING),
            waker: UnsafeCell::new(None),
        }
    }

    /// Onthoudt `w` als de waker die [`wake`](Self::wake) moet roepen.
    ///
    /// Komt er tijdens het registreren een wake binnen, dan wordt `w`
    /// meteen gewekt: level-triggered, geen verloren wek.
    pub fn register(&self, w: &Waker) {
        match self
            .state
            .compare_exchange(WAITING, REGISTERING, Acquire, Acquire)
        {
            Ok(_) => {
                // SAFETY: wij houden het REGISTERING-bit, dus niemand
                // anders raakt het slot (invariant).
                unsafe {
                    let slot = &mut *self.waker.get();
                    if !slot.as_ref().is_some_and(|old| old.will_wake(w)) {
                        *slot = Some(w.clone());
                    }
                }
                match self
                    .state
                    .compare_exchange(REGISTERING, WAITING, AcqRel, Acquire)
                {
                    Ok(_) => {}
                    Err(_) => {
                        // Een wake kwam binnen terwijl wij registreerden
                        // (state is REGISTERING | WAKING). De wekker heeft
                        // het slot niet aangeraakt en laat het aan ons.
                        // SAFETY: nog steeds de enige met het REGISTERING-bit.
                        let w = unsafe { (*self.waker.get()).take() };
                        self.state.swap(WAITING, AcqRel);
                        if let Some(w) = w {
                            w.wake();
                        }
                    }
                }
            }
            Err(WAKING) => {
                // Er wordt nu gewekt: dat geldt ook voor ons.
                w.wake_by_ref();
            }
            Err(_) => {
                // Een andere taak registreert net. Op één core kan dat niet;
                // over cores heen is het een ontwerpfout (één wachter per
                // slot), maar we laten geen wek verloren gaan: wek onszelf,
                // dan komt de volgende poll terug.
                w.wake_by_ref();
            }
        }
    }

    /// Wekt de geregistreerde waker, als die er is, en vergeet hem.
    pub fn wake(&self) {
        if let Some(w) = self.take() {
            w.wake();
        }
    }

    /// Haalt de geregistreerde waker eruit zonder hem te wekken.
    pub fn take(&self) -> Option<Waker> {
        match self.state.fetch_or(WAKING, AcqRel) {
            WAITING => {
                // SAFETY: state was WAITING en draagt nu ons WAKING-bit, dus
                // wij zijn de enige die het slot aanraakt (invariant).
                let w = unsafe { (*self.waker.get()).take() };
                self.state.fetch_and(!WAKING, Release);
                w
            }
            // Een registratie is bezig (die ziet ons bit en wekt zelf) of
            // een andere wake loopt al: één wek is genoeg.
            _ => None,
        }
    }
}

impl Default for AtomicWaker {
    fn default() -> Self {
        Self::new()
    }
}

/// Een vaste set waker-slots voor iets waar meerdere taken op wachten
/// ([`Stop`](crate::Stop)).
///
/// Een wachter claimt bij zijn eerste poll een slot en geeft het bij `Drop`
/// terug. Zijn de slots op, dan meldt `register` dat met `false`; de
/// wachter wekt dan zichzelf (een spin op ronde-korrel: correct, niet
/// zuinig) en de teller [`overflows`](Self::overflows) maakt het meetbaar.
pub struct WakerSet<const N: usize> {
    slots: [AtomicWaker; N],
    used: [AtomicBool; N],
    overflows: AtomicU32,
}

impl<const N: usize> WakerSet<N> {
    /// Een lege set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { AtomicWaker::new() }; N],
            used: [const { AtomicBool::new(false) }; N],
            overflows: AtomicU32::new(0),
        }
    }

    /// Registreert `w` in het slot van deze wachter (`slot`), en claimt er
    /// eerst een als de wachter er nog geen heeft. `false` = geen slot vrij.
    pub fn register(&self, slot: &mut Option<usize>, w: &Waker) -> bool {
        if slot.is_none() {
            for (i, used) in self.used.iter().enumerate() {
                if used.compare_exchange(false, true, AcqRel, Acquire).is_ok() {
                    *slot = Some(i);
                    break;
                }
            }
        }
        match *slot {
            Some(i) => {
                self.slots[i].register(w);
                true
            }
            None => {
                self.overflows.fetch_add(1, Relaxed);
                false
            }
        }
    }

    /// Geeft het slot van deze wachter terug.
    pub fn release(&self, slot: &mut Option<usize>) {
        if let Some(i) = slot.take() {
            let _ = self.slots[i].take();
            self.used[i].store(false, Release);
        }
    }

    /// Wekt elke wachter met een slot.
    pub fn wake_all(&self) {
        for (i, used) in self.used.iter().enumerate() {
            if used.load(Acquire) {
                self.slots[i].wake();
            }
        }
    }

    /// Hoe vaak een wachter geen slot kreeg (cumulatief): de meetlat.
    #[must_use]
    pub fn overflows(&self) -> u32 {
        self.overflows.load(Relaxed)
    }
}

impl<const N: usize> Default for WakerSet<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::waker;
    use std::sync::atomic::Ordering::SeqCst;

    #[test]
    fn register_then_wake() {
        let (c, w) = waker();
        let a = AtomicWaker::new();
        a.wake(); // niemand geregistreerd: niets gebeurt
        a.register(&w);
        assert_eq!(c.0.load(SeqCst), 0);
        a.wake();
        assert_eq!(c.0.load(SeqCst), 1);
        a.wake(); // vergeten na de eerste wek
        assert_eq!(c.0.load(SeqCst), 1);
    }

    #[test]
    fn take_returns_the_waker_without_waking() {
        let (c, w) = waker();
        let a = AtomicWaker::new();
        a.register(&w);
        let taken = a.take();
        assert!(taken.is_some());
        assert_eq!(c.0.load(SeqCst), 0);
        assert!(a.take().is_none());
    }

    #[test]
    fn waker_set_wakes_everyone_and_reuses_slots() {
        let set: WakerSet<2> = WakerSet::new();
        let (c1, w1) = waker();
        let (c2, w2) = waker();
        let (c3, w3) = waker();
        let (mut s1, mut s2, mut s3) = (None, None, None);
        assert!(set.register(&mut s1, &w1));
        assert!(set.register(&mut s2, &w2));
        assert!(!set.register(&mut s3, &w3)); // vol
        assert_eq!(set.overflows(), 1);
        set.wake_all();
        assert_eq!(
            (c1.0.load(SeqCst), c2.0.load(SeqCst), c3.0.load(SeqCst)),
            (1, 1, 0)
        );
        set.release(&mut s1);
        assert!(set.register(&mut s3, &w3)); // het slot van 1 is vrij
        assert_eq!(s3, Some(0));
    }
}
