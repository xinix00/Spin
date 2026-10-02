//! Laagste allocatiegrens voor gepinde verbindingstaken.
use alloc::{
    alloc::{Layout, alloc},
    boxed::Box,
};
use core::{future::Future, pin::Pin};
pub(crate) type Task<'a, T = ()> = Pin<Box<dyn Future<Output = T> + 'a>>;
pub(crate) fn task<'a, F: Future + 'a>(future: F) -> spin_server::Result<Task<'a, F::Output>> {
    let layout = Layout::new::<F>();
    if layout.size() == 0 {
        return Ok(Box::pin(future));
    }
    // SAFETY: layout hoort bij F. Bij null blijft future lokaal en wordt hij
    // normaal gedropt; geen ongeïnitialiseerde Box of referentie wordt gemaakt.
    let pointer = unsafe { alloc(layout) }.cast::<F>();
    if pointer.is_null() {
        return Err(spin_server::Error::Http(
            503,
            "connection allocation failed",
        ));
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
