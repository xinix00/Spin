//! Een begrensde eigen stack voor synchrone C-code die op async I/O wacht.
//!
//! Eén eigenaar/core; geen geneste executor. `Suspender::wait` pollt uitsluitend
//! de gevraagde future en geeft bij Pending de oorspronkelijke executor terug.
//! Drop hervat met annulering: callbacks krijgen Cancelled, zodat C normaal
//! zijn eigen stack kan verlaten vóór vrijgave. Panics over de C-ingang aborteren.
use alloc::{
    alloc::{Layout, alloc_zeroed, dealloc},
    boxed::Box,
};
use core::{
    cell::UnsafeCell,
    future::Future,
    marker::PhantomData,
    pin::{Pin, pin},
    ptr::NonNull,
    task::{Context, Poll, Waker},
};
mod arch;
/// Een begrensde stack kon niet worden gemaakt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// De opgegeven maat ligt buiten 64 KiB..=16 MiB.
    Size,
    /// Heapallocatie geweigerd.
    Memory,
}
/// De omvattende taak werd geannuleerd; voltooi het synchrone opruimpad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;
#[repr(C, align(16))]
struct Registers {
    general: [u64; 14],
    float: [u64; 12],
    control: [u64; 2],
}
impl Registers {
    const ZERO: Self = Self {
        general: [0; 14],
        float: [0; 12],
        control: [0; 2],
    };
}
struct Raw {
    parent: Registers,
    child: Registers,
    waker: Waker,
    cancelled: bool,
    finished: bool,
}
struct State<F, R> {
    raw: UnsafeCell<Raw>,
    function: UnsafeCell<Option<F>>,
    result: UnsafeCell<Option<R>>,
}
struct Stack {
    base: NonNull<u8>,
    layout: Layout,
}
impl Stack {
    fn new(bytes: usize) -> Result<Self, Error> {
        if !(65536..=16 << 20).contains(&bytes) {
            return Err(Error::Size);
        }
        let size = bytes.checked_add(15).ok_or(Error::Size)? & !15;
        let layout = Layout::from_size_align(size, 16).map_err(|_| Error::Size)?;
        // SAFETY: Niet-lege, geldige layout; null is een gewone allocatiefout.
        let base = NonNull::new(unsafe { alloc_zeroed(layout) }).ok_or(Error::Memory)?;
        Ok(Self { base, layout })
    }
    fn top(&self) -> usize {
        self.base.as_ptr() as usize + self.layout.size()
    }
}
impl Drop for Stack {
    fn drop(&mut self) {
        // SAFETY: Alleen na voltooide child of vóór zijn eerste start; niemand leent de stack nog.
        unsafe { dealloc(self.base.as_ptr(), self.layout) };
    }
}
/// Alleen op de private stack beschikbaar; kan niet naar een andere core.
pub struct Suspender {
    raw: NonNull<UnsafeCell<Raw>>,
    _local: PhantomData<*mut ()>,
}
impl Suspender {
    fn raw(&self) -> *mut Raw {
        // SAFETY: State staat op zijn vaste heapadres tot de child voltooid is.
        unsafe { self.raw.as_ref().get() }
    }
    /// Poll één I/O-future; geef bij Pending de normale executor terug.
    /// Na annulering wordt de future gedropt op dezelfde C-stack als waarop hij ontstond.
    pub fn wait<F: Future>(&self, future: F) -> Result<F::Output, Cancelled> {
        let mut future = pin!(future);
        loop {
            // SAFETY: De private stack is de enige lopende context; de parent
            // schrijft uitsluitend terwijl deze stack geschorst is. Geen lening
            // van Raw, Waker of Context leeft over switch heen.
            let (cancelled, waker) = unsafe {
                let r = &*self.raw();
                (r.cancelled, r.waker.clone())
            };
            if cancelled {
                return Err(Cancelled);
            }
            let poll = future.as_mut().poll(&mut Context::from_waker(&waker));
            drop(waker);
            match poll {
                Poll::Ready(v) => return Ok(v),
                Poll::Pending => {
                    // SAFETY: Beide registersets horen bij deze ene eigenaar en
                    // beide stacks blijven gereserveerd; alleen één context loopt.
                    unsafe {
                        let r = self.raw();
                        arch::switch(
                            core::ptr::addr_of_mut!((*r).child),
                            core::ptr::addr_of!((*r).parent),
                        );
                    }
                }
            }
        }
    }
}
/// Een synchrone aanroep als future, met eigen stack en expliciete annulering.
/// `!Send`/`!Sync`: aanmaken, pollen en droppen blijven op dezelfde eigenaar-core.
#[must_use = "een stacktaak doet niets totdat hij gepolld wordt"]
pub struct Task<F: FnOnce(&Suspender) -> R, R> {
    state: Box<State<F, R>>,
    _stack: Stack,
    started: bool,
    taken: bool,
    _local: PhantomData<*mut ()>,
}
impl<F: FnOnce(&Suspender) -> R, R> Task<F, R> {
    /// Reserveer vóór uitvoering de volledige stack.
    ///
    /// # Safety
    /// De aanroeper bewijst dat de maximale C/Rust-stackdiepte in `bytes` past:
    /// de slot-heap heeft geen guard pages. Het synchrone werk moet na Cancelled
    /// alle callbacks afwikkelen en terugkeren; geen longjmp buiten deze stack,
    /// geen verwijzingen naar deze stack bewaren na voltooiing. SQLite wordt
    /// uitsluitend binnen deze taak gebruikt en elders niet heringetreden.
    /// RISC-V vereist de actieve D-FPU van het ondersteunde rv64gc/lp64d-target.
    pub unsafe fn new(bytes: usize, function: F) -> Result<Self, Error> {
        let stack = Stack::new(bytes)?;
        // Een fallibele Box zonder nightly allocator_api.
        let layout = Layout::new::<State<F, R>>();
        // SAFETY: Layout bevat Raw en is dus niet leeg; bij null nog geen eigendom.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) }.cast::<State<F, R>>())
            .ok_or(Error::Memory)?;
        // SAFETY: Exclusieve verse allocatie, één write vóór gebruik, Box neemt
        // exact dezelfde global-allocator-layout in eigendom.
        let state = unsafe {
            ptr.as_ptr().write(State {
                raw: UnsafeCell::new(Raw {
                    parent: Registers::ZERO,
                    child: Registers::ZERO,
                    waker: Waker::noop().clone(),
                    cancelled: false,
                    finished: false,
                }),
                function: UnsafeCell::new(Some(function)),
                result: UnsafeCell::new(None),
            });
            Box::from_raw(ptr.as_ptr())
        };
        // SAFETY: De child loopt nog niet. De entry en het vaste State-adres
        // blijven geldig tot entry zichzelf voltooid aan de parent teruggeeft.
        unsafe {
            arch::init(
                &mut (*state.raw.get()).child,
                stack.top(),
                ptr.as_ptr().cast(),
                entry::<F, R>,
            );
        }
        Ok(Self {
            state,
            _stack: stack,
            started: false,
            taken: false,
            _local: PhantomData,
        })
    }
    fn resume(&mut self, waker: &Waker) {
        // SAFETY: Alleen de parent loopt; korte leningen eindigen vóór switch.
        unsafe {
            let r = self.state.raw.get();
            (*r).waker = waker.clone();
            self.started = true;
            arch::switch(
                core::ptr::addr_of_mut!((*r).parent),
                core::ptr::addr_of!((*r).child),
            );
        }
    }
}
// De Task zelf mag verplaatsen: State en Stack blijven op hun eigen heapadres.
impl<F: FnOnce(&Suspender) -> R, R> Unpin for Task<F, R> {}
impl<F: FnOnce(&Suspender) -> R, R> Future for Task<F, R> {
    type Output = R;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        let this = self.get_mut();
        if this.taken {
            return Poll::Pending;
        }
        this.resume(cx.waker());
        // SAFETY: Child gaf de uitvoering terug, dus result wordt niet geschreven.
        unsafe {
            if (*this.state.raw.get()).finished {
                this.taken = true;
                match (*this.state.result.get()).take() {
                    Some(r) => Poll::Ready(r),
                    None => Poll::Pending,
                }
            } else {
                Poll::Pending
            }
        }
    }
}
impl<F: FnOnce(&Suspender) -> R, R> Drop for Task<F, R> {
    fn drop(&mut self) {
        // SAFETY: Drop kan uitsluitend in de parent lopen; de child ziet cancelled
        // vóór iedere volgende poll en verlaat zijn synchrone callback normaal.
        unsafe {
            let r = self.state.raw.get();
            (*r).cancelled = true;
            while self.started && !(*r).finished {
                self.resume(Waker::noop());
            }
        }
    }
}
extern "C" fn entry<F: FnOnce(&Suspender) -> R, R>(opaque: *mut ()) -> ! {
    let state = opaque.cast::<State<F, R>>();
    // SAFETY: Task::new initialiseert een uniek, vast State-adres. Deze ingang
    // wordt precies één keer gestart, vóórdat de parent result leest.
    unsafe {
        let raw = core::ptr::addr_of_mut!((*state).raw);
        let suspension = Suspender {
            raw: NonNull::new_unchecked(raw),
            _local: PhantomData,
        };
        if let Some(function) = (*(*state).function.get()).take() {
            *(*state).result.get() = Some(function(&suspension));
        }
        let r = (*raw).get();
        (*r).finished = true;
        arch::switch(
            core::ptr::addr_of_mut!((*r).child),
            core::ptr::addr_of!((*r).parent),
        );
    }
    // Een voltooide child wordt nooit hervat. Geen unwind over de C-ingang.
    loop {
        core::hint::spin_loop();
    }
}
#[cfg(test)]
mod tests;
