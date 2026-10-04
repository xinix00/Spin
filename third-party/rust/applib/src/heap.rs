//! De app-heap: dezelfde allocator als de kern, met de app-core-identiteit.

pub use heap::{Corrupt, HDR, HeapStats, LockStats, MAX_ALIGN, Walk};

/// De core-identiteit voor de app-allocator.
#[doc(hidden)]
pub struct AppCore;
impl heap::CoreId for AppCore {
    fn id() -> u64 {
        crate::arch::core_id()
    }
}

/// Vrije lijsten met het app-plafond en SMP-synchronisatie.
pub type Heap = heap::Heap<AppCore>;

/// De heap van dit image, en op het target de global allocator.
#[cfg_attr(all(target_os = "none", not(test)), global_allocator)]
pub static HEAP: Heap = Heap::new();
