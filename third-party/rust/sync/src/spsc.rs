//! Eén producer, één consument: [`Channel`].

use crate::Full;
use crate::waker::AtomicWaker;
use core::cell::{Cell, UnsafeCell};
use core::future::Future;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::{
    AtomicBool, AtomicUsize,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use core::task::{Context, Poll};

/// Een rij met vaste capaciteit `N` tussen precies één [`Sender`] en één
/// [`Receiver`]. De tegenhanger van Go's gebufferde `chan T`, met het
/// verschil dat "vol" een keuze van de zender is: droppen (`try_send`) of
/// wachten (`send`).
///
/// # Invariants
///
/// - `head` en `tail` lopen monotoon op; `head - tail` is het aantal
///   elementen en is nooit groter dan `N`.
/// - De slots `tail..head` (modulo `N`) zijn geïnitialiseerd, de rest niet.
/// - Alleen de `Sender` schrijft `head` en de slots erachter; alleen de
///   `Receiver` schrijft `tail` en leest de slots ervoor.
pub struct Channel<T, const N: usize> {
    buf: [UnsafeCell<MaybeUninit<T>>; N],
    head: AtomicUsize,
    tail: AtomicUsize,
    rx: AtomicWaker,
    tx: AtomicWaker,
    split: AtomicBool,
}

// SAFETY: de invariant verdeelt de toegang tussen de twee helften; de
// atomics ordenen het schrijven van een slot (Release op `head`) vóór het
// lezen ervan (Acquire op `head`). Een `T` reist zo van de ene helft naar
// de andere, vandaar `T: Send`.
unsafe impl<T: Send, const N: usize> Sync for Channel<T, N> {}

impl<T, const N: usize> Channel<T, N> {
    /// Een leeg kanaal.
    #[must_use]
    pub const fn new() -> Self {
        const { assert!(N > 0, "een kanaal zonder plaats is geen kanaal") };
        Self {
            buf: [const { UnsafeCell::new(MaybeUninit::uninit()) }; N],
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            rx: AtomicWaker::new(),
            tx: AtomicWaker::new(),
            split: AtomicBool::new(false),
        }
    }

    /// Geeft de twee helften, precies één keer.
    pub fn split(&self) -> Option<(Sender<'_, T, N>, Receiver<'_, T, N>)> {
        if self.split.swap(true, AcqRel) {
            return None;
        }
        Some((
            Sender {
                ch: self,
                _one: PhantomData,
            },
            Receiver {
                ch: self,
                _one: PhantomData,
            },
        ))
    }

    /// Het aantal elementen dat klaarligt.
    #[must_use]
    pub fn len(&self) -> usize {
        self.head
            .load(Acquire)
            .wrapping_sub(self.tail.load(Acquire))
    }

    /// Ligt er niets?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn slot(&self, i: usize) -> *mut MaybeUninit<T> {
        self.buf[i % N].get()
    }
}

impl<T, const N: usize> Default for Channel<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for Channel<T, N> {
    fn drop(&mut self) {
        let head = *self.head.get_mut();
        let mut tail = *self.tail.get_mut();
        while tail != head {
            // SAFETY: slots `tail..head` zijn geïnitialiseerd (invariant) en
            // `&mut self` sluit elke andere toegang uit.
            unsafe { (*self.slot(tail)).assume_init_drop() };
            tail = tail.wrapping_add(1);
        }
    }
}

/// De zendkant. `Send`, niet `Sync`: één taak tegelijk.
pub struct Sender<'a, T, const N: usize> {
    ch: &'a Channel<T, N>,
    _one: PhantomData<Cell<()>>,
}

/// De ontvangkant. `Send`, niet `Sync`: één taak tegelijk.
pub struct Receiver<'a, T, const N: usize> {
    ch: &'a Channel<T, N>,
    _one: PhantomData<Cell<()>>,
}

impl<'a, T, const N: usize> Sender<'a, T, N> {
    /// Zet `v` in de rij, of geeft hem terug als de rij vol is.
    pub fn try_send(&mut self, v: T) -> Result<(), Full<T>> {
        let head = self.ch.head.load(Relaxed);
        let tail = self.ch.tail.load(Acquire);
        if head.wrapping_sub(tail) >= N {
            return Err(Full(v));
        }
        // SAFETY: slot `head` is vrij (invariant: `head - tail < N`) en
        // alleen de zender schrijft het; `head` gaat pas daarna omhoog.
        unsafe { (*self.ch.slot(head)).write(v) };
        self.ch.head.store(head.wrapping_add(1), Release);
        self.ch.rx.wake();
        Ok(())
    }

    /// Wacht tot er plaats is en zet `v` dan in de rij.
    pub fn send(&mut self, v: T) -> SendFut<'_, 'a, T, N> {
        SendFut {
            tx: self,
            item: Some(v),
        }
    }

    /// Hoeveel plaats er nog is.
    #[must_use]
    pub fn free(&self) -> usize {
        N - self.ch.len()
    }
}

