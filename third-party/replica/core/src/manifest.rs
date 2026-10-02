//! Versie-2 commitmanifest en keuze van een aaneengesloten herstelpad.
use crate::{Error, Result, hash, reserve, segment::Segment, string, time::Time};
use alloc::{string::String, vec::Vec};
use hop_types::json::{self, Object, Value};
/// Bovenlimiet voor onderdelen per manifest.
pub const MAX_PARTS: usize = 32768;
/// Byte budget for large production snapshots, independent of HTTP JSON limits.
pub const MAX_MANIFEST_BYTES: usize = 16 << 20;
/// Bovenlimiet voor manifesten in één geladen generatie.
pub const MAX_COMMITS: usize = 65536;
/// Een object is pas vertrouwd na zowel deze hash als de interne segmenthash.
#[derive(Debug, PartialEq, Eq)]
pub struct Part {
    /// Volledige objectsleutel binnen de data-map van deze generatie.
    pub key: String,
    /// Exacte objectlengte.
    pub size: u64,
    /// SHA-256 van het volledige segment inclusief zijn eigen checksum.
    pub hash: [u8; 32],
}
impl Part {
    /// Controleert de manifestreferentie vóór interpretatie van het segment.
    pub fn read<'a>(&self, bytes: &'a [u8]) -> Result<Segment<'a>> {
        if bytes.len() as u64 != self.size || hash(bytes) != self.hash {
            return Err(Error::Corrupt);
        }
        Segment::decode(bytes)
    }
}
/// Een gepubliceerd manifest maakt alle bijbehorende onderdelen samen zichtbaar.
#[derive(Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Kleinste bestandsgrootte tijdens deze reeks, vóór de onderdelen toepassen.
    pub min_size: u64,
    /// Eerste complete leestransactie in de reeks.
    pub first: u64,
    /// Laatste complete leestransactie in de reeks.
    pub sequence: u64,
    /// Tijd van de laatste transactie.
    pub at: Time,
    /// Nul is raw; hogere niveaus representeren een compactievenster.
    pub level: u32,
    /// Exclusief begin van een compactievenster.
    pub start: Time,
    /// Inclusief einde van een compactievenster.
    pub end: Time,
    /// In deze volgorde toe te passen onderdelen.
    pub parts: Vec<Part>,
}
pub(crate) fn json_error(e: hop_types::Error) -> Error {
    match e {
        hop_types::Error::OutOfMemory => Error::Memory,
        hop_types::Error::TooLarge { .. } | hop_types::Error::TooDeep { .. } => Error::Limit,
        _ => Error::Corrupt,
    }
}
pub(crate) fn field<'a>(o: &'a Object, k: &str) -> Result<&'a Value> {
    o.get(k).ok_or(Error::Corrupt)
}
pub(crate) fn uint(o: &Object, k: &str) -> Result<u64> {
    let v = field(o, k)?.as_u64().ok_or(Error::Corrupt)?;
    if v > i64::MAX as u64 {
        Err(Error::Corrupt)
    } else {
        Ok(v)
    }
}
pub(crate) fn timestamp(o: &Object, k: &str, optional: bool) -> Result<Time> {
    match o.get(k) {
        None | Some(Value::Null) if optional => Ok(Time::ZERO),
        Some(v) => Time::parse(v.as_str().ok_or(Error::Corrupt)?),
        _ => Err(Error::Corrupt),
    }
}
/// Hash in dezelfde kleine hexletters als Go's `sha256hex`.
pub fn hex(hash: &[u8; 32]) -> [u8; 64] {
    let mut out = [0; 64];
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for (pair, b) in out.chunks_exact_mut(2).zip(hash) {
        pair[0] = DIGITS[(b >> 4) as usize];
        pair[1] = DIGITS[(b & 15) as usize];
    }
    out
}
fn unhex(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 {
        return Err(Error::Corrupt);
    }
    let mut out = [0; 32];
    let nibble = |b| match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(Error::Corrupt),
    };
    for (dst, pair) in out.iter_mut().zip(s.as_bytes().chunks_exact(2)) {
        *dst = nibble(pair[0])? * 16 + nibble(pair[1])?;
    }
    Ok(out)
}
impl Manifest {
    /// Controleert de structuur en dat ieder onderdeel bij deze generatie hoort.
    pub fn validate(&self, prefix: &str) -> Result {
        if !prefix.ends_with('/') || prefix.len() > 1024 || prefix.contains("..") {
            return Err(Error::State);
        }
        if self.min_size > i64::MAX as u64
            || self.first == 0
            || self.sequence < self.first
            || self.sequence > i64::MAX as u64
            || self.at == Time::ZERO
            || self.parts.is_empty()
        {
            return Err(Error::Corrupt);
        }
        if self.parts.len() > MAX_PARTS || self.level > 32 {
            return Err(Error::Limit);
        }
        if self.level > 0
            && (self.start == Time::ZERO
                || self.end <= self.start
                || self.at <= self.start
                || self.at > self.end)
        {
            return Err(Error::Corrupt);
        }
        let mut keys = Vec::new();
        reserve(&mut keys, self.parts.len())?;
        for part in &self.parts {
            if part.key.len() > 1024
                || part.key.contains("..")
                || !part
                    .key
                    .strip_prefix(prefix)
                    .is_some_and(|s| s.starts_with("data/") && s.len() > 5)
                || part.size < 56
                || part.size > crate::segment::MAX_BYTES as u64
            {
                return Err(Error::Corrupt);
            }
            keys.push(part.key.as_str());
        }
        keys.sort_unstable();
        if keys.windows(2).any(|p| p[0] == p[1]) {
            return Err(Error::Corrupt);
        }
        Ok(())
    }
    /// Leest het Go-formaat; onbekende velden mogen mee, dubbele sleutels niet.
    pub fn decode(bytes: &[u8], prefix: &str) -> Result<Self> {
        let value = json::parse_with_limit(bytes, MAX_MANIFEST_BYTES).map_err(json_error)?;
        let o = value.as_object().ok_or(Error::Corrupt)?;
        if uint(o, "version")? != 2 {
            return Err(Error::Legacy);
        }
        let array = field(o, "parts")?.as_array().ok_or(Error::Corrupt)?;
        if array.len() > MAX_PARTS {
            return Err(Error::Limit);
        }
        let mut parts = Vec::new();
        reserve(&mut parts, array.len())?;
        for v in array {
            let o = v.as_object().ok_or(Error::Corrupt)?;
            parts.push(Part {
                key: string(field(o, "key")?.as_str().ok_or(Error::Corrupt)?)?,
                size: uint(o, "size")?,
                hash: unhex(field(o, "sha256")?.as_str().ok_or(Error::Corrupt)?)?,
            });
        }
        let m = Self {
            min_size: uint(o, "min_size")?,
            first: uint(o, "first_seq")?,
            sequence: uint(o, "seq")?,
            at: timestamp(o, "at", false)?,
            level: u32::try_from(uint(o, "level")?).map_err(|_| Error::Corrupt)?,
            start: timestamp(o, "start", true)?,
            end: timestamp(o, "end", true)?,
            parts,
        };
        m.validate(prefix)?;
        Ok(m)
    }
    /// Dezelfde veldvolgorde en nul-tijden als `encoding/json` met de Go-struct.
    pub fn encode(&self, prefix: &str) -> Result<Vec<u8>> {
        self.validate(prefix)?;
        let mut o = Object::new();
        for (k, v) in [
            ("min_size", self.min_size),
            ("version", 2),
            ("first_seq", self.first),
            ("seq", self.sequence),
        ] {
            o.push(k, Value::uint(v)).map_err(json_error)?;
        }
        o.push("at", Value::String(self.at.encode()?))
            .map_err(json_error)?;
        o.push("level", Value::uint(u64::from(self.level)))
            .map_err(json_error)?;
        o.push("start", Value::String(self.start.encode()?))
            .map_err(json_error)?;
        o.push("end", Value::String(self.end.encode()?))
            .map_err(json_error)?;
        let mut parts = Vec::new();
        reserve(&mut parts, self.parts.len())?;
        for p in &self.parts {
            let mut o = Object::new();
            o.push("key", Value::String(string(&p.key)?))
                .map_err(json_error)?;
            o.push("size", Value::uint(p.size)).map_err(json_error)?;
            o.push(
                "sha256",
                Value::String(string(
                    core::str::from_utf8(&hex(&p.hash)).map_err(|_| Error::Corrupt)?,
                )?),
            )
            .map_err(json_error)?;
            parts.push(Value::Object(o));
        }
        o.push("parts", Value::Array(parts)).map_err(json_error)?;
        let text = json::to_string(&Value::Object(o)).map_err(json_error)?;
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Limit);
        }
        Ok(text.into_bytes())
    }
}
/// Snapshot en raw/gecompacteerde manifesten; onderdelen zonder manifest tellen niet.
pub struct Layout {
    snapshot: Manifest,
    commits: Vec<Manifest>,
}
impl Layout {
    /// De permanente basis van iedere herstelketen.
    pub fn snapshot(&self) -> &Manifest {
        &self.snapshot
    }

