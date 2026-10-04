//! [`Pool`]: een vaste set futures van één soort in één taak.
//!
//! De `FuturesUnordered` van één core, maar dan met `N` vaste plaatsen en
//! zonder heap: een actor die meerdere verzoeken tegelijk in de lucht wil
//! hebben (de hopfs-actor met een call per app), zet elk verzoek als future
//! in een vrije plaats en haalt de uitkomsten op in de volgorde waarin ze
//! klaar zijn. Alles blijft in de ene taak, dus de staat van de actor
//! heeft nog steeds één eigenaar (handboek §1); de futures lenen hem alleen
//! binnen één poll.
//!
//! Elke [`poll_next`](Pool::poll_next) pollt de lopende futures (hoogstens
//! `N`, voor de hopfs-actor 16) met de waker van de taak: wie wekt, wekt de
//! hele taak, en de pool kijkt dan bij iedereen. Bij zestien plaatsen is
//! dat goedkoper dan een waker per plaats.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Hoogstens `N` futures van type `F` tegelijk, in vaste plaatsen.
///
/// # Invariants
///
/// Een future in een plaats wordt nooit verplaatst: hij komt erin via
/// [`push`](Pool::push) op een gepinde pool en gaat eruit door hem ter
/// plekke te droppen (`*slot = None`). Daarmee pint een gepinde `Pool` zijn
/// futures (structurele pinning, zoals `Select`).
pub struct Pool<F, const N: usize> {
    slots: [Option<F>; N],
    /// Waar de volgende ronde begint: zo komt niet steeds dezelfde plaats
    /// als eerste aan de beurt.
    next: usize,
}

impl<F: Future, const N: usize> Pool<F, N> {
    /// Een lege pool.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; N],
            next: 0,
        }
    }

    /// Het aantal lopende futures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Loopt er niets?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// Zijn alle plaatsen bezet?
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.slots.iter().all(Option::is_some)
    }

    /// Zet `f` in een vrije plaats; vol geeft hem terug. De future loopt
    /// pas bij de volgende [`poll_next`](Self::poll_next).
    pub fn push(self: Pin<&mut Self>, f: F) -> Result<(), crate::Full<F>> {
        // SAFETY: structurele pinning (zie de invariant): we schrijven
        // alleen in een lege plaats, en verplaatsen geen lopende future.
        let this = unsafe { self.get_unchecked_mut() };
        match this.slots.iter_mut().find(|s| s.is_none()) {
            Some(s) => {
                *s = Some(f);
                Ok(())
            }
            None => Err(crate::Full(f)),
        }
    }

    /// Pollt de lopende futures en geeft de eerste uitkomst die klaar is;
    /// de rest blijft lopen. `Ready(None)` als de pool leeg is.
    pub fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<F::Output>> {
        // SAFETY: structurele pinning (zie de invariant): een plaats wordt
        // alleen ter plekke gepolld of gedropt.
        let this = unsafe { self.get_unchecked_mut() };
        let mut any = false;
        for k in 0..N {
            let i = (this.next + k) % N;
            let Some(slot) = this.slots.get_mut(i) else {
                continue;
            };
            let Some(f) = slot.as_mut() else { continue };
            any = true;
            // SAFETY: de future staat vast in zijn plaats (invariant).
            if let Poll::Ready(v) = unsafe { Pin::new_unchecked(f) }.poll(cx) {
                // Ter plekke droppen, niet verplaatsen.
                *slot = None;
                this.next = (i + 1) % N;
                return Poll::Ready(Some(v));
            }
        }
        if any {
            Poll::Pending
        } else {
            Poll::Ready(None)
        }
    }
}

impl<F: Future, const N: usize> Default for Pool<F, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Signal;
    use crate::testing::waker;

    #[test]
    fn results_come_in_the_order_they_finish_and_free_places_are_reused() {
        let bells = [Signal::new(), Signal::new(), Signal::new()];
        let (_, w) = waker();
        let mut cx = Context::from_waker(&w);
        let mut pool = core::pin::pin!(Pool::<_, 2>::new());
        let job = |i: usize| {
            let b = &bells[i];
            async move {
                b.wait().await;
                i
            }
        };
        assert!(pool.as_mut().push(job(0)).is_ok());
        assert!(pool.as_mut().push(job(1)).is_ok());
        assert!(pool.is_full());
        assert!(pool.as_mut().push(job(2)).is_err(), "vol");
        assert_eq!(pool.as_mut().poll_next(&mut cx), Poll::Pending);
        bells[1].set();
        assert_eq!(pool.as_mut().poll_next(&mut cx), Poll::Ready(Some(1)));
        assert_eq!(pool.len(), 1);
        assert!(pool.as_mut().push(job(2)).is_ok(), "de plaats is weer vrij");
        bells[2].set();
        bells[0].set();
        let mut got = [
            pool.as_mut().poll_next(&mut cx),
            pool.as_mut().poll_next(&mut cx),
        ];
        got.sort_by_key(|p| match p {
            Poll::Ready(Some(v)) => *v,
            _ => usize::MAX,
        });
        assert_eq!(got, [Poll::Ready(Some(0)), Poll::Ready(Some(2))]);
        assert_eq!(pool.as_mut().poll_next(&mut cx), Poll::Ready(None));
        assert!(pool.is_empty());
    }
}
