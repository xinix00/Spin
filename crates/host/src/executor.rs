//! Kleine host-executor; iedere toekomstige taak wordt faalbaar gepind.
use std::{
    alloc::{Layout, alloc},
    cell::Cell,
    future::Future,
    os::fd::RawFd,
    pin::Pin,
    task::{Context, Poll, Waker},
    time::Duration,
};
std::thread_local! { static PROGRESS: Cell<bool> = const { Cell::new(false) }; }
struct Readiness {
    descriptors: [libc::pollfd; 256],
    len: usize,
}
std::thread_local! {
    static READINESS: std::cell::RefCell<Readiness> = const { std::cell::RefCell::new(Readiness {
        descriptors: [libc::pollfd { fd: -1, events: 0, revents: 0 }; 256],
        len: 0,
    }) };
}
// Interests live for one executor round only. No descriptor ownership moves;
// an operation dropped during the round simply wakes poll with POLLNVAL.
pub(crate) fn wait_for(fd: RawFd, events: i16) {
    READINESS.with(|ready| {
        let mut ready = ready.borrow_mut();
        let len = ready.len;
        if let Some(entry) = ready.descriptors[..len]
            .iter_mut()
            .find(|entry| entry.fd == fd)
        {
            entry.events |= events;
        } else if len < ready.descriptors.len() {
            ready.descriptors[len] = libc::pollfd {
                fd,
                events,
                revents: 0,
            };
            ready.len += 1;
        }
    });
}
// Een volle I/O-quantum vraagt een nieuwe eerlijke ronde, geen timerpauze.
pub(crate) fn progress() {
    PROGRESS.with(|flag| flag.set(true));
}
pub(crate) fn idle() {
    let progressed = PROGRESS.with(|flag| flag.replace(false));
    let waited = READINESS.with(|ready| {
        let mut ready = ready.borrow_mut();
        let len = std::mem::take(&mut ready.len);
        if progressed || len == 0 {
            return false;
        }
        // SAFETY: the fixed array owns len initialized pollfd entries and stays
        // exclusively borrowed for this bounded call. poll only observes fds.
        unsafe { libc::poll(ready.descriptors.as_mut_ptr(), len as libc::nfds_t, 10) };
        true
    });
    if !progressed && !waited {
        std::thread::sleep(Duration::from_millis(10));
    }
}
pub(crate) type Task<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;
pub(crate) fn task<'a, F: Future<Output = ()> + 'a>(future: F) -> std::io::Result<Task<'a>> {
    let layout = Layout::new::<F>();
    if layout.size() == 0 {
        return Ok(Box::pin(future));
    }
    // SAFETY: layout hoort bij F. Bij null blijft future lokaal en wordt hij
    // normaal gedropt; geen ongeïnitialiseerde Box of referentie wordt gemaakt.
    let pointer = unsafe { alloc(layout) }.cast::<F>();
    if pointer.is_null() {
        return Err(std::io::Error::from(std::io::ErrorKind::OutOfMemory));
    }
    // SAFETY: De unieke allocatie is groot en uitgelijnd voor F. write vestigt
    // de waarde; Box krijgt hetzelfde allocatie-eigendom, waarna Pin verplaatsen
    // voorkomt tot Drop. Geen andere verwijzing naar de future wordt behouden.
    let boxed = unsafe {
        pointer.write(future);
        Box::from_raw(pointer)
    };
    Ok(Box::into_pin(boxed))
}
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => idle(),
        }
    }
}
pub(crate) async fn next_round() {
    let mut yielded = false;
    std::future::poll_fn(|_| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            Poll::Pending
        }
    })
    .await
}
