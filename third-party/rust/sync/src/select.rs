//! [`select`]: twee futures, één wint.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Welke van de twee klaar was.
#[derive(Debug, PartialEq, Eq)]
pub enum Either<A, B> {
    /// De eerste.
    Left(A),
    /// De tweede.
    Right(B),
}

/// Pollt `a` en dan `b`; de eerste die klaar is wint, de ander wordt
/// gedropt. De vertaling van Go's `select` met twee takken; `a` heeft
/// voorrang als beide klaar zijn, dus zet de stopbel links.
pub fn select<A: Future, B: Future>(a: A, b: B) -> Select<A, B> {
    Select { a, b }
}

/// De future van [`select`].
#[must_use = "een future doet niets tot hij gepolld wordt"]
pub struct Select<A, B> {
    a: A,
    b: B,
}

impl<A: Future, B: Future> Future for Select<A, B> {
    type Output = Either<A::Output, B::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structurele pinning: `a` en `b` worden nooit uit `self`
        // verplaatst en `Select` heeft geen `Drop`-impl, dus een gepinde
        // `Select` pint zijn velden.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: zie hierboven.
        let a = unsafe { Pin::new_unchecked(&mut this.a) };
        if let Poll::Ready(v) = a.poll(cx) {
            return Poll::Ready(Either::Left(v));
        }
        // SAFETY: zie hierboven.
        let b = unsafe { Pin::new_unchecked(&mut this.b) };
        if let Poll::Ready(v) = b.poll(cx) {
            return Poll::Ready(Either::Right(v));
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Signal;
    use crate::testing::{poll_once, waker};

    #[test]
    fn left_has_priority_and_right_wins_alone() {
        let stop = Signal::new();
        let work = Signal::new();
        let (_, w) = waker();
        let mut f = core::pin::pin!(select(stop.wait(), work.wait()));
        assert_eq!(poll_once(&mut f, &w), Poll::Pending);
        work.set();
        assert_eq!(poll_once(&mut f, &w), Poll::Ready(Either::Right(())));
        stop.set();
        work.set();
        let mut g = core::pin::pin!(select(stop.wait(), work.wait()));
        assert_eq!(poll_once(&mut g, &w), Poll::Ready(Either::Left(())));
        assert!(work.is_set()); // de verliezer is niet aangeraakt
    }
}
