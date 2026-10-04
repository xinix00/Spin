//! [`Local`]: een static die alleen de executor van één core aanraakt.

use core::cell::RefCell;
use core::ops::Deref;

/// Een waarde in een `static` die uitsluitend vanaf de executor van déze
/// core wordt aangeraakt.
///
/// `Sync` staat er met de hand op, en dat is het enige `unsafe impl Sync`
/// dat het handboek toestaat (§1.1). De belofte die dat sluitend maakt:
///
/// # Invariants
///
/// - Alleen taken op de executor van één core raken de waarde aan; een ISR
///   nooit, een andere core nooit. Wie een `Local` op twee cores gebruikt,
///   heeft een bug die dit type niet kan vangen.
/// - Een lening uit een [`LocalCell`] leeft nooit over een `.await`; de lint
///   `await_holding_refcell_ref` weigert de rest.
pub struct Local<T>(T);

// SAFETY: door de invariant is er nooit gelijktijdige toegang: de taken op
// één executor draaien na elkaar, en tussen twee `.await`-punten is er
// precies één van hen aan het woord.
unsafe impl<T> Sync for Local<T> {}

impl<T> Local<T> {
    /// Verpakt `v`.
    #[must_use]
    pub const fn new(v: T) -> Self {
        Self(v)
    }

    /// De waarde.
    #[must_use]
    pub const fn get(&self) -> &T {
        &self.0
    }

    /// De waarde, veranderlijk: wie de `Local` zelf bezit, leent niets.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.0
    }

    /// Geeft de waarde terug.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Deref for Local<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// De leesbare tabel uit het handboek §1.1: één taak schrijft, meerdere
/// lezen kort, en niemand leent hem over een `.await`.
pub type LocalCell<T> = Local<RefCell<T>>;

impl<T> LocalCell<T> {
    /// Een tabel in een static.
    #[must_use]
    pub const fn cell(v: T) -> Self {
        Local(RefCell::new(v))
    }
}
