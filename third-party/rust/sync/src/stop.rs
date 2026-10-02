//! De stopbel: [`Stop`].

use crate::waker::WakerSet;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{
    AtomicBool,
    Ordering::{Acquire, Release},
};
use core::task::{Context, Poll};

/// Een signaal dat nooit meer uitgaat, met plaats voor `N` wachters.
///
/// De tegenhanger van `close(stop)` en `context.WithCancel`: een taak wacht
/// erop in zijn `select`, kinderen krijgen dezelfde `&Stop`. Meer dan `N`
/// gelijktijdige wachters blijft correct (ze wekken zichzelf per ronde) en
/// telt in [`overflows`](Self::overflows).
pub struct Stop<const N: usize = 4> {
    set: AtomicBool,
    waiters: WakerSet<N>,
}

impl<const N: usize> Stop<N> {
    /// Een stopbel die nog niet geluid heeft.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            set: AtomicBool::new(false),
            waiters: WakerSet::new(),
        }
    }

    /// Luidt de bel: elke wachter, nu en later, komt terug.
    pub fn set(&self) {
        self.set.store(true, Release);
        self.waiters.wake_all();
    }

    /// Heeft de bel geluid?
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.set.load(Acquire)
    }

    /// Wacht tot de bel luidt.
    pub fn wait(&self) -> StopWait<'_, N> {
        StopWait {
            stop: self,
            slot: None,
        }
    }

    /// Hoe vaak een wachter geen slot kreeg: de meetlat voor `N`.
    #[must_use]
    pub fn overflows(&self) -> u32 {
        self.waiters.overflows()
    }
}

impl<const N: usize> Default for Stop<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// De future van [`Stop::wait`].
#[must_use = "een future doet niets tot hij gepolld wordt"]
pub struct StopWait<'a, const N: usize> {
    stop: &'a Stop<N>,
    slot: Option<usize>,
}

impl<const N: usize> Future for StopWait<'_, N> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.stop.is_set() {
            return Poll::Ready(());
        }
        if !this.stop.waiters.register(&mut this.slot, cx.waker()) {
            cx.waker().wake_by_ref();
        }
        if this.stop.is_set() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl<const N: usize> Drop for StopWait<'_, N> {
    fn drop(&mut self) {
        self.stop.waiters.release(&mut self.slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{poll_once, waker};
    use std::sync::atomic::Ordering::SeqCst;

    #[test]
    fn all_waiters_come_back_and_stay_back() {
        let stop: Stop<2> = Stop::new();
        let (c1, w1) = waker();
        let (c2, w2) = waker();
        let mut f1 = core::pin::pin!(stop.wait());
        let mut f2 = core::pin::pin!(stop.wait());
        assert_eq!(poll_once(&mut f1, &w1), Poll::Pending);
        assert_eq!(poll_once(&mut f2, &w2), Poll::Pending);
        stop.set();
        assert_eq!((c1.0.load(SeqCst), c2.0.load(SeqCst)), (1, 1));
        assert_eq!(poll_once(&mut f1, &w1), Poll::Ready(()));
        assert_eq!(poll_once(&mut f2, &w2), Poll::Ready(()));
        let mut f3 = core::pin::pin!(stop.wait());
        assert_eq!(poll_once(&mut f3, &w1), Poll::Ready(()));
    }

    #[test]
    fn overflow_self_wakes_and_drop_frees_the_slot() {
        let stop: Stop<1> = Stop::new();
        let (_, w1) = waker();
        let (c2, w2) = waker();
        {
            let mut f1 = core::pin::pin!(stop.wait());
            assert_eq!(poll_once(&mut f1, &w1), Poll::Pending);
            let mut f2 = core::pin::pin!(stop.wait());
            assert_eq!(poll_once(&mut f2, &w2), Poll::Pending);
            assert_eq!(c2.0.load(SeqCst), 1); // zelf gewekt: geen slot
            assert_eq!(stop.overflows(), 1);
        } // beide wachters weg: het slot is vrij
        let mut f3 = core::pin::pin!(stop.wait());
        assert_eq!(poll_once(&mut f3, &w2), Poll::Pending);
        assert_eq!(c2.0.load(SeqCst), 1); // nu wél een slot: niet zelf gewekt
    }
}
