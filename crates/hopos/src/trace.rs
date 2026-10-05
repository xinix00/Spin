//! Welke opslagstap de eigenaar en de uploader nu doen. De boot-lus zet het in
//! SPIN_OWNER_PHASE; een stap van een seconde of langer logt bij afloop
//! SPIN_STORAGE_STEP met zijn naam en duur.
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

/// Een opslagstap.
#[derive(Clone, Copy)]
pub(crate) enum Step {
    // 0 is "idle": nog niets begonnen.
    LeaseClaim = 1,
    Prepare,
    Renew,
    Finish,
    BeginCapture,
    SqlLoad,
    SqlSave,
    SqlReplace,
    SqlBlob,
    SqlUsage,
    SqlPurge,
    SqlRestore,
    UploadParts,
}
const NAMES: [&str; 14] = [
    "idle",
    "lease_claim",
    "prepare",
    "renew",
    "finish",
    "begin_capture",
    "sql_load",
    "sql_save",
    "sql_replace",
    "sql_blob",
    "sql_usage",
    "sql_purge",
    "sql_restore",
    "upload_parts",
];
pub(crate) struct Slot {
    step: AtomicUsize,
    since: AtomicU64,
}
static OWNER: Slot = Slot {
    step: AtomicUsize::new(0),
    since: AtomicU64::new(0),
};
static UPLOADER: Slot = Slot {
    step: AtomicUsize::new(0),
    since: AtomicU64::new(0),
};
fn now_ms() -> u64 {
    applib::clock::now_ns() / 1_000_000
}
/// Zet de stap terug naar wat er daarvoor liep, en logt een trage stap.
pub(crate) struct Guard {
    slot: &'static Slot,
    step: usize,
    previous: usize,
    started: u64,
    previous_since: u64,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let ms = now_ms().saturating_sub(self.started);
        if ms >= 1_000 {
            let who = if core::ptr::eq(self.slot, &UPLOADER) {
                "uploader"
            } else {
                "owner"
            };
            applib::log!(
                "SPIN_STORAGE_STEP who={who} step={} ms={ms}",
                NAMES[self.step]
            );
        }
        self.slot.step.store(self.previous, Relaxed);
        self.slot.since.store(self.previous_since, Relaxed);
    }
}
fn enter(slot: &'static Slot, step: Step) -> Guard {
    let started = now_ms();
    let guard = Guard {
        slot,
        step: step as usize,
        previous: slot.step.load(Relaxed),
        started,
        previous_since: slot.since.load(Relaxed),
    };
    slot.step.store(step as usize, Relaxed);
    slot.since.store(started, Relaxed);
    guard
}
/// De eigenaar begint aan `step` tot de guard valt.
pub(crate) fn owner_step(step: Step) -> Guard {
    enter(&OWNER, step)
}
/// De uploader begint aan `step` tot de guard valt.
pub(crate) fn uploader_step(step: Step) -> Guard {
    enter(&UPLOADER, step)
}
fn read(slot: &Slot) -> (&'static str, u64) {
    (
        NAMES.get(slot.step.load(Relaxed)).copied().unwrap_or("?"),
        slot.since.load(Relaxed),
    )
}
/// De lopende stap van de eigenaar en sinds wanneer (ms).
pub(crate) fn owner() -> (&'static str, u64) {
    read(&OWNER)
}
/// De lopende stap van de uploader en sinds wanneer (ms).
pub(crate) fn uploader() -> (&'static str, u64) {
    read(&UPLOADER)
}