impl<'a, T, const N: usize> Receiver<'a, T, N> {
    /// Haalt het volgende element eruit, als het er is.
    pub fn try_recv(&mut self) -> Option<T> {
        let tail = self.ch.tail.load(Relaxed);
        let head = self.ch.head.load(Acquire);
        if head == tail {
            return None;
        }
        // SAFETY: slot `tail` is geïnitialiseerd (invariant) en de Acquire
        // op `head` ziet de schrijf; alleen de ontvanger leest het en zet
        // daarna `tail` omhoog, dus het wordt precies één keer genomen.
        let v = unsafe { (*self.ch.slot(tail)).assume_init_read() };
        self.ch.tail.store(tail.wrapping_add(1), Release);
        self.ch.tx.wake();
        Some(v)
    }

    /// Wacht op het volgende element.
    pub fn recv(&mut self) -> RecvFut<'_, 'a, T, N> {
        RecvFut { rx: self }
    }

    /// Het aantal elementen dat klaarligt.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ch.len()
    }

    /// Ligt er niets?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ch.is_empty()
    }
}

/// De future van [`Sender::send`].
#[must_use = "een future doet niets tot hij gepolld wordt"]
pub struct SendFut<'s, 'a, T, const N: usize> {
    tx: &'s mut Sender<'a, T, N>,
    item: Option<T>,
}

// `item` wordt nooit gepind gebruikt (hij verlaat de future als waarde),
// dus de future is Unpin ongeacht `T`.
impl<T, const N: usize> Unpin for SendFut<'_, '_, T, N> {}

impl<T, const N: usize> Future for SendFut<'_, '_, T, N> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let Some(v) = this.item.take() else {
            return Poll::Ready(());
        };
        match this.tx.try_send(v) {
            Ok(()) => Poll::Ready(()),
            Err(Full(v)) => {
                this.tx.ch.tx.register(cx.waker());
                match this.tx.try_send(v) {
                    Ok(()) => Poll::Ready(()),
                    Err(Full(v)) => {
                        this.item = Some(v);
                        Poll::Pending
                    }
                }
            }
        }
    }
}

/// De future van [`Receiver::recv`].
#[must_use = "een future doet niets tot hij gepolld wordt"]
pub struct RecvFut<'s, 'a, T, const N: usize> {
    rx: &'s mut Receiver<'a, T, N>,
}

impl<T, const N: usize> Unpin for RecvFut<'_, '_, T, N> {}

impl<T, const N: usize> Future for RecvFut<'_, '_, T, N> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        if let Some(v) = this.rx.try_recv() {
            return Poll::Ready(v);
        }
        this.rx.ch.rx.register(cx.waker());
        match this.rx.try_recv() {
            Some(v) => Poll::Ready(v),
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{poll_once, waker};
    use std::rc::Rc;
    use std::sync::atomic::Ordering::SeqCst;

    #[test]
    fn fifo_full_and_wrap() {
        let ch: Channel<u32, 2> = Channel::new();
        let (mut tx, mut rx) = ch.split().unwrap();
        assert!(ch.split().is_none());
        assert_eq!(tx.try_send(1), Ok(()));
        assert_eq!(tx.try_send(2), Ok(()));
        assert_eq!(tx.try_send(3), Err(Full(3)));
        assert_eq!(rx.try_recv(), Some(1));
        assert_eq!(tx.try_send(3), Ok(())); // omloop
        assert_eq!(rx.try_recv(), Some(2));
        assert_eq!(rx.try_recv(), Some(3));
        assert_eq!(rx.try_recv(), None);
        assert_eq!(tx.free(), 2);
    }

    #[test]
    fn recv_waits_and_is_woken_by_send() {
        let ch: Channel<u32, 1> = Channel::new();
        let (mut tx, mut rx) = ch.split().unwrap();
        let (c, w) = waker();
        {
            let mut f = core::pin::pin!(rx.recv());
            assert_eq!(poll_once(&mut f, &w), Poll::Pending);
        }
        assert_eq!(tx.try_send(7), Ok(()));
        assert_eq!(c.0.load(SeqCst), 1);
        let mut f = core::pin::pin!(rx.recv());
        assert_eq!(poll_once(&mut f, &w), Poll::Ready(7));
    }

    #[test]
    fn send_waits_for_room_and_is_woken_by_recv() {
        let ch: Channel<u32, 1> = Channel::new();
        let (mut tx, mut rx) = ch.split().unwrap();
        let (c, w) = waker();
        assert_eq!(tx.try_send(1), Ok(()));
        let mut f = core::pin::pin!(tx.send(2));
        assert_eq!(poll_once(&mut f, &w), Poll::Pending);
        assert_eq!(rx.try_recv(), Some(1));
        assert_eq!(c.0.load(SeqCst), 1);
        assert_eq!(poll_once(&mut f, &w), Poll::Ready(()));
        assert_eq!(rx.try_recv(), Some(2));
    }

    #[test]
    fn drop_releases_what_is_left() {
        let rc = Rc::new(());
        {
            let ch: Channel<Rc<()>, 4> = Channel::new();
            let (mut tx, mut rx) = ch.split().unwrap();
            for _ in 0..3 {
                let _ = tx.try_send(rc.clone());
            }
            let _ = rx.try_recv();
            assert_eq!(Rc::strong_count(&rc), 3);
        }
        assert_eq!(Rc::strong_count(&rc), 1);
    }
}
