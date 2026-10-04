//! Begrensde verzamelingen: vaste capaciteit in het type, geen heap, en
//! "vol" is een `Result`, geen abort.
//!
//! Een verzameling die in een lus groeit is een fout (handboek §6); wat hier
//! staat groeit nooit. `BoundedVec<T, N>` is de basisvorm: een array met een
//! lengte. `Text<N>` is hetzelfde voor tekst, met `fmt::Write`. Wie meer
//! nodig heeft (een index-map, een set) bouwt het hierop en zet het hier
//! neer.

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

use core::fmt;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};

/// De verzameling is vol; het element komt terug naar de aanroeper.
#[derive(Debug, PartialEq, Eq)]
pub struct Full<T>(pub T);

/// Een vector met vaste capaciteit `N`, op de stack of in een static.
///
/// # Invariants
///
/// De eerste `len` elementen van `buf` zijn geïnitialiseerd; de rest niet.
pub struct BoundedVec<T, const N: usize> {
    buf: [MaybeUninit<T>; N],
    len: usize,
}

impl<T, const N: usize> BoundedVec<T, N> {
    /// Een lege vector.
    #[must_use]
    pub const fn new() -> Self {
        // INVARIANT: `len` is 0, dus er hoeft niets geïnitialiseerd te zijn.
        Self {
            buf: [const { MaybeUninit::uninit() }; N],
            len: 0,
        }
    }

    /// De capaciteit `N`.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Het aantal elementen.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Geen elementen?
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Zit hij vol?
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.len == N
    }

    /// Voegt `v` achteraan toe, of geeft hem terug als de vector vol is.
    pub fn push(&mut self, v: T) -> Result<(), Full<T>> {
        if self.len == N {
            return Err(Full(v));
        }
        self.buf[self.len].write(v);
        // INVARIANT: element `len` is zojuist geschreven.
        self.len += 1;
        Ok(())
    }

    /// Haalt het laatste element weg.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: door de invariant was element `len` (na de verlaging)
        // geïnitialiseerd, en hij telt nu niet meer mee, dus we nemen hem
        // precies één keer.
        Some(unsafe { self.buf[self.len].assume_init_read() })
    }

    /// Haalt element `i` weg door het laatste ernaartoe te verplaatsen.
    /// `None` als `i` buiten bereik is.
    pub fn swap_remove(&mut self, i: usize) -> Option<T> {
        if i >= self.len {
            return None;
        }
        let last = self.len - 1;
        self.as_mut_slice().swap(i, last);
        self.pop()
    }

    /// Haalt element `i` weg en schuift de rest op; `None` buiten bereik.
    pub fn remove(&mut self, i: usize) -> Option<T> {
        if i >= self.len {
            return None;
        }
        // SAFETY: `i < len`, dus geïnitialiseerd; we lezen hem één keer en
        // schuiven de elementen erboven een plek omlaag, waarna de oude
        // laatste plek niet meer meetelt.
        let v = unsafe { self.buf[i].assume_init_read() };
        let base = self.buf.as_mut_ptr();
        // SAFETY: `i + 1..len` ligt binnen de array en `i` ook; de bron en
        // het doel overlappen, dus `ptr::copy` (memmove). De bytes zijn
        // `MaybeUninit`, dus verplaatsen zonder droppen is precies goed.
        unsafe { core::ptr::copy(base.add(i + 1), base.add(i), self.len - i - 1) };
        self.len -= 1;
        Some(v)
    }

    /// Kort in tot `n` elementen; de rest wordt gedropt.
    pub fn truncate(&mut self, n: usize) {
        while self.len > n {
            let _ = self.pop();
        }
    }

    /// Verwijdert alle elementen.
    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Houdt alleen de elementen waarvoor `keep` waar is.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        let mut i = 0;
        while i < self.len {
            if keep(&self[i]) {
                i += 1;
            } else {
                let _ = self.remove(i);
            }
        }
    }

    /// De elementen als slice.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: de eerste `len` elementen zijn geïnitialiseerd (invariant),
        // en `MaybeUninit<T>` heeft dezelfde indeling als `T`.
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr().cast::<T>(), self.len) }
    }

    /// De elementen als veranderlijke slice.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: zie `as_slice`; `&mut self` sluit andere toegang uit.
        unsafe { core::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast::<T>(), self.len) }
    }
}

