//! Herstelpunten ontdekken en een bestaande generatie naar een offline database halen.
use crate::{
    Error, Result, grow,
    local::Name,
    manifest::{Layout, MAX_COMMITS},
    marker,
    object::{self, Store},
    replication, restore, string,
    time::Time,
};
use alloc::{string::String, vec::Vec};
use replica_sqlite::Storage;
/// Bovenlimiet voor één puntencatalogus, ook als de bucket meer historie bevat.
pub const MAX_POINTS: usize = 4096;
/// Een behouden herstelpunt; tijdstippen behouden nanosecondeprecisie.
#[derive(Debug, PartialEq, Eq)]
pub struct Point {
    /// Volledige generatie-ID.
    pub generation: String,
    /// Snapshot/commit-tijd, of het inclusieve einde van een compactievenster.
    pub at: Time,
    /// -1 voor snapshot, 0 voor raw, positief voor compactie.
    pub level: i32,
    /// De complete, door de lokale eigenaar gebruikte generatie.
    pub current: bool,
}
fn ids<S: Store>(store: &mut S, namespace: &str) -> Result<Vec<String>> {
    if namespace.is_empty() || namespace.ends_with('/') || namespace.contains("..") {
        return Err(Error::State);
    }
    let prefix = object::key(namespace, "/generations/")?;
    let mut listing = store.list(&prefix, MAX_COMMITS)?;
    if listing.len() > MAX_COMMITS {
        return Err(Error::Limit);
    }
    for o in &listing {
        if !o.key.starts_with(&prefix) || o.key.len() > 1024 {
            return Err(Error::Corrupt);
        }
    }
    listing.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    let mut ids = Vec::new();
    for o in &listing {
        let Some((id, _)) = o.key[prefix.len()..].split_once('/') else {
            continue;
        };
        if marker::generation_time(id).is_err() || ids.last().is_some_and(|old| old == id) {
            continue;
        }
        grow(&mut ids, 1, MAX_COMMITS)?;
        ids.push(string(id)?);
    }
    Ok(ids)
}
/// De eigenaar sluit gelijktijdige compactie/pruning uit met de exclusieve storelening.
/// Beschadigde oude generaties worden overgeslagen; schade aan current blijft een fout.
/// `limit` is een harde bovengrens: te veel punten geeft Limit, nooit een afgekapt succes.
pub fn points<S: Store>(
    store: &mut S,
    namespace: &str,
    current: &marker::Marker,
    limit: usize,
) -> Result<Vec<Point>> {
    if limit == 0 || limit > MAX_POINTS {
        return Err(Error::Limit);
    }
    let mut out = Vec::new();
    for generation in ids(store, namespace)? {
        let prefix = replication::generation_prefix(namespace, &generation)?;
        let layout = match object::layout(store, &prefix) {
            Ok(layout) => layout,
            Err(Error::Legacy) => continue,
            Err(Error::Corrupt) if generation != current.generation => continue,
            Err(e) => return Err(e),
        };
        append(&mut out, &layout, &generation, current, limit)?;
    }
    out.sort_unstable_by(|a, b| {
        b.at.cmp(&a.at)
            .then_with(|| b.current.cmp(&a.current))
            .then_with(|| b.generation.cmp(&a.generation))
    });
    Ok(out)
}
fn append(
    out: &mut Vec<Point>,
    layout: &Layout,
    generation: &str,
    current: &marker::Marker,
    limit: usize,
) -> Result {
    let snapshot = layout.snapshot();
    if snapshot.at == Time::ZERO {
        return Ok(());
    }
    let mut candidates = Vec::new();
    grow(&mut candidates, 1, MAX_COMMITS + 1)?;
    candidates.push((snapshot.at, -1));
    for m in layout.commits() {
        grow(&mut candidates, 1, MAX_COMMITS + 1)?;
        candidates.push((if m.level == 0 { m.at } else { m.end }, m.level as i32));
    }
    candidates.sort_unstable();
    // Eén tijdstip verschijnt eenmaal: snapshot heeft voorrang, verder het fijnste niveau.
    candidates.dedup_by_key(|p| p.0);
    for (at, level) in candidates {
        match layout.plan(Some(at)) {
            Ok(_) => {}
            Err(Error::Gap) => continue,
            Err(e) => return Err(e),
        }
        grow(out, 1, limit)?;
        out.push(Point {
            generation: string(generation)?,
            at,
            level,
            current: current.complete && generation == current.generation,
        });
    }
    Ok(())
}
/// Bestemming en keuze voor een offline restore, gescheiden van de levende database.
pub struct Fetch<'a> {
    /// Namespace `prefix/domain`.
    pub namespace: &'a str,
    /// Volledige generatie-ID; nooit een ongecontroleerd objectpad.
    pub generation: &'a str,
    /// None kiest het laatste behouden punt.
    pub at: Option<Time>,
    /// De levende database, in dezelfde canonieke VFS-naamruimte.
    pub live: Name,
    /// Andere, gesloten bestemming zonder aliassen naar de levende database.
    pub destination: Name,
    /// Geheugenbudget van de coveragebitmap.
    pub page_limit: u32,
}
/// Download/verifieer eerst, publiceer daarna. De callback moet echte SQLite-integriteit
/// controleren. Beide namen zijn canoniek binnen dezelfde Storage; ook sidecars van
/// de levende DB mogen geen bestemming zijn. Geen restore naast onderhoud of SQL.
pub fn fetch<B: Storage, S: Store>(
    b: &mut B,
    store: &mut S,
    options: Fetch<'_>,
    verify: impl FnOnce(&mut B, &Name) -> Result,
) -> Result<restore::Restored> {
    let name = |n: &Name| -> Result<String> {
        let s = n
            .cstr()?
            .to_str()
            .map_err(|_| Error::State)?
            .trim_start_matches('/');
        if s.split('/').any(|s| s.is_empty() || s == "." || s == "..") || s.contains('\\') {
            return Err(Error::State);
        }
        string(s)
    };
    let live = name(&options.live)?;
    let target = name(&options.destination)?;
    let sidecar = |base: &str, other: &str| {
        other.strip_prefix(base).is_some_and(|s| {
            s.starts_with(".replica")
                || s == "-journal"
                || s == "-wal"
                || s == "-shm"
                || s == ".lease"
        })
    };
    // Ook de scratchnaam van de bestemming mag nooit de levende bron zijn.
    if target == live || sidecar(&live, &target) || sidecar(&target, &live) {
        return Err(Error::State);
    }
    let prefix = replication::generation_prefix(options.namespace, options.generation)?;
    let layout = object::layout(store, &prefix)?;
    restore::stage(
        b,
        store,
        &layout,
        options.at,
        options.destination,
        options.page_limit,
    )?
    .verify(b, verify)?
    .publish(b, options.generation)
}