    /// Raw-commits en gepubliceerde vensters, zonder de snapshotbasis.
    pub fn commits(&self) -> &[Manifest] {
        &self.commits
    }
    pub(crate) fn add_window(&mut self, manifest: Manifest, prefix: &str) -> Result {
        manifest.validate(prefix)?;
        if manifest.level == 0 || manifest.first <= 1 {
            return Err(Error::Corrupt);
        }
        crate::grow(&mut self.commits, 1, MAX_COMMITS)?;
        self.commits.push(manifest);
        Ok(())
    }
    /// Hoogste reeds gepubliceerde venstereinde, ook buiten het gekozen herstelpunt.
    pub fn frontier(&self) -> Time {
        self.commits
            .iter()
            .filter(|m| m.level > 0)
            .map(|m| m.end)
            .max()
            .unwrap_or(Time::ZERO)
    }
    /// Controleert manifestrollen, dubbele raw-sequences en de snapshotbasis.
    pub fn new(snapshot: Manifest, commits: Vec<Manifest>, prefix: &str) -> Result<Self> {
        snapshot.validate(prefix)?;
        if snapshot.level != 0 || snapshot.first != 1 || snapshot.sequence != 1 {
            return Err(Error::Corrupt);
        }
        if commits.len() > MAX_COMMITS {
            return Err(Error::Limit);
        }
        let mut raw = Vec::new();
        reserve(&mut raw, commits.len())?;
        for m in &commits {
            m.validate(prefix)?;
            if m.first <= 1 {
                return Err(Error::Corrupt);
            }
            if m.level == 0 {
                if m.first != m.sequence {
                    return Err(Error::Corrupt);
                }
                raw.push(m.sequence);
            }
        }
        raw.sort_unstable();
        if raw.windows(2).any(|s| s[0] == s[1]) {
            return Err(Error::Corrupt);
        }
        Ok(Self { snapshot, commits })
    }
    /// Kiest complete vensters `(start,end]` en raw-commits tot de gevraagde tijd.
    pub fn plan(&self, at: Option<Time>) -> Result<Vec<&Manifest>> {
        if at.is_some_and(|t| t < self.snapshot.at) {
            return Err(Error::Gap);
        }
        let eligible =
            |m: &Manifest| at.is_none_or(|t| if m.level == 0 { m.at <= t } else { m.end <= t });
        let mut eligible_commits = Vec::new();
        reserve(&mut eligible_commits, self.commits.len())?;
        eligible_commits.extend(self.commits.iter().filter(|m| eligible(m)));
        eligible_commits.sort_unstable_by_key(|m| (m.first, m.sequence, m.level));
        let target = eligible_commits
            .iter()
            .map(|m| m.sequence)
            .max()
            .unwrap_or(1);
        let mut plan = Vec::new();
        reserve(&mut plan, eligible_commits.len() + 1)?;
        plan.push(&self.snapshot);
        let mut seq = 1;
        while seq < target {
            // Zoek één aaneengesloten bereik; geen volledige manifests-scan per commit.
            let end = eligible_commits.partition_point(|m| m.first <= seq + 1);
            let best = end
                .checked_sub(1)
                .and_then(|i| eligible_commits.get(i))
                .filter(|m| m.first == seq + 1)
                .ok_or(Error::Gap)?;
            seq = best.sequence;
            plan.push(*best);
        }
        Ok(plan)
    }
}
