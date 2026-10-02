//! De idle van de app-core: [`AppSleeper`], de [`executor::Sleeper`] van een
//! app.
//!
//! De Go-governor (`cpu/idle`) bestuurde tamago's scheduler van buitenaf,
//! met deuren, `nested`-vlaggen en een sysmon-lite. Hier is de executor van
//! ons, en blijft er één vraag over: als geen taak iets te doen heeft, hoe
//! slaapt deze core? Drie antwoorden, in deze volgorde:
//!
//! 1. **De deurbel.** Heeft de app een RX-ring (zie [`crate::net`]), dan
//!    wapent de slaper vlak vóór de slaap `CtrlRXDoor` met "gezien tot
//!    head H" (H | bit 63), en kijkt hij daarna nog één keer: een frame dat
//!    net vóór het wapenen kwam valt anders tussen wal en schip. Ligt er
//!    iets, dan belt hij de pomp en slaapt hij niet. Na de slaap ontwapent
//!    hij, zodat de kern alleen een slapende pomp kickt: één kick per burst,
//!    niet per frame (een kick per frame kostte HOP naar app 3,5×, 534 naar
//!    150 MB/s, gemeten 04-09). Een app zonder RX-ring wapent nooit; anders
//!    maakte elke ARP-flood hem permanent "due" en at de resume/yield-
//!    pingpong de gedeelde core op.
//! 2. **De yield.** Deelt dit slot zijn core (`CtrlShared`), of vraagt het
//!    board om yield-idle (`CtrlIdleMode`, Apple silicon: op de M4 slaapt een
//!    app-core op EL1 niet, gemeten 02-09), dan HVC #1 met de wektijd.
//! 3. **WFE** op de event-stream, tot er werk is of de deadline verstreek
//!    (zie [`AppSleeper::wfe_sleep`]): een wek zonder werk is geen ronde
//!    van de executor.
//!
//! De verloren-wek-race: een app-core draait met de interrupts permanent
//! gemaskeerd (hij heeft geen vectoren; de deurbel-als-vFIQ is niet
//! geport), en zijn wekken zijn SEV's en de event-stream. Een SEV die na de
//! laatste `ready()`-toets valt, zet het event-register, en de WFE daarna
//! keert meteen terug. Dat is de deur die de executor eist.

use crate::arch;
use crate::clock;
use crate::contract::RX_ARMED;
use crate::ctrl::Ctrl;
use crate::ring::Peek;
use core::cell::Cell;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use executor::Sleeper;
use sync::{Local, Signal};

/// De instructies van de slaap, achter een trait zodat de logica op de host
/// test met een nep-core.
pub trait Idle {
    /// De vrijlopende teller.
    fn counter(&self) -> u64;
    /// Tikken per seconde.
    fn counter_hz(&self) -> u64;
    /// Eén WFE; de tikken die hij duurde.
    fn wfe(&mut self) -> u64;
    /// De yield naar EL2 met wektijd `deadline` (tellerstand, 0 = nu); de
    /// tikken tot we terug waren.
    fn hvc_yield(&mut self, deadline: u64) -> u64;
}

/// De echte core.
#[derive(Debug, Default)]
pub struct Hw;

impl Idle for Hw {
    fn counter(&self) -> u64 {
        arch::counter()
    }
    fn counter_hz(&self) -> u64 {
        arch::counter_hz()
    }
    fn wfe(&mut self) -> u64 {
        arch::wfe()
    }
    fn hvc_yield(&mut self, deadline: u64) -> u64 {
        arch::hvc_yield(deadline)
    }
}

/// De deurbel van de RX-ring: de leesblik op zijn kop en de bel van de pomp.
#[derive(Copy, Clone)]
pub struct RxDoor {
    /// De kop van de RX-ring.
    pub peek: Peek,
    /// De bel waarop de RX-pomp wacht.
    pub bell: &'static Signal,
}

/// De deurbel, als de app er een heeft. Gezet door [`watch_rx`] vanuit de
/// app-executor; gelezen door de slaper op dezelfde executor.
static RX_DOOR: DoorSlot = Local::new(Cell::new(None));

/// Waar een slaper zijn deurbel zoekt: de static van de app, of in een test
/// een eigen exemplaar.
pub type DoorSlot = Local<Cell<Option<RxDoor>>>;

/// Hangt de deurbel aan: vanaf nu wapent de idle `CtrlRXDoor` en belt hij
/// `door.bell` zodra er RX ligt. Eén keer, door wie de RX-ring leest.
pub fn watch_rx(door: RxDoor) {
    RX_DOOR.get().set(Some(door));
}

