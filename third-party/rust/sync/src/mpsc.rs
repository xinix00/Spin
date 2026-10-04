//! Veel producers, één consument: [`Mailbox`], de brievenbus van een actor.
//!
//! Het algoritme is Vyukovs begrensde MPMC-rij: elk slot draagt een
//! volgnummer, en een producer wint een slot met één CAS op de
//! enqueue-positie. Geen slot, geen wachtrij-in-de-wachtrij. Wij gebruiken
//! hem met één consument (de actor), en die krijgt een waker.

use crate::Full;
use crate::waker::AtomicWaker;
use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::{
    AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use core::task::{Context, Poll};

struct Slot<T> {
    seq: AtomicUsize,
    val: UnsafeCell<MaybeUninit<T>>,
}

/// Een rij met vaste capaciteit `N` waar elke taak (en elke core) in mag
/// schrijven en één taak uit leest.
///
/// # Invariants
///
/// Voor slot `i` met positie `pos` (`pos % N == i`): `seq == pos` betekent
/// leeg en beschikbaar voor de producer die `pos` wint; `seq == pos + 1`
/// betekent gevuld en beschikbaar voor de consument die `pos` wint; de
/// consument zet `seq` daarna op `pos + N` voor de volgende ronde. Er staan
/// nooit meer dan `N` elementen in de rij (`enq - deq <= N`): de producer
/// toetst dat expliciet, want bij `N == 1` is "gevuld voor `pos`" hetzelfde
/// getal als "leeg voor `pos + 1`" (zie [`Mailbox::try_send`]).
pub struct Mailbox<T, const N: usize> {
    slots: [Slot<T>; N],
    enq: AtomicUsize,
    deq: AtomicUsize,
    rx: AtomicWaker,
}

// SAFETY: elk slot wordt door precies één partij tegelijk aangeraakt: wie
// de CAS op `enq` of `deq` wint, en de `seq`-atomics ordenen schrijven vóór
// lezen. Een `T` reist tussen taken en cores, vandaar `T: Send`.
unsafe impl<T: Send, const N: usize> Sync for Mailbox<T, N> {}
// SAFETY: zie `Sync`.
unsafe impl<T: Send, const N: usize> Send for Mailbox<T, N> {}

impl<T, const N: usize> Mailbox<T, N> {
    /// Een lege brievenbus.
    #[must_use]
    pub const fn new() -> Self {
        const { assert!(N > 0, "een brievenbus zonder plaats is geen brievenbus") };
        let mut slots = [const {
            Slot {
                seq: AtomicUsize::new(0),
                val: UnsafeCell::new(MaybeUninit::uninit()),
            }
        }; N];
        let mut i = 0;
        while i < N {
            slots[i].seq = AtomicUsize::new(i);
            i += 1;
        }
        Self {
            slots,
            enq: AtomicUsize::new(0),
            deq: AtomicUsize::new(0),
            rx: AtomicWaker::new(),
        }
    }

    /// Zet `v` in de rij, of geeft hem terug als de rij vol is. Vanaf elke
    /// taak en elke core; nooit vanuit een ISR, want de consument mag geen
    /// `T` van een ISR erven.
    ///
    /// De capaciteitstoets vóór de claim is er voor `N == 1` (29-09, de
    /// kern-flip): daar zegt het volgnummer van een gevuld slot (`pos + 1`)
    /// ook "leeg" tegen de volgende producer. Zonder de toets won een tweede
    /// `try_send` zonder `try_recv` ertussen het slot opnieuw (het eerste
    /// element weg, `seq` op `pos + 2`), en bleef elke `try_recv` daarna
    /// voor altijd draaien. Zo hing de OS-core van QEMU in een `try_take`
    /// van de `Ack` van de switch, na twee geadopteerde `Attach`-en op één
    /// `Ack`. Een verouderde `deq` maakt de toets alleen strenger: vol
    /// zeggen waar net plaats kwam, nooit andersom.
    pub fn try_send(&self, v: T) -> Result<(), Full<T>> {
        let mut pos = self.enq.load(Relaxed);
        loop {
            let slot = &self.slots[pos % N];
            let seq = slot.seq.load(Acquire);
            let dif = seq.wrapping_sub(pos) as isize;
            if dif == 0 {
                if pos.wrapping_sub(self.deq.load(Acquire)) >= N {
                    return Err(Full(v));
                }
                if self
                    .enq
                    .compare_exchange_weak(pos, pos.wrapping_add(1), Relaxed, Relaxed)
                    .is_ok()
                {
                    // SAFETY: wij wonnen positie `pos`, dus dit slot is van
                    // ons tot we `seq` op `pos + 1` zetten (invariant).
                    unsafe { (*slot.val.get()).write(v) };
                    slot.seq.store(pos.wrapping_add(1), Release);
                    self.rx.wake();
                    return Ok(());
                }
            } else if dif < 0 {
                return Err(Full(v));
            } else {
                pos = self.enq.load(Relaxed);
            }
        }
    }

    /// Haalt het volgende element eruit, als het er is. Alleen de eigenaar.
    pub fn try_recv(&self) -> Option<T> {
        let mut pos = self.deq.load(Relaxed);
        loop {
            let slot = &self.slots[pos % N];
            let seq = slot.seq.load(Acquire);
            let dif = seq.wrapping_sub(pos.wrapping_add(1)) as isize;
            if dif == 0 {
                if self
                    .deq
                    .compare_exchange_weak(pos, pos.wrapping_add(1), Relaxed, Relaxed)
                    .is_ok()
                {
                    // SAFETY: `seq == pos + 1`: het slot is gevuld en wij
                    // wonnen positie `pos`, dus we nemen het precies één keer.
                    let v = unsafe { (*slot.val.get()).assume_init_read() };
                    slot.seq.store(pos.wrapping_add(N), Release);
                    return Some(v);
                }
            } else if dif < 0 {
                return None;
            } else {
                pos = self.deq.load(Relaxed);
            }
        }
    }

    /// Wacht op het volgende element. Eén consument: een tweede wachter
    /// verdringt de waker van de eerste.
    pub fn recv(&self) -> RecvFut<'_, T, N> {
        RecvFut { mb: self }
    }

    /// Het aantal elementen dat (ongeveer) klaarligt: een momentopname.
    #[must_use]
    pub fn len(&self) -> usize {
        self.enq
            .load(Relaxed)
            .wrapping_sub(self.deq.load(Relaxed))
            .min(N)
    }

    /// Ligt er (op dit moment) niets?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T, const N: usize> Default for Mailbox<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for Mailbox<T, N> {
    fn drop(&mut self) {
        while self.try_recv().is_some() {}
    }
}

