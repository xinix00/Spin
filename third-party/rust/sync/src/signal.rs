//! De bel: [`Signal`].

use crate::waker::AtomicWaker;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{
    AtomicBool,
    Ordering::{Acquire, Release},
};
use core::task::{Context, Poll};

/// Een level-triggered, samengevoegd signaal.
///
/// `set` mag uit een ISR of van een andere core komen; `wait` is een
/// future voor precies één wachter. Tien keer `set` vóór één `wait` is één
/// wek: het signaal zegt "kijk", niet "hoe vaak". Precies Go's
/// `chan struct{}` met capaciteit 1, en de `switchDoor` van de kern.
pub struct Signal {
    set: AtomicBool,
    waker: AtomicWaker,
}

impl Signal {
    /// Een signaal dat niet staat.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            set: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        }
    }

    /// Zet het signaal en wekt de wachter. ISR-veilig.
    pub fn set(&self) {
        self.set.store(true, Release);
        self.waker.wake();
    }

    /// Staat het signaal?
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.set.load(Acquire)
    }

    /// Neemt het signaal weg en zegt of het stond.
    pub fn take(&self) -> bool {
        self.set.swap(false, Acquire)
    }

    /// Wacht tot het signaal staat en neemt het dan weg.
    pub fn wait(&self) -> Wait<'_> {
        Wait(self)
    }
}

impl Default for Signal {
    fn default() -> Self {
        Self::new()
    }
}

/// De future van [`Signal::wait`].
#[must_use = "een future doet niets tot hij gepolld wordt"]
pub struct Wait<'a>(&'a Signal);

impl Future for Wait<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let s = self.0;
        if s.take() {
            return Poll::Ready(());
        }
        s.waker.register(cx.waker());
        // Nog een keer kijken: een `set` tussen de eerste toets en de
        // registratie is anders een verloren wek.
        if s.take() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{poll_once, waker};
    use std::sync::atomic::Ordering::SeqCst;

    #[test]
    fn set_before_wait_is_ready_at_once() {
        let s = Signal::new();
        s.set();
        s.set(); // samengevoegd
        let (_, w) = waker();
        let mut f = core::pin::pin!(s.wait());
        assert_eq!(poll_once(&mut f, &w), Poll::Ready(()));
        let mut g = core::pin::pin!(s.wait());
        assert_eq!(poll_once(&mut g, &w), Poll::Pending);
    }

    #[test]
    fn set_after_wait_wakes_and_resolves() {
        let s = Signal::new();
        let (c, w) = waker();
        let mut f = core::pin::pin!(s.wait());
        assert_eq!(poll_once(&mut f, &w), Poll::Pending);
        s.set();
        assert_eq!(c.0.load(SeqCst), 1);
        assert_eq!(poll_once(&mut f, &w), Poll::Ready(()));
        assert!(!s.is_set());
    }
}