/// De slaap van elke secundaire core van deze app, in tikken, per core
/// (index 0 blijft leeg): elke `crate::smp::CoreSleeper` schrijft zijn eigen
/// totaal, en de primaire publiceert de som met zijn eigen slaap in
/// `CtrlIdle`. Eén schrijver per woord, dus een kale store: een exclusive
/// (`fetch_add`) in de WFE-lus zou het event-register vullen en de volgende
/// WFE meteen laten terugkeren (Altra 18-07).
pub(crate) static SECONDARY_IDLE: [AtomicU64; crate::smp::MAX_CORES] =
    [const { AtomicU64::new(0) }; crate::smp::MAX_CORES];

/// De som van [`SECONDARY_IDLE`].
fn secondary_idle() -> u64 {
    SECONDARY_IDLE
        .iter()
        .fold(0, |a, t| a.wrapping_add(t.load(Relaxed)))
}

/// WFE's tot `woke()` of tot de teller `deadline` haalt; de tikken die ze
/// duurden. Na elke WFE krijgt `progress` de slaap tot dan: de teller op de
/// page moet meelopen, niet pas bij het verlaten van de lus. De dvfs van de
/// kern kijkt per 10 ms over een venster van 50 ms, en een lus die tot de
/// heartbeat (50 ms) slaapt, kwam anders als één klonter binnen, geklemd,
/// met lege samples ervoor: de Pi 4 las een stille tweecore-vitals als 266
/// tot 653 permille idle en bleef op 1500 MHz (30-09, stempel I). De lus
/// van [`AppSleeper::wfe_sleep`] en van `crate::smp::CoreSleeper`.
pub(crate) fn wfe_until<I: Idle>(
    idle: &mut I,
    deadline: u64,
    woke: &dyn Fn() -> bool,
    progress: &dyn Fn(u64),
) -> u64 {
    let mut slept: u64 = 0;
    while !woke() && idle.counter() < deadline {
        slept = slept.saturating_add(idle.wfe());
        progress(slept);
    }
    slept
}

/// De slaap van een app-core.
pub struct AppSleeper<I: Idle = Hw> {
    ctrl: Ctrl,
    idle: I,
    door: &'static DoorSlot,
    idle_ticks: u64,
    wakes: u64,
}

impl AppSleeper<Hw> {
    /// De slaper van deze core, op de control-page `ctrl`.
    #[must_use]
    pub fn new(ctrl: Ctrl) -> Self {
        Self::with(ctrl, Hw)
    }
}

impl<I: Idle> AppSleeper<I> {
    /// Een slaper met core `idle` (een nep-core in de tests).
    pub fn with(ctrl: Ctrl, idle: I) -> Self {
        Self {
            ctrl,
            idle,
            door: &RX_DOOR,
            idle_ticks: 0,
            wakes: 0,
        }
    }

    /// Zoekt de deurbel in `slot` in plaats van in de static van de app.
    #[must_use]
    pub fn with_door(mut self, slot: &'static DoorSlot) -> Self {
        self.door = slot;
        self
    }

    /// De core, voor de tests.
    pub fn idle(&self) -> &I {
        &self.idle
    }

    /// Geslapen tikken tot nu (de `CtrlIdle`-teller).
    #[must_use]
    pub fn idle_ticks(&self) -> u64 {
        self.idle_ticks
    }

    /// Wapent de deurbel. `false`: er ligt al RX, de pomp is gebeld en er
    /// wordt niet geslapen.
    fn arm(&self, d: RxDoor) -> bool {
        let (head, pending) = d.peek.head_pending();
        if pending {
            d.bell.set();
            return false;
        }
        self.ctrl.set_rx_door(head | RX_ARMED);
        // De hercontrole: zonder haar valt een frame dat net vóór het
        // wapenen kwam tussen wal en schip.
        if d.peek.head_pending().1 {
            self.ctrl.set_rx_door(0);
            d.bell.set();
            return false;
        }
        true
    }

    /// `CtrlIdle` en `CtrlWakes`: de idle-tijd van alle cores van de app
    /// (deze plus [`SECONDARY_IDLE`]), de wekken van deze.
    fn publish(&self) {
        let idle = self.idle_ticks.wrapping_add(secondary_idle());
        self.ctrl.publish_idle(idle, self.wakes);
    }

    /// Ontwapent de deurbel na de slaap en belt als er iets ligt.
    fn disarm(&self, d: RxDoor) {
        self.ctrl.set_rx_door(0);
        if d.peek.head_pending().1 {
            d.bell.set();
        }
    }

