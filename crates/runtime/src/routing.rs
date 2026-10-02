//! Host selection happens before a connection can reach a tenant's mailbox.
use super::*;
/// A request either reaches an opened tenant or receives an immediate gateway response.
pub enum Route<'a> {
    /// Isolated state owner selected by the validated host.
    Owner(&'a Mailbox),
    /// Opening, liveness, or rejection response, independent of SQLite.
    Response(Response),
}
/// Bounded host table owned by the platform boot shell.
pub trait Routing {
    /// Resolve a host without waiting on storage or borrowing an application owner.
    fn route(&self, host: &str, path: &str, method: &str) -> Result<Route<'_>>;
    /// A global socket index remains reserved until its previous owner has detached it.
    fn occupied(&self, index: usize) -> bool;
}
#[derive(Clone, Copy)]
pub(super) enum Selection<'a> {
    Single(&'a Mailbox),
    Routed(&'a dyn Routing),
}
impl<'a> Selection<'a> {
    pub(super) fn active(self) -> bool {
        match self {
            Self::Single(mail) => mail.active.get(),
            Self::Routed(_) => true,
        }
    }
    pub(super) fn occupied(self, index: usize) -> bool {
        match self {
            Self::Single(mail) => mail.occupied(index),
            Self::Routed(routes) => routes.occupied(index),
        }
    }
    pub(super) fn route(self, host: &str, path: &str, method: &str) -> Result<Route<'a>> {
        match self {
            Self::Single(mail) => Ok(Route::Owner(mail)),
            Self::Routed(routes) => routes.route(host, path, method),
        }
    }
}
impl Mailbox {
    /// Whether a previous connection still needs cleanup on this owner.
    pub fn occupied(&self, index: usize) -> bool {
        self.slots
            .0
            .borrow()
            .get(index)
            .is_some_and(|slot| slot.occupied)
    }
    /// Owner initialization has completed and requests may be queued.
    pub fn active(&self) -> bool {
        self.active.get()
    }
}
/// Marks the selected mailbox finished even when socket I/O fails or is cancelled.
pub(super) struct Claim<'a> {
    pub(super) mail: Cell<Option<&'a Mailbox>>,
    pub(super) index: usize,
}
impl<'a> Claim<'a> {
    pub(super) fn set(&self, mail: &'a Mailbox) {
        self.mail.set(Some(mail));
        mail.slots.0.borrow_mut()[self.index].occupied = true;
    }
}
impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if let Some(mail) = self.mail.get() {
            let mut slots = mail.slots.0.borrow_mut();
            if slots[self.index].routed {
                slots[self.index].finished = true;
            } else {
                slots[self.index] = Slot::default();
            }
        }
    }
}
