//! De upload van Replica-delen op een eigen stack, naast de geparkeerde eigenaar.
//!
//! De eigenaar doet de capture (één korte leestransactie) en de afronding
//! (marker, manifest, bevestiging); de delen gaan hier de lijn op terwijl de
//! eigenaar verzoeken blijft bedienen. Hoogstens één upload per tenant tegelijk.
use alloc::{rc::Rc, string::String};
use core::{
    cell::{Cell, RefCell},
    future::poll_fn,
    task::{Poll, Waker},
};
use replica_core::{
    owner::{Maintenance, Pending, Synced},
    replication::Uploaded,
    writer::Writer,
};

/// Werk voor de uploader: de delen van een capture, of een onderhoudsbeurt.
// Er is hoogstens één taak tegelijk; een box zou alleen een allocatie toevoegen.
#[allow(clippy::large_enum_variant)]
pub(in crate::boot) enum Work {
    Upload(Pending),
    Maintain(Maintenance),
}
/// Het werk met de plek van de database van zijn tenant.
pub(in crate::boot) type Job = (Work, super::backend::Location);
/// Het resultaat dat de eigenaar afrondt.
#[allow(clippy::large_enum_variant)]
pub(in crate::boot) enum Outcome {
    Upload(Pending, replica_core::Result<Uploaded>),
    Maintain(replica_core::Result<Synced>),
}
/// Het overdrachtspunt tussen de eigenaar en zijn uploader; beide draaien op
/// dezelfde kern, dus RefCell volstaat.
pub(in crate::boot) struct Uploads {
    job: RefCell<Option<Job>>,
    done: RefCell<Option<Outcome>>,
    waker: RefCell<Option<Waker>>,
    busy: Cell<bool>,
    /// De schrijverlease met zijn sleutel, voor de leasetaak: die vernieuwt hem
    /// op zijn eigen stack, hoe lang eigenaar en uploader ook bezig zijn.
    lease: RefCell<Option<(Rc<RefCell<Writer>>, String)>>,
    lease_lost: Cell<bool>,
}
impl Uploads {
    pub(in crate::boot) fn new() -> Self {
        Self {
            job: RefCell::new(None),
            done: RefCell::new(None),
            waker: RefCell::new(None),
            busy: Cell::new(false),
            lease: RefCell::new(None),
            lease_lost: Cell::new(false),
        }
    }
    /// Bij een (her)start van de eigenaar: een vorige upload ging met zijn stack weg.
    pub(in crate::boot) fn reset(&self) {
        *self.job.borrow_mut() = None;
        *self.done.borrow_mut() = None;
        self.busy.set(false);
        *self.lease.borrow_mut() = None;
        self.lease_lost.set(false);
    }
    /// De eigenaar geeft na Prepare zijn lease aan de leasetaak.
    pub(in crate::boot) fn install_lease(&self, writer: Rc<RefCell<Writer>>, key: String) {
        self.lease_lost.set(false);
        *self.lease.borrow_mut() = Some((writer, key));
    }
    pub(in crate::boot) fn lease(&self) -> Option<(Rc<RefCell<Writer>>, String)> {
        self.lease.borrow().clone()
    }
    /// De leasetaak meldt verlies; de eigenaar stopt dan als bij elke verloren lease.
    pub(in crate::boot) fn mark_lease_lost(&self) {
        self.lease_lost.set(true);
        *self.lease.borrow_mut() = None;
    }
    pub(in crate::boot) fn lease_lost(&self) -> bool {
        self.lease_lost.get()
    }
    /// De eigenaar geeft een capture af en wekt de uploader.
    pub(in crate::boot) fn start(&self, work: Work, database: super::backend::Location) {
        self.busy.set(true);
        *self.job.borrow_mut() = Some((work, database));
        if let Some(waker) = self.waker.borrow_mut().take() {
            waker.wake();
        }
    }
    /// Of er een upload loopt of op afronding door de eigenaar wacht.
    pub(in crate::boot) fn busy(&self) -> bool {
        self.busy.get()
    }
    /// De eigenaar haalt een afgeronde upload op voor [`replica_core::owner::Replica::finish`].
    pub(in crate::boot) fn take_done(&self) -> Option<Outcome> {
        let done = self.done.borrow_mut().take();
        if done.is_some() {
            self.busy.set(false);
        }
        done
    }
    /// De uploader wacht op de volgende capture.
    pub(in crate::boot) async fn next(&self) -> Job {
        poll_fn(|cx| match self.job.borrow_mut().take() {
            Some(job) => Poll::Ready(job),
            None => {
                *self.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
    /// De uploader levert het resultaat af; de eigenaar rondt af bij zijn volgende onderhoud.
    pub(in crate::boot) fn done(&self, outcome: Outcome) {
        *self.done.borrow_mut() = Some(outcome);
    }
}