    /// WFE's tot er werk is of tot `deadline` (tellerstand). Werk is een
    /// klare taak, RX in de ring (de deurbel: de kick van de kern is een
    /// SEV), of een kern die de core nu gedeeld of in yield-modus wil.
    ///
    /// Na elke WFE kijken, want een snelle terugkeer kan de échte bel zijn
    /// (tot 04-09 slikten we die weg: 6% van de system calls kostte 1 ms in
    /// plaats van 20 µs). Maar een wek zónder werk (de tik van de
    /// event-stream, een SEV die voor een ander was) gaat meteen de volgende
    /// WFE in, niet terug naar de executor: Linux' `do_idle`. Tot 30-09 was
    /// elke echte WFE een ronde, en maakte een stille app-core 1.011 (Pi 4)
    /// tot 3.003 (Radxa) rondes per seconde van 5 tot 10 µs. Tussen twee
    /// WFE's staan alleen loads en cache-onderhoud, geen exclusive die het
    /// event-register weer vult (Altra 18-07: 4,7M wakes/s).
    pub fn wfe_sleep(&mut self, deadline: u64, ready: &dyn Fn() -> bool) -> u64 {
        let (ctrl, door) = (&self.ctrl, self.door.get().get());
        let woke = || {
            ready()
                || door.is_some_and(|d| d.peek.head_pending().1)
                || ctrl.is_shared()
                || ctrl.is_yield_mode()
        };
        // `CtrlIdle` loopt mee met elke WFE (zie `wfe_until`).
        let base = self.idle_ticks;
        let progress = |s: u64| {
            ctrl.set(
                crate::contract::CTRL_IDLE,
                base.wrapping_add(s).wrapping_add(secondary_idle()),
            );
        };
        wfe_until(&mut self.idle, deadline, &woke, &progress)
    }

    /// De slaap zelf, zonder deurbel: yield of WFE.
    fn nap(&mut self, now: u64, until: Option<u64>, ready: &dyn Fn() -> bool) -> u64 {
        // De laatste toets. Een wek hierna is een SEV en die laat de WFE
        // meteen terugkeren.
        if ready() {
            return 0;
        }
        let deadline = clock::wake_at(now, until, self.idle.counter(), self.idle.counter_hz());
        if self.ctrl.is_shared() || self.ctrl.is_yield_mode() {
            // Eén yield per idle-ronde: de switcher doet zelf de slaap en de
            // rotatie, en de wektijd houdt twee wachtende buren uit een
            // pingpong.
            return self.idle.hvc_yield(deadline);
        }
        if deadline == 0 {
            return 0; // de deadline is al voorbij
        }
        self.wfe_sleep(deadline, ready)
    }
}

