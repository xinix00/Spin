//! Verzoek en antwoord: [`Oneshot`], en [`call`] dat er een verzoek mee
//! stuurt en op het antwoord wacht.
//!
//! Het patroon "zend, wacht op een bel, neem het antwoord" stond zes keer
//! met de hand in de kern (een [`Signal`](crate::Signal) plus een plaats per
//! soort antwoord). De vorm is die van embassy's `Signal<T>` en Linux'
//! `struct completion`: één plaats voor één waarde, een tweede `put` vervangt
//! de eerste (een verouderd antwoord wint nooit van een vers), en één
//! wachter. Wie een verzoek stuurt, leegt de plaats eerst ([`call`]): een
//! antwoord van een verzoek waarvan de wachter wegging (een `select` met een
//! termijn), komt zo nooit bij het volgende verzoek terecht.

use crate::Full;
use crate::mpsc::Mailbox;
use crate::waker::AtomicWaker;
use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::{
    AtomicU8,
    Ordering::{Acquire, Relaxed, Release},
};
use core::task::{Context, Poll};

const EMPTY: u8 = 0;
const FULL: u8 = 1;
/// Iemand schrijft of leest de plaats; alleen over cores heen te zien,
/// want `put` en [`Oneshot::try_take`] geven nooit af.
const BUSY: u8 = 2;

/// Eén antwoordplek: de actor legt er een waarde in ([`put`](Self::put)),
/// de aanroeper wacht erop ([`recv`](Self::recv)).
///
/// Van elke taak en elke core; nooit vanuit een ISR (de waarde is een `T`
/// die een ISR niet hoort te bouwen, en een ISR die een `try_take` op
/// dezelfde core onderbreekt, zou op [`BUSY`] wachten).
///
/// # Invariants
///
/// `val` is geïnitialiseerd precies als `state == FULL`, en alleen wie
/// `state` op [`BUSY`] zette, raakt `val` aan.
pub struct Oneshot<T> {
    state: AtomicU8,
    val: UnsafeCell<MaybeUninit<T>>,
    waker: AtomicWaker,
}

// SAFETY: `val` wordt alleen aangeraakt door wie `state` op BUSY zette
// (invariant), en de Acquire/Release op `state` ordenen schrijven vóór
// lezen. Een `T` reist tussen taken en cores, vandaar `T: Send`.
unsafe impl<T: Send> Sync for Oneshot<T> {}
// SAFETY: zie `Sync`.
unsafe impl<T: Send> Send for Oneshot<T> {}

impl<T> Oneshot<T> {
    /// Een lege plaats.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            val: UnsafeCell::new(MaybeUninit::uninit()),
            waker: AtomicWaker::new(),
        }
    }

    /// Zet de plaats op [`BUSY`] vanuit de staat die hij heeft, en geeft
    /// die staat: [`EMPTY`] of [`FULL`].
    fn lock(&self) -> u8 {
        loop {
            let s = self.state.load(Relaxed);
            if s != BUSY
                && self
                    .state
                    .compare_exchange(s, BUSY, Acquire, Relaxed)
                    .is_ok()
            {
                return s;
            }
            core::hint::spin_loop();
        }
    }

    /// Legt `v` op de plaats en wekt de wachter. Een waarde die er nog lag,
    /// gaat weg: het nieuwste antwoord wint.
    pub fn put(&self, v: T) {
        let was = self.lock();
        // SAFETY: de plaats is BUSY en van ons; was hij FULL, dan is de oude
        // waarde er (invariant) en gaat ze één keer weg.
        unsafe {
            let val = &mut *self.val.get();
            if was == FULL {
                val.assume_init_drop();
            }
            val.write(v);
        }
        // INVARIANT: `val` is geschreven vóór FULL zichtbaar wordt.
        self.state.store(FULL, Release);
        self.waker.wake();
    }

    /// Neemt de waarde als die er ligt, zonder te wachten.
    pub fn try_take(&self) -> Option<T> {
        if self.state.load(Relaxed) == EMPTY {
            return None;
        }
        let was = self.lock();
        // SAFETY: de plaats is BUSY en van ons; was hij FULL, dan is de
        // waarde er en nemen we hem precies één keer.
        let v = (was == FULL).then(|| unsafe { (*self.val.get()).assume_init_read() });
        // INVARIANT: de waarde is gelezen (of was er niet); de plaats is leeg.
        self.state.store(EMPTY, Release);
        v
    }

    /// Wacht op de waarde en neemt hem. Eén wachter: een tweede verdringt
    /// de waker van de eerste.
    pub fn recv(&self) -> Recv<'_, T> {
        Recv(self)
    }
}