/// De future van [`Mailbox::recv`].
#[must_use = "een future doet niets tot hij gepolld wordt"]
pub struct RecvFut<'a, T, const N: usize> {
    mb: &'a Mailbox<T, N>,
}

impl<T, const N: usize> Future for RecvFut<'_, T, N> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        if let Some(v) = self.mb.try_recv() {
            return Poll::Ready(v);
        }
        self.mb.rx.register(cx.waker());
        match self.mb.try_recv() {
            Some(v) => Poll::Ready(v),
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{poll_once, waker};
    use std::sync::atomic::Ordering::SeqCst;

    #[test]
    fn fifo_and_full() {
        let mb: Mailbox<u32, 2> = Mailbox::new();
        assert_eq!(mb.try_send(1), Ok(()));
        assert_eq!(mb.try_send(2), Ok(()));
        assert_eq!(mb.try_send(3), Err(Full(3)));
        assert_eq!(mb.try_recv(), Some(1));
        assert_eq!(mb.try_send(3), Ok(()));
        assert_eq!(mb.try_recv(), Some(2));
        assert_eq!(mb.try_recv(), Some(3));
        assert_eq!(mb.try_recv(), None);
    }

    #[test]
    fn one_place_holds_one_and_never_spins() {
        // Een brievenbus van één plaats (tot 04-10 de `Ack` van de switch):
        // twee resultaten zonder `try_recv` ertussen. Het tweede is vol, het
        // eerste blijft, en een lege rij zegt daarna gewoon `None` (vóór
        // 29-09 draaide die `try_recv` voor altijd).
        let mb: Mailbox<u32, 1> = Mailbox::new();
        assert_eq!(mb.try_send(1), Ok(()));
        assert_eq!(mb.try_send(2), Err(Full(2)));
        assert_eq!(mb.try_recv(), Some(1));
        assert_eq!(mb.try_recv(), None);
        for i in 0..5 {
            assert_eq!(mb.try_send(i), Ok(()));
            assert_eq!(mb.try_send(99), Err(Full(99)));
            assert_eq!(mb.try_recv(), Some(i));
        }
        assert_eq!(mb.try_recv(), None);
    }

    #[test]
    fn recv_is_woken() {
        let mb: Mailbox<u32, 1> = Mailbox::new();
        let (c, w) = waker();
        let mut f = core::pin::pin!(mb.recv());
        assert_eq!(poll_once(&mut f, &w), Poll::Pending);
        assert_eq!(mb.try_send(9), Ok(()));
        assert_eq!(c.0.load(SeqCst), 1);
        assert_eq!(poll_once(&mut f, &w), Poll::Ready(9));
    }

    #[test]
    fn many_producers_one_consumer() {
        static MB: Mailbox<u64, 64> = Mailbox::new();
        let producers: Vec<_> = (0..4)
            .map(|p| {
                std::thread::spawn(move || {
                    let mut sent = 0u64;
                    let mut i = 0u64;
                    while i < 1000 {
                        if MB.try_send(p * 1000 + i).is_ok() {
                            sent += i;
                            i += 1;
                        } else {
                            std::thread::yield_now();
                        }
                    }
                    sent
                })
            })
            .collect();
        let mut got = 0u64;
        let mut n = 0;
        while n < 4000 {
            if let Some(v) = MB.try_recv() {
                got += v % 1000;
                n += 1;
            } else {
                std::thread::yield_now();
            }
        }
        let sent: u64 = producers.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(got, sent);
        assert!(MB.is_empty());
    }
}
