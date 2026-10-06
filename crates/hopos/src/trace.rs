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
    Maintenance,
    Capture,
}
const NAMES: [&str; 16] = [
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
    "maintenance",
    "capture",
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

/// Wat de eigenaar per minuut schrijft: mutaties, rijen en bytes per collectie,
/// en blobopdrachten met bytes per soort. `SPIN_WRITES` vat het samen.
#[derive(Default)]
pub(crate) struct Writes {
    since: u64,
    saves: u32,
    rows: alloc::vec::Vec<(&'static str, u32, u64)>,
    blobs: alloc::vec::Vec<(&'static str, u32, u64)>,
}
fn add(list: &mut alloc::vec::Vec<(&'static str, u32, u64)>, key: &'static str, bytes: u64) {
    if let Some(entry) = list.iter_mut().find(|(k, _, _)| *k == key) {
        entry.1 += 1;
        entry.2 = entry.2.saturating_add(bytes);
    } else if list.try_reserve(1).is_ok() {
        list.push((key, 1, bytes));
    }
}
impl Writes {
    /// Eén opslag met deze rijen.
    pub(crate) fn save(&mut self, rows: &[spin_persistence::Row]) {
        self.saves += 1;
        for row in rows {
            add(
                &mut self.rows,
                row.collection,
                row.value.as_ref().map_or(0, |v| v.len() as u64),
            );
        }
    }
    /// Eén blobopdracht.
    pub(crate) fn blob(&mut self, request: &spin_store::BlobRequest<'_>) {
        use spin_store::BlobRequest as Q;
        let (key, bytes): (&'static str, u64) = match request {
            Q::Begin { .. } => ("upload_begin", 0),
            Q::Write { bytes, .. } => ("upload_write", bytes.len() as u64),
            Q::Publish { .. } => ("upload_publish", 0),
            Q::Abandon(_) => ("upload_abandon", 0),
            Q::Put {
                reference, bytes, ..
            } => (
                match reference.split(':').next().unwrap_or("") {
                    "manifest" => "put_manifest",
                    "attachment" => "put_attachment",
                    "bundle" => "put_bundle",
                    "snapshot" => "put_snapshot",
                    _ => "put_other",
                },
                bytes.len() as u64,
            ),
            Q::Delete(_) => ("delete", 0),
            Q::Info(_) | Q::Chunk { .. } | Q::Pending { .. } => return,
        };
        add(&mut self.blobs, key, bytes);
    }
    /// Logt en begint opnieuw, eens per minuut.
    pub(crate) fn report(&mut self) {
        let now = now_ms();
        if self.since == 0 {
            self.since = now;
            return;
        }
        if now.saturating_sub(self.since) < 60_000 {
            return;
        }
        let mut rows = alloc::string::String::new();
        for (key, count, bytes) in &self.rows {
            let _ = core::fmt::Write::write_fmt(
                &mut rows,
                format_args!("{key}:{count}/{}KB ", bytes / 1024),
            );
        }
        let mut blobs = alloc::string::String::new();
        for (key, count, bytes) in &self.blobs {
            let _ = core::fmt::Write::write_fmt(
                &mut blobs,
                format_args!("{key}:{count}/{}KB ", bytes / 1024),
            );
        }
        applib::log!(
            "SPIN_WRITES s={} saves={} rows=[{}] blobs=[{}]",
            now.saturating_sub(self.since) / 1000,
            self.saves,
            rows.trim_end(),
            blobs.trim_end()
        );
        *self = Self {
            since: now,
            ..Self::default()
        };
    }
}