impl<T, const N: usize> Drop for BoundedVec<T, N> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<T, const N: usize> Default for BoundedVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Deref for BoundedVec<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, const N: usize> DerefMut for BoundedVec<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T: fmt::Debug, const N: usize> fmt::Debug for BoundedVec<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: Clone, const N: usize> Clone for BoundedVec<T, N> {
    fn clone(&self) -> Self {
        let mut out = Self::new();
        for v in self.iter() {
            // Kan niet vol zijn: dezelfde N.
            if out.push(v.clone()).is_err() {
                break;
            }
        }
        out
    }
}

/// Tekst van hooguit `N` bytes, op de stack of in een static:
/// `fmt::Write` zonder allocatie.
///
/// Wat niet past, valt weg. Een `write_str` schrijft het langste stuk dat
/// past, tot op een hele UTF-8-letter, en geeft dan `fmt::Error`; zo doen
/// `RawFormatter` en `Formatter` het in Linux (`rust/kernel/str.rs`). Wie
/// de afgekapte tekst wil, negeert de fout.
///
/// # Invariants
///
/// `buf[..len]` is geldige UTF-8 en `len <= N`.
#[derive(Clone, Copy)]
pub struct Text<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Text<N> {
    /// Een lege tekst.
    #[must_use]
    pub const fn new() -> Self {
        // INVARIANT: leeg is geldige UTF-8.
        Self {
            buf: [0; N],
            len: 0,
        }
    }

    /// De tekst.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Door de invariant altijd `Ok`.
        core::str::from_utf8(self.as_bytes()).unwrap_or_default()
    }

    /// De tekst als bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or_default()
    }

    /// De lengte in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Leeg?
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> fmt::Write for Text<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let take = s.floor_char_boundary(N - self.len);
        let (Some(dst), Some(src)) = (
            self.buf.get_mut(self.len..self.len + take),
            s.as_bytes().get(..take),
        ) else {
            return Err(fmt::Error);
        };
        dst.copy_from_slice(src);
        // INVARIANT: `take` eindigt op een lettergrens en past.
        self.len += take;
        if take == s.len() {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}

impl<const N: usize> Default for Text<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Debug for Text<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write as _;
    use std::rc::Rc;

    #[test]
    fn push_pop_full() {
        let mut v: BoundedVec<u32, 2> = BoundedVec::new();
        assert!(v.is_empty());
        assert_eq!(v.push(1), Ok(()));
        assert_eq!(v.push(2), Ok(()));
        assert!(v.is_full());
        assert_eq!(v.push(3), Err(Full(3)));
        assert_eq!(&*v, &[1, 2]);
        assert_eq!(v.pop(), Some(2));
        assert_eq!(v.pop(), Some(1));
        assert_eq!(v.pop(), None);
    }

    #[test]
    fn remove_and_swap_remove() {
        let mut v: BoundedVec<u32, 4> = BoundedVec::new();
        for i in 1..=4 {
            let _ = v.push(i);
        }
        assert_eq!(v.remove(1), Some(2));
        assert_eq!(&*v, &[1, 3, 4]);
        assert_eq!(v.swap_remove(0), Some(1));
        assert_eq!(&*v, &[4, 3]);
        assert_eq!(v.remove(5), None);
    }

    #[test]
    fn retain_and_drop_count() {
        let rc = Rc::new(());
        let mut v: BoundedVec<Rc<()>, 8> = BoundedVec::new();
        for _ in 0..5 {
            let _ = v.push(rc.clone());
        }
        assert_eq!(Rc::strong_count(&rc), 6);
        let mut n = 0;
        v.retain(|_| {
            n += 1;
            n % 2 == 0
        });
        assert_eq!(v.len(), 2);
        assert_eq!(Rc::strong_count(&rc), 3);
        drop(v);
        assert_eq!(Rc::strong_count(&rc), 1);
    }

    #[test]
    fn text_cuts_on_a_letter_and_says_so() {
        let mut t: Text<6> = Text::new();
        assert_eq!(write!(t, "ab{}", 12), Ok(()));
        assert_eq!(t.as_str(), "ab12");
        // De é (twee bytes) past nog, de ë niet meer half.
        assert!(t.write_str("\u{e9}\u{eb}").is_err());
        assert_eq!(t.as_str(), "ab12\u{e9}");
        assert_eq!(t.len(), 6);
        assert!(t.write_str("x").is_err());
        assert!(t.write_str("").is_ok());
        assert_eq!(format!("{t:?}"), "\"ab12\u{e9}\"");
        assert!(Text::<0>::new().is_empty());
    }
}
