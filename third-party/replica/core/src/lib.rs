//! Replica's Go-compatibele opslagformaat en herstelcontracten.
#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
extern crate alloc;
pub mod archive;
pub mod capture;
pub mod compact;
pub mod coverage;
pub mod dirty;
pub mod lease;
pub mod local;
pub mod maintenance;
pub mod manifest;
pub mod marker;
pub mod object;
pub mod owner;
pub mod prepare;
pub mod replication;
pub mod restore;
pub mod segment;
pub mod time;
pub mod tracking;
use alloc::{string::String, vec::Vec};
/// Begrensde fouten; transportfouten worden nooit als bewezen corruptie behandeld.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Ongeldig of beschadigd gepubliceerd formaat.
    Corrupt,
    /// Het expliciete geheugen- of objectbudget is overschreden.
    Limit,
    /// Een fallibele allocatie is geweigerd.
    Memory,
    /// Er ontbreekt een aaneengesloten commit of databasepagina.
    Gap,
    /// Een versie zonder atomair commitmanifest.
    Legacy,
    /// Onjuiste lokale levensloop of configuratie.
    State,
    /// De database veranderde buiten de tracking-VFS; een nieuw snapshot is vereist.
    ForeignWrite,
    /// Lokale en remote gegevens hebben geen bewezen gezamenlijke herkomst.
    Unproven,
    /// De databaselease is niet meer bevestigd; de eigenaar moet stoppen met schrijven.
    LeaseLost,
    /// Een lokale SQLite/VFS-fout.
    Storage(replica_sqlite::Error),
    /// Objectopslag: niet verwarren met beschadigde committed bytes.
    Object(object::StoreError),
}
/// Resultaat van een formaat- of opslagbewerking.
pub type Result<T = ()> = core::result::Result<T, Error>;
impl From<replica_sqlite::Error> for Error {
    fn from(e: replica_sqlite::Error) -> Self {
        Self::Storage(e)
    }
}
pub(crate) fn reserve<T>(v: &mut Vec<T>, n: usize) -> Result {
    v.try_reserve_exact(n).map_err(|_| Error::Memory)
}
pub(crate) fn grow<T>(v: &mut Vec<T>, n: usize, limit: usize) -> Result {
    let needed = v
        .len()
        .checked_add(n)
        .filter(|n| *n <= limit)
        .ok_or(Error::Limit)?;
    if needed > v.capacity() {
        let target = needed
            .checked_next_power_of_two()
            .unwrap_or(limit)
            .max(8)
            .min(limit);
        reserve(v, target - v.len())?;
    }
    Ok(())
}
pub(crate) fn string(s: &str) -> Result<String> {
    let mut out = String::new();
    out.try_reserve_exact(s.len()).map_err(|_| Error::Memory)?;
    out.push_str(s);
    Ok(out)
}
pub(crate) fn hash(data: &[u8]) -> [u8; 32] {
    hop_auth::sha256(data)
}
