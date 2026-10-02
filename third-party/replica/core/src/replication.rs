//! Publicatie van één capture; markerbeslissingen blijven duurzaam vóór onbekende PUTs.
use crate::{
    Error, Result,
    capture::Capture,
    local::Name,
    manifest::{Manifest, Part},
    marker::{self, LocalMarker, Marker},
    object::{self, Store, StoreError},
    reserve, string,
    time::Time,
    tracking::Tracking,
};
use alloc::{string::String, vec::Vec};
use core::fmt::Write;
use replica_sqlite::Storage;
/// Een capture blijft aan precies één generatie en voorganger gekoppeld.
pub struct Batch {
    capture: Capture,
    generation: String,
    previous_sequence: u64,
}
/// Prefix van een generatie, gelijk aan Go's `prefix/domain/generations/id/`.
pub fn generation_prefix(namespace: &str, generation: &str) -> Result<String> {
    if namespace.is_empty() || namespace.ends_with('/') || namespace.contains("..") {
        return Err(Error::State);
    }
    marker::generation_time(generation)?;
    object::key(
        &object::key(namespace, "/generations/")?,
        &object::key(generation, "/")?,
    )
}
/// Begint een verse snapshotpoging, met Previous bewaard totdat current bevestigd is.
pub fn renew<B: Storage>(b: &mut B, local: &mut LocalMarker, now: Time) -> Result {
    if local.value.uncertain != 0 {
        return Err(Error::State);
    }
    let generation = marker::new_generation(b, now)?;
    let mut next = Marker::new(&local.value.destination, &generation, now)?;
    next.repair_from = string(&local.value.repair_from)?;
    let old = if local.value.complete {
        Some(&local.value)
    } else {
        local.value.previous.first()
    };
    if let Some(old) = old {
        let mut previous = old.duplicate()?;
        previous.previous.clear();
        reserve(&mut next.previous, 1)?;
        next.previous.push(previous);
    }
    local.value = next;
    local.save(b)
}
impl Batch {
    /// Capture vóór netwerk-I/O; None wanneer broncontrole geen wijzigingen vindt.
    pub fn capture<B: Storage>(
        b: &mut B,
        path: Name,
        tracking: &Tracking,
        local: &LocalMarker,
        segment_bytes: usize,
    ) -> Result<Option<Self>> {
        if local.value.generation.is_empty() || local.value.uncertain != 0 {
            return Err(Error::State);
        }
        if !local.value.complete && local.value.sequence != 0 {
            return Err(Error::State);
        }
        let generation = string(&local.value.generation)?;
        let capture = Capture::read(
            b,
            path,
            tracking,
            !local.value.complete,
            local.value.size,
            segment_bytes,
        )?;
        if capture.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            capture,
            generation,
            previous_sequence: local.value.sequence,
        }))
    }
    fn check(&self, local: &LocalMarker) -> Result {
        if local.value.generation != self.generation
            || local.value.sequence != self.previous_sequence
            || local.value.uncertain != 0
        {
            return Err(Error::State);
        }
        Ok(())
    }
    /// Kiest de generatieprefix en een verse attempt-prefix voor de delen; geen netwerk.
    pub fn stage<B: Storage>(
        &self,
        b: &mut B,
        namespace: &str,
        local: &LocalMarker,
        now: Time,
    ) -> Result<Staged> {
        self.check(local)?;
        let prefix = generation_prefix(namespace, &self.generation)?;
        let attempt = marker::new_generation(b, now)?;
        let data_prefix = object::key(
            &object::key(&prefix, "data/")?,
            &object::key(&attempt, "/")?,
        )?;
        Ok(Staged {
            namespace: string(namespace)?,
            prefix,
            data_prefix,
        })
    }
    /// Uploadt de delen. Dit raakt alleen de spool en de store, dus het mag
    /// buiten de eigenaar lopen terwijl SQL via de VFS doorschrijft: die
    /// pagina's blijven dirty tot een volgende capture.
    pub fn upload<B: Storage, S: Store>(
        &self,
        b: &mut B,
        store: &mut S,
        staged: &Staged,
    ) -> Result<Uploaded> {
        Ok(Uploaded {
            namespace: string(&staged.namespace)?,
            prefix: string(&staged.prefix)?,
            parts: self.capture.upload(b, store, &staged.data_prefix)?,
        })
    }
    /// Manifest laatst, met de marker duurzaam vóór die onbekende PUT, en
    /// daarna de bevestiging van de tracking. Kort, en op de eigenaar.
    pub fn finish<B: Storage, S: Store>(
        self,
        b: &mut B,
        store: &mut S,
        uploaded: Uploaded,
        local: &mut LocalMarker,
        tracking: &mut Tracking,
        now: Time,
    ) -> Result {
        self.check(local)?;
        let Uploaded {
            namespace,
            prefix,
            parts,
        } = uploaded;
        let seq = self
            .previous_sequence
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or(Error::Limit)?;
        let at = now
            .max(local.value.at.next()?)
            .max(local.value.sealed_at.next()?);
        let manifest = Manifest {
            min_size: self.capture.size,
            first: seq,
            sequence: seq,
            at,
            level: 0,
            start: Time::ZERO,
            end: Time::ZERO,
            parts,
        };
        let key = if self.capture.snapshot {
            object::key(&prefix, "snapshot")?
        } else {
            raw_key(&prefix, seq, at)?
        };
        let added = manifest
            .parts
            .iter()
            .try_fold(0u64, |n, p| n.checked_add(p.size).ok_or(Error::Limit))?;
        let bytes = local
            .value
            .bytes
            .checked_add(added)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or(Error::Limit)?;
        // Vóór de PUT: een crash na serveracceptatie mag de sequence nooit vrijgeven.
        local.value.uncertain = seq;
        local.value.clean = false;
        local.save(b)?;
        object::publish(store, &key, &manifest, &prefix)?;
        if self.capture.snapshot {
            let current = object::key(&namespace, "/current")?;
            if let Err(original) = store.put(&current, self.generation.as_bytes()) {
                match store.get(&current, 255) {
                    Ok(found) if found == self.generation.as_bytes() => {}
                    _ => return Err(original.into()),
                }
            }
        }
        local.value.sequence = seq;
        local.value.at = at;
        local.value.size = self.capture.size;
        local.value.page_size = self.capture.page_size;
        local.value.bytes = bytes;
        local.value.complete = true;
        local.value.uncertain = 0;
        local.value.previous.clear();
        local.value.repair_from.clear();
        local.save(b)?;
        tracking.source_counter = Some(self.capture.counter);
        if self.capture.witness.is_some() {
            tracking.witness = self.capture.witness;
        }
        let clean = tracking.acknowledge(
            b,
            &self.generation,
            seq,
            self.capture.revision,
            self.capture.snapshot,
        )?;
        local.value.clean = clean;
        local.save(b)
    }
    /// Onderdelen eerst, manifest laatst, in één beurt op de eigenaar: stage,
    /// upload en finish. SQL mag tussen capture en deze call schrijven; die
    /// pagina's blijven dirty.
    pub fn publish<B: Storage, S: Store>(
        self,
        b: &mut B,
        store: &mut S,
        namespace: &str,
        local: &mut LocalMarker,
        tracking: &mut Tracking,
        now: Time,
    ) -> Result {
        let staged = self.stage(b, namespace, local, now)?;
        let uploaded = self.upload(b, store, &staged)?;
        self.finish(b, store, uploaded, local, tracking, now)
    }
}
/// De objectsleutels van één publicatiepoging, gekozen vóór de upload van de delen.
pub struct Staged {
    namespace: String,
    prefix: String,
    data_prefix: String,
}
/// De geüploade delen van één capture; het manifest volgt in [`Batch::finish`].
pub struct Uploaded {
    namespace: String,
    prefix: String,
    parts: Vec<Part>,
}
/// Bestaande raw-keyvorm; tijden buiten Go's veilige UnixNano-bereik worden niet gepubliceerd.
pub fn raw_key(prefix: &str, sequence: u64, at: Time) -> Result<String> {
    let ns = at
        .seconds()
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(i64::from(at.nanos())))
        .ok_or(Error::Limit)?;
    if ns < 0 || sequence > i64::MAX as u64 {
        return Err(Error::Limit);
    }
    let mut suffix = String::new();
    suffix.try_reserve_exact(64).map_err(|_| Error::Memory)?;
    // Alle velden samen zijn hoogstens 47 bytes; deze fmt-write kan niet groeien.
    write!(&mut suffix, "L0/{sequence:012}-{ns:020}.json").map_err(|_| Error::Memory)?;
    object::key(prefix, &suffix)
}
/// Lost de onzekere sequence op vóór een nieuwe capture. Een transportfout laat
/// de marker onzeker; alleen een volledige listing mag 'niet geland' bewijzen.
pub fn resolve<B: Storage, S: Store>(
    b: &mut B,
    store: &mut S,
    namespace: &str,
    local: &mut LocalMarker,
) -> Result<bool> {
    let seq = local.value.uncertain;
    if seq == 0 {
        return Ok(false);
    }
    let prefix = generation_prefix(namespace, &local.value.generation)?;
    let key = if !local.value.complete {
        let current = object::key(namespace, "/current")?;
        match store.get(&current, 255) {
            Ok(bytes) if bytes == local.value.generation.as_bytes() => {
                Some(object::key(&prefix, "snapshot")?)
            }
            Ok(bytes) => {
                let current = core::str::from_utf8(&bytes)
                    .map_err(|_| Error::Corrupt)?
                    .trim();
                if local
                    .value
                    .previous
                    .first()
                    .is_some_and(|p| p.generation == current)
                {
                    None
                } else {
                    return Err(Error::State);
                }
            }
            Err(StoreError::Missing) => None,
            Err(e) => return Err(e.into()),
        }
    } else {
        let mut suffix = String::new();
        suffix.try_reserve_exact(32).map_err(|_| Error::Memory)?;
        write!(&mut suffix, "L0/{seq:012}-").map_err(|_| Error::Memory)?;
        let query = object::key(&prefix, &suffix)?;
        let list = store.list(&query, 2)?;
        if list.len() > 1 {
            return Err(Error::Corrupt);
        }
        match list.into_iter().next() {
            Some(o) if o.key.starts_with(&query) && o.key.ends_with(".json") => Some(o.key),
            Some(_) => return Err(Error::Corrupt),
            None => None,
        }
    };
    let landed = key.is_some();
    if let Some(key) = key {
        let manifest = Manifest::decode(
            &object::committed(store, &key, hop_types::json::MAX_INPUT)?,
            &prefix,
        )?;
        if manifest.level != 0 || manifest.first != seq || manifest.sequence != seq {
            return Err(Error::Corrupt);
        }
        let last = manifest.parts.last().ok_or(Error::Corrupt)?;
        let bytes = object::committed(store, &last.key, last.size as usize)?;
        let segment = last.read(&bytes)?;
        let bytes = manifest.parts.iter().try_fold(local.value.bytes, |n, p| {
            n.checked_add(p.size)
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or(Error::Limit)
        })?;
        local.value.sequence = seq;
        local.value.at = local.value.at.max(manifest.at);
        local.value.size = segment.database_size();
        local.value.page_size = segment.page_size();
        local.value.complete = true;
        local.value.previous.clear();
        local.value.repair_from.clear();
        local.value.bytes = bytes;
    }
    local.value.uncertain = 0;
    local.value.clean = false;
    local.save(b)?;
    Ok(landed)
}