impl<I: Idle> Sleeper for AppSleeper<I> {
    fn sleep(&mut self, now: u64, until: Option<u64>, ready: &dyn Fn() -> bool) {
        self.wakes = self.wakes.wrapping_add(1);
        let door = self.door.get().get();
        if let Some(d) = door
            && !self.arm(d)
        {
            self.publish();
            return;
        }
        let slept = self.nap(now, until, ready);
        self.idle_ticks = self.idle_ticks.wrapping_add(slept);
        self.publish();
        if let Some(d) = door {
            self.disarm(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{CTRL_IDLE_MODE, CTRL_RX_DOOR, CTRL_SHARED, CTRL_WAKES, IDLE_YIELD};
    use crate::ctrl::tests::Page;
    use crate::ring::tests::Backing;
    use crate::ring::{Kind, Writer};

    /// Een nep-core: telt de instructies en laat de teller lopen.
    #[derive(Default)]
    struct Fake {
        now: Cell<u64>,
        wfes: u32,
        yields: Vec<u64>,
        per_wfe: u64,
    }

    impl Idle for Fake {
        fn counter(&self) -> u64 {
            self.now.get()
        }
        fn counter_hz(&self) -> u64 {
            1_000_000_000
        }
        fn wfe(&mut self) -> u64 {
            self.wfes += 1;
            self.now.set(self.now.get() + self.per_wfe);
            self.per_wfe
        }
        fn hvc_yield(&mut self, deadline: u64) -> u64 {
            self.yields.push(deadline);
            500
        }
    }

    fn fake(per_wfe: u64) -> Fake {
        Fake {
            per_wfe,
            ..Fake::default()
        }
    }

    #[test]
    fn ready_work_means_no_sleep() {
        let p = Page::new();
        let mut s = AppSleeper::with(p.ctrl(), fake(10_000));
        s.sleep(0, Some(1_000_000), &|| true);
        assert_eq!(s.idle().wfes, 0);
        assert_eq!(p.word(CTRL_WAKES), 1);
    }

    /// Een wek zonder werk (de event-stream, een vreemde SEV) is geen
    /// ronde: de slaper WFE't door tot de deadline, en telt één wek.
    #[test]
    fn a_wake_without_work_sleeps_on_to_the_deadline() {
        let p = Page::new();
        // 1 GHz, een event-stream-tik per 1,048 ms (de M4-keuze), een
        // deadline over 50 ms (de heartbeat): 48 WFE's, één wek.
        let mut s = AppSleeper::with(p.ctrl(), fake(1_048_576));
        s.sleep(0, Some(50_000_000), &|| false);
        assert_eq!(s.idle().wfes, 48);
        assert_eq!(p.word(CTRL_WAKES), 1);
        assert_eq!(s.idle_ticks(), 48 * 1_048_576);
        // Werk na de derde wek: daar houdt hij op.
        let mut s = AppSleeper::with(p.ctrl(), fake(1_000));
        let n = Cell::new(0u32);
        s.sleep(0, Some(50_000_000), &|| {
            n.set(n.get() + 1);
            n.get() > 4 // de toets van `nap` en drie na een WFE
        });
        assert_eq!(s.idle().wfes, 3);
    }

    /// Wordt de core gedeeld terwijl hij in WFE ligt, dan houdt de lus op
    /// en yieldt de volgende ronde: de buur wacht niet tot de deadline.
    #[test]
    fn a_core_that_becomes_shared_stops_the_wfe_loop() {
        let p = Page::new();
        let ctrl = p.ctrl();
        let mut s = AppSleeper::with(ctrl, fake(1_000));
        let n = Cell::new(0u32);
        s.sleep(0, Some(50_000_000), &|| {
            n.set(n.get() + 1);
            if n.get() == 3 {
                // De kern plaatst een buur op deze core.
                dev::write64(ctrl.addr(CTRL_SHARED), 1);
            }
            false
        });
        assert_eq!(s.idle().wfes, 1);
    }

    #[test]
    fn shared_core_yields_with_the_deadline() {
        let mut p = Page::new();
        p.put(CTRL_SHARED, 1);
        let mut s = AppSleeper::with(p.ctrl(), fake(0));
        s.idle.now.set(1_000);
        s.sleep(0, Some(2_000_000), &|| false);
        assert_eq!(s.idle().yields, [2_001_000]);
        assert_eq!(s.idle().wfes, 0);
        // Yield-modus van het board: dezelfde weg, ook zonder buurman.
        let mut p = Page::new();
        p.put(CTRL_IDLE_MODE, IDLE_YIELD);
        let mut s = AppSleeper::with(p.ctrl(), fake(0));
        s.sleep(0, None, &|| false);
        assert_eq!(s.idle().yields.len(), 1);
    }

    #[test]
    fn doorbell_arms_before_and_disarms_after_the_sleep() {
        static BELL: Signal = Signal::new();
        static DOOR: DoorSlot = Local::new(Cell::new(None));
        let b = Backing::new(4096);
        let mut switch = Writer::open(b.pa(), 4096).unwrap();
        DOOR.get().set(Some(RxDoor {
            peek: Peek::new(b.pa(), 4096),
            bell: &BELL,
        }));

        // Leeg: gewapend met head 0 tijdens de slaap, ontwapend erna.
        let p = Page::new();
        struct Watch<'a>(&'a Page, Cell<u64>);
        let w = Watch(&p, Cell::new(0));
        let mut s = AppSleeper::with(p.ctrl(), fake(5_000)).with_door(&DOOR);
        s.sleep(0, Some(1_000_000), &|| {
            w.1.set(w.0.word(CTRL_RX_DOOR));
            false
        });
        assert_eq!(w.1.get(), RX_ARMED); // gewapend op head 0
        assert_eq!(p.word(CTRL_RX_DOOR), 0);
        assert!(!BELL.take());

        // Er ligt een frame: bellen en niet slapen.
        switch.write(Kind::FRAME, &[1; 60]).unwrap();
        let mut s = AppSleeper::with(p.ctrl(), fake(5_000)).with_door(&DOOR);
        s.sleep(0, Some(1_000_000), &|| false);
        assert!(BELL.take());
        assert_eq!(s.idle().wfes, 0);
        assert_eq!(p.word(CTRL_RX_DOOR), 0);
    }
}
