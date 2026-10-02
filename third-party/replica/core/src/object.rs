//! De bestaande objectgrens: atomic replace, sterke GET/LIST-consistentie, één schrijver.
use crate::{
    Error, Result,
    manifest::{Layout, MAX_COMMITS, Manifest},
    reserve, string,
};
use alloc::{string::String, vec::Vec};
/// Fouten blijven herkenbaar; alleen Missing op een committed object bewijst schade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// GET vindt de sleutel niet; DELETE van een ontbrekend object mag slagen.
    Missing,
    /// Verbinding, timeout of onbekende PUT-uitkomst.
    Transport,
    /// Authenticatie/autorisatie; geen bewijs van ontbrekende data.
    Denied,
    /// Begrensde body of listing past niet in het gevraagde budget.
    Limit,
    /// De eigenaar heeft de operatie gestopt.
    Cancelled,
    /// Verplichte configuratie of een betrouwbare klok ontbreekt.
    Configuration,
}
impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Self::Object(e)
    }
}
/// Sleutel en grootte uit een begrensde lijstaanvraag.
pub struct Object {
    /// Volledige sleutel, geen URL.
    pub key: String,
    /// Grootte indien het listingtransport die levert; hashes valideren altijd GET.
    pub size: Option<u64>,
}
/// Een objectstore voor één namespace-eigenaar.
///
/// Implementaties mogen op de private stack async I/O parkeren. Put vervangt
/// atomair; get/list moeten alle bevestigde writes en deletes direct zien.
/// Een fout na PUT is onbekend, nooit automatisch "niet geschreven".
pub trait Store {
    /// Vervangt één volledig object; onderdelen krijgen per poging verse sleutels.
    fn put(&mut self, key: &str, bytes: &[u8]) -> core::result::Result<(), StoreError>;
    /// Weiger bodies boven `limit` vóór ze in hun geheel worden gealloceerd.
    fn get(&mut self, key: &str, limit: usize) -> core::result::Result<Vec<u8>, StoreError>;
    /// Duurzaam verwijderen; afwezig mag succes zijn.
    fn delete(&mut self, key: &str) -> core::result::Result<(), StoreError>;
    /// Alle pagina's onder deze prefix, maximaal `limit` objecten; nooit stil afkappen.
    fn list(&mut self, prefix: &str, limit: usize)
    -> core::result::Result<Vec<Object>, StoreError>;
    /// Immediate child prefixes; remote stores should use delimiter listings.
    fn directories(
        &mut self,
        prefix: &str,
        limit: usize,
    ) -> core::result::Result<Vec<String>, StoreError> {
        let objects = self.list(prefix, MAX_COMMITS)?;
        let mut out = Vec::new();
        for object in objects {
            let rest = object
                .key
                .strip_prefix(prefix)
                .ok_or(StoreError::Transport)?;
            let Some(end) = rest.find('/') else { continue };
            let child = &object.key[..prefix.len() + end + 1];
            if out.iter().any(|s: &String| s == child) {
                continue;
            }
            if out.len() == limit {
                return Err(StoreError::Limit);
            }
            out.try_reserve(1).map_err(|_| StoreError::Limit)?;
            out.push(string(child).map_err(|_| StoreError::Limit)?);
        }
        Ok(out)
    }
    /// First bounded batch; truncation is intentional for resumable deletion.
    fn list_batch(
        &mut self,
        prefix: &str,
        limit: usize,
    ) -> core::result::Result<Vec<Object>, StoreError> {
        if limit == 0 {
            return Err(StoreError::Limit);
        }
        let mut objects = self.list(prefix, MAX_COMMITS)?;
        objects.truncate(limit);
        Ok(objects)
    }
}
/// Begrensd samenvoegen, zodat sleutels niet tijdens publicatie onbegrensd groeien.
pub fn key(prefix: &str, suffix: &str) -> Result<String> {
    let n = prefix.len().checked_add(suffix.len()).ok_or(Error::Limit)?;
    if n > 1024 {
        return Err(Error::Limit);
    }
    let mut out = String::new();
    out.try_reserve_exact(n).map_err(|_| Error::Memory)?;
    out.push_str(prefix);
    out.push_str(suffix);
    Ok(out)
}
/// Laadt uitsluitend manifesten; miljoenen data-objecten horen niet in de layoutlisting.
pub fn layout<S: Store>(store: &mut S, prefix: &str) -> Result<Layout> {
    let snapshot = read_manifest(store, &key(prefix, "snapshot")?, prefix)?;
    let listing = store.list(&key(prefix, "L")?, MAX_COMMITS)?;
    if listing.len() > MAX_COMMITS {
        return Err(Error::Limit);
    }
    let mut commits = Vec::new();
    reserve(&mut commits, listing.len())?;
    let mut seen = Vec::new();
    reserve(&mut seen, listing.len())?;
    for object in &listing {
        let rest = object.key.strip_prefix(prefix).ok_or(Error::Corrupt)?;
        let raw = rest.starts_with("L0/") && rest.ends_with(".json");
        let window = rest.starts_with('L') && rest.ends_with("/complete");
        if !raw && !window {
            continue;
        }
        if object.key.len() > 1024 {
            return Err(Error::Limit);
        }
        seen.push(object.key.as_str());
        let m = read_manifest(store, &object.key, prefix)?;
        if (raw && m.level != 0) || (window && m.level == 0) {
            return Err(Error::Corrupt);
        }
        commits.push(m);
    }
    seen.sort_unstable();
    if seen.windows(2).any(|p| p[0] == p[1]) {
        return Err(Error::Corrupt);
    }
    Layout::new(snapshot, commits, prefix)
}
/// Een onvindbaar gecommit object is beschadiging, een transportfout blijft transport.
pub fn committed<S: Store>(store: &mut S, key: &str, limit: usize) -> Result<Vec<u8>> {
    let bytes = store.get(key, limit).map_err(|e| {
        if e == StoreError::Missing {
            Error::Corrupt
        } else {
            e.into()
        }
    })?;
    if bytes.len() > limit {
        return Err(Error::Limit);
    }
    Ok(bytes)
}
fn read_manifest<S: Store>(store: &mut S, key: &str, prefix: &str) -> Result<Manifest> {
    Manifest::decode(
        &committed(store, key, crate::manifest::MAX_MANIFEST_BYTES)?,
        prefix,
    )
}
/// Publiceert het manifest nadat de aanroeper alle onderdelen heeft bevestigd.
/// Verloren PUT-antwoorden worden alleen met exact gelijk teruggelezen bytes opgelost.
/// De lokale marker moet vóór deze aanroep de onzekere sequence bewaren.
pub fn publish<S: Store>(store: &mut S, key: &str, manifest: &Manifest, prefix: &str) -> Result {
    let bytes = manifest.encode(prefix)?;
    match store.put(key, &bytes) {
        Ok(()) => Ok(()),
        Err(original) => match store.get(key, bytes.len()) {
            Ok(found) if found == bytes => Ok(()),
            _ => Err(original.into()),
        },
    }
}
/// Gevalideerde kopie van een sleutel, bruikbaar bij een aparte publicatiefase.
pub fn owned_key(value: &str) -> Result<String> {
    if value.len() > 1024 || value.is_empty() {
        return Err(Error::Limit);
    }
    string(value)
}
