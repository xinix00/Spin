//! De concurrency-bouwstenen van HopOS (handboek §2).
//!
//! Alles hier werkt over `core::task`: een executor is niet nodig om deze
//! crate te gebruiken of te testen, en elke bouwsteen is klein genoeg om in
//! één keer te lezen.
//!
//! - [`Signal`]: de bel. Level-triggered en samengevoegd: tien bellen
//!   tegelijk zijn er één. De tegenhanger van Go's `chan struct{}` met
//!   capaciteit 1, en ISR-veilig aan de `set`-kant.
//! - [`Stop`]: een bel die nooit meer uitgaat, met meerdere wachters.
//! - [`spsc::Channel`]: één producer, één consument, vaste capaciteit.
//! - [`mpsc::Mailbox`]: veel producers, één consument: de brievenbus van
//!   een actor.
//! - [`Local`]: een static die alleen de executor van één core aanraakt.
//! - [`select`], [`yield_now`]: de twee lus-hulpjes uit de Go-vertaling.
//!
//! Wat hier NIET staat: een mutex. Zie het handboek §1 en §3.

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

pub mod local;
pub mod mpsc;
pub mod select;
pub mod signal;
pub mod spsc;
pub mod stop;
pub mod waker;

pub use local::{Local, LocalCell};
pub use select::{Either, select};
pub use signal::Signal;
pub use stop::Stop;
pub use waker::AtomicWaker;

/// De verzameling is vol; het element komt terug naar de zender.
#[derive(Debug, PartialEq, Eq)]
pub struct Full<T>(pub T);

/// Geeft de rest van de ronde aan de andere taken en komt daarna terug.
///
/// De tegenhanger van `runtime.Gosched()`: gebruiken in een lange lus die
/// anders de pompen verhongert (het `Scrub`-voorbeeld uit de Go-kern).
pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

/// De future van [`yield_now`].
#[derive(Debug)]
pub struct YieldNow {
    yielded: bool,
}

impl core::future::Future for YieldNow {
    type Output = ();

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<()> {
        if self.yielded {
            return core::task::Poll::Ready(());
        }
        self.yielded = true;
        cx.waker().wake_by_ref();
        core::task::Poll::Pending
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Een handmatige waker voor de tests: telt hoe vaak hij gewekt is.
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    pub(crate) struct Counter(pub(crate) AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub(crate) fn waker() -> (Arc<Counter>, Waker) {
        let c = Arc::new(Counter(AtomicUsize::new(0)));
        (c.clone(), Waker::from(c))
    }

    pub(crate) fn poll_once<F: Future>(f: &mut Pin<&mut F>, w: &Waker) -> Poll<F::Output> {
        f.as_mut().poll(&mut Context::from_waker(w))
    }
}