impl<T> Default for Oneshot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for Oneshot<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == FULL {
            // SAFETY: FULL en `&mut self`: de waarde is er en niemand
            // anders raakt hem aan.
            unsafe { self.val.get_mut().assume_init_drop() };
        }
    }
}

/// De future van [`Oneshot::recv`].
#[must_use = "een future doet niets tot hij gepolld wordt"]
pub struct Recv<'a, T>(&'a Oneshot<T>);

impl<T> Future for Recv<'_, T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let o = self.0;
        if let Some(v) = o.try_take() {
            return Poll::Ready(v);
        }
        o.waker.register(cx.waker());
        // Nog een keer kijken: een `put` tussen de eerste toets en de
        // registratie is anders een verloren wek.
        match o.try_take() {
            Some(v) => Poll::Ready(v),
            None => Poll::Pending,
        }
    }
}

/// Stuurt `msg` (dat `reply` draagt) naar `inbox` en wacht op het antwoord
/// op `reply`. Eerst gaat een oud antwoord weg. Zit de brievenbus vol, dan
/// komt het bericht meteen terug.
pub async fn call<M, T, const N: usize>(
    inbox: &Mailbox<M, N>,
    reply: &Oneshot<T>,
    msg: M,
) -> Result<T, Full<M>> {
    let _ = reply.try_take();
    inbox.try_send(msg)?;
    Ok(reply.recv().await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{poll_once, waker};
    use std::rc::Rc;
    use std::sync::atomic::Ordering::SeqCst;

    #[test]
    fn put_then_take_and_the_newest_wins() {
        let o: Oneshot<u32> = Oneshot::new();
        assert_eq!(o.try_take(), None);
        o.put(1);
        o.put(2);
        assert_eq!(o.try_take(), Some(2));
        assert_eq!(o.try_take(), None);
    }

    #[test]
    fn recv_is_woken_by_put() {
        let o: Oneshot<u32> = Oneshot::new();
        let (c, w) = waker();
        let mut f = core::pin::pin!(o.recv());
        assert_eq!(poll_once(&mut f, &w), Poll::Pending);
        o.put(7);
        assert_eq!(c.0.load(SeqCst), 1);
        assert_eq!(poll_once(&mut f, &w), Poll::Ready(7));
    }

    #[test]
    fn a_replaced_or_left_value_is_dropped_once() {
        let v = Rc::new(());
        {
            let o = Oneshot::new();
            o.put(v.clone());
            o.put(v.clone());
            assert_eq!(Rc::strong_count(&v), 2, "de eerste ging weg bij de tweede");
        }
        assert_eq!(Rc::strong_count(&v), 1, "de laatste ging weg met de plaats");
    }

    #[test]
    fn call_drops_a_stale_answer_and_returns_the_message_when_full() {
        let inbox: Mailbox<u32, 1> = Mailbox::new();
        let reply: Oneshot<u32> = Oneshot::new();
        reply.put(99); // het antwoord van een wachter die wegging
        let (_, w) = waker();
        {
            let mut f = core::pin::pin!(call(&inbox, &reply, 5));
            assert_eq!(poll_once(&mut f, &w), Poll::Pending);
            assert_eq!(inbox.try_recv(), Some(5));
            reply.put(6);
            assert_eq!(poll_once(&mut f, &w), Poll::Ready(Ok(6)));
        }
        assert_eq!(inbox.try_send(1), Ok(()));
        let mut f = core::pin::pin!(call(&inbox, &reply, 2));
        assert_eq!(poll_once(&mut f, &w), Poll::Ready(Err(Full(2))));
    }

    #[test]
    fn answers_cross_cores() {
        static O: Oneshot<u64> = Oneshot::new();
        let t = std::thread::spawn(|| {
            for i in 1..=1000u64 {
                O.put(i);
            }
        });
        let mut last = 0;
        while last < 1000 {
            if let Some(v) = O.try_take() {
                assert!(v > last, "nooit een oudere waarde na een nieuwere");
                last = v;
            }
        }
        t.join().unwrap();
    }
}
