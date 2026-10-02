//! De bestaande lokale `.replica`-marker; onzekere commits behouden hun herkomst.
use crate::{
    Error, Result,
    local::{self, Name},
    manifest::{field, hex, json_error, timestamp, uint},
    reserve,
    segment::valid_page_size,
    string,
    time::Time,
    tracking::CleanMarker,
};
use alloc::{string::String, vec::Vec};
use hop_types::json::{self, Object, Value};
use replica_sqlite::Storage;
/// Compleet Go-formaat, inclusief herstel van een onderbroken generatiewissel.
#[derive(Debug, PartialEq, Eq)]
pub struct Marker {
    /// SHA-256 van endpoint, bucket, prefix en domain met nul ertussen.
    pub destination: String,
    /// SQLite-paginamaten, nul vóór het eerste snapshot.
    pub page_size: u32,
    /// Tijd van het laatst bevestigde manifest.
    pub at: Time,
    /// Nieuwe commits moeten ná reeds gesloten compactievensters landen.
    pub sealed_at: Time,
    /// Naam van de huidige generatie.
    pub generation: String,
    /// Laatste bevestigde manifestsequence.
    pub sequence: u64,
    /// Databasegrootte bij dat manifest.
    pub size: u64,
    /// Totaal bevestigde segmentbytes; geen reden voor vervroegde generatiewissel.
    pub bytes: u64,
    /// Deze generatie heeft een gecommit snapshot.
    pub complete: bool,
    /// Sinds het bevestigde manifest zijn geen lokale writes meer over.
    pub clean: bool,
    /// Begintijd; vernieuwing gebeurt alleen op leeftijd.
    pub started_at: Time,
    /// Nul of één vorige marker; Vec maakt allocatie faalbaar zonder unsafe Box.
    pub previous: Vec<Marker>,
    /// Bewijs van de beschadigde generatie, nooit een fallback naar die generatie.
    pub repair_from: String,
    /// Een uitgezonden manifest waarvan de PUT-uitkomst nog moet worden opgehelderd.
    pub uncertain: u64,
}
impl Marker {
    /// Een nog ongepubliceerde generatie; lege generatie is de bootstrapstaat.
    pub fn new(destination: &str, generation: &str, started_at: Time) -> Result<Self> {
        let out = Self {
            destination: string(destination)?,
            generation: string(generation)?,
            page_size: 0,
            at: Time::ZERO,
            sealed_at: Time::ZERO,
            sequence: 0,
            size: 0,
            bytes: 0,
            complete: false,
            clean: false,
            started_at,
            previous: Vec::new(),
            repair_from: String::new(),
            uncertain: 0,
        };
        out.validate(0)?;
        Ok(out)
    }
    fn validate(&self, depth: usize) -> Result {
        if depth > 1 || self.previous.len() > 1 {
            return Err(Error::Limit);
        }
        if self.destination.len() != 64
            || !self
                .destination
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Corrupt);
        }
        if !self.generation.is_empty() {
            generation_time(&self.generation)?;
        }
        if !self.repair_from.is_empty() {
            generation_time(&self.repair_from)?;
        }
        for n in [self.sequence, self.size, self.bytes, self.uncertain] {
            if n > i64::MAX as u64 {
                return Err(Error::Corrupt);
            }
        }
        if self.clean && !self.complete {
            return Err(Error::Corrupt);
        }
        if self.complete
            && (self.generation.is_empty()
                || self.sequence == 0
                || self.at == Time::ZERO
                || !valid_page_size(self.page_size)
                || !self.size.is_multiple_of(u64::from(self.page_size)))
        {
            return Err(Error::Corrupt);
        }
        for old in &self.previous {
            old.validate(depth + 1)?;
            if !old.complete {
                return Err(Error::Corrupt);
            }
        }
        Ok(())
    }
    /// Gevalideerd lezen met dezelfde 64 KiB-grens als Go.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 65536 {
            return Err(Error::Limit);
        }
        let value = json::parse(bytes).map_err(json_error)?;
        Self::from_value(&value, 0)
    }
    fn from_value(value: &Value, depth: usize) -> Result<Self> {
        if depth > 1 {
            return Err(Error::Limit);
        }
        let o = value.as_object().ok_or(Error::Corrupt)?;
        if uint(o, "version")? != 2 {
            return Err(Error::Legacy);
        }
        let text = |k| -> Result<String> { string(field(o, k)?.as_str().ok_or(Error::Corrupt)?) };
        let mut previous = Vec::new();
        if let Some(v) = o.get("previous").filter(|v| !v.is_null()) {
            reserve(&mut previous, 1)?;
            previous.push(Self::from_value(v, depth + 1)?);
        }
        let out = Self {
            destination: text("destination")?,
            page_size: u32::try_from(uint(o, "page_size")?).map_err(|_| Error::Corrupt)?,
            at: timestamp(o, "at", false)?,
            sealed_at: timestamp(o, "sealed_at", true)?,
            generation: text("generation")?,
            sequence: uint(o, "seq")?,
            size: uint(o, "size")?,
            bytes: uint(o, "bytes")?,
            complete: field(o, "complete")?.as_bool().ok_or(Error::Corrupt)?,
            clean: field(o, "clean")?.as_bool().ok_or(Error::Corrupt)?,
            started_at: timestamp(o, "started_at", true)?,
            previous,
            repair_from: match o.get("repair_from") {
                Some(v) => string(v.as_str().ok_or(Error::Corrupt)?)?,
                None => String::new(),
            },
            uncertain: match o.get("uncertain") {
                Some(_) => uint(o, "uncertain")?,
                None => 0,
            },
        };
        out.validate(depth)?;
        Ok(out)
    }
    fn value(&self, depth: usize) -> Result<Value> {
        self.validate(depth)?;
        let mut o = Object::new();
        let push = |o: &mut Object, k, v| o.push(k, v).map_err(json_error);
        push(
            &mut o,
            "destination",
            Value::String(string(&self.destination)?),
        )?;
        push(&mut o, "page_size", Value::uint(u64::from(self.page_size)))?;
        push(&mut o, "version", Value::uint(2))?;
        push(&mut o, "at", Value::String(self.at.encode()?))?;
        push(&mut o, "sealed_at", Value::String(self.sealed_at.encode()?))?;
        push(
            &mut o,
            "generation",
            Value::String(string(&self.generation)?),
        )?;
        for (k, n) in [
            ("seq", self.sequence),
            ("size", self.size),
            ("bytes", self.bytes),
        ] {
            push(&mut o, k, Value::uint(n))?;
        }
        push(&mut o, "complete", Value::Bool(self.complete))?;
        push(&mut o, "clean", Value::Bool(self.clean))?;
        push(
            &mut o,
            "started_at",
            Value::String(self.started_at.encode()?),
        )?;
        if let Some(old) = self.previous.first() {
            push(&mut o, "previous", old.value(depth + 1)?)?;
        }
        if !self.repair_from.is_empty() {
            push(
                &mut o,
                "repair_from",
                Value::String(string(&self.repair_from)?),
            )?;
        }
        if self.uncertain != 0 {
            push(&mut o, "uncertain", Value::uint(self.uncertain))?;
        }
        Ok(Value::Object(o))
    }
    /// Exacte veldvolgorde en optionele velden uit de Go-struct.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let s = json::to_string(&self.value(0)?).map_err(json_error)?;
        if s.len() > 65536 {
            return Err(Error::Limit);
        }
        Ok(s.into_bytes())
    }
    /// Fallibele kopie voor een renewal; geen verborgen allocator-abort.
    pub fn duplicate(&self) -> Result<Self> {
        Self::decode(&self.encode()?)
    }
}
/// Leest de datum uit `20060102T150405Z-<random>`; padcomponenten blijven begrensd.
pub fn generation_time(id: &str) -> Result<Time> {
    let b = id.as_bytes();
    if !(17..=255).contains(&b.len())
        || b[8] != b'T'
        || b[15] != b'Z'
        || b[16] != b'-'
        || id.contains(['/', '\\', ' '])
        || id.contains("..")
        || b.iter().any(|c| c.is_ascii_control())
    {
        return Err(Error::Corrupt);
    }
    let mut stamp = *b"0000-00-00T00:00:00Z";
    for (from, to, n) in [
        (0, 0, 4),
        (4, 5, 2),
        (6, 8, 2),
        (9, 11, 2),
        (11, 14, 2),
        (13, 17, 2),
    ] {
        stamp[to..to + n].copy_from_slice(&b[from..from + n]);
    }
    Time::parse(core::str::from_utf8(&stamp).map_err(|_| Error::Corrupt)?)
}
/// Zelfde generatie-ID als Go, maar mislukte entropie komt expliciet terug.
pub fn new_generation<B: Storage>(b: &mut B, now: Time) -> Result<String> {
    if now.seconds() < 1_577_836_800 {
        return Err(Error::State);
    }
    let text = now.encode()?;
    let bytes = text.as_bytes();
    let mut random = [0u8; 32];
    b.random(&mut random[..16])?;
    let digits = hex(&random);
    let mut out = String::new();
    out.try_reserve_exact(49).map_err(|_| Error::Memory)?;
    for range in [0..4, 5..7, 8..10, 10..13, 14..16, 17..19] {
        out.push_str(core::str::from_utf8(&bytes[range]).map_err(|_| Error::State)?);
    }
    out.push_str("Z-");
    out.push_str(core::str::from_utf8(&digits[..32]).map_err(|_| Error::State)?);
    Ok(out)
}
/// De bestemmingsidentiteit voorkomt dat een clean marker voor een andere bucket geldt.
pub fn destination(endpoint: &str, bucket: &str, prefix: &str, domain: &str) -> Result<String> {
    let mut hash = hop_auth::Sha256::new();
    for (i, s) in [endpoint, bucket, prefix, domain].into_iter().enumerate() {
        if i != 0 {
            hash.update(&[0]);
        }
        hash.update(s.as_bytes());
    }
    string(core::str::from_utf8(&hex(&hash.finish())).map_err(|_| Error::State)?)
}
/// Marker en sidecar-pad horen bij één eigenaar, ook wanneer persist faalt.
pub struct LocalMarker {
    /// De waarheid in geheugen; een fout bij opslaan zet clean altijd terug naar false.
    pub value: Marker,
    path: Name,
}
impl LocalMarker {
    /// Koppelt aan de `.replica` naast de database.
    pub fn new(database: Name, value: Marker) -> Result<Self> {
        Ok(Self {
            value,
            path: database.suffix(".replica")?,
        })
    }
    /// Bewaart een beslissing; onzekere schrijfbevestiging wordt niet als clean behandeld.
    pub fn save<B: Storage>(&mut self, b: &mut B) -> Result {
        let result = self
            .value
            .encode()
            .and_then(|bytes| local::write(b, &self.path, &bytes));
        if result.is_err() {
            self.value.clean = false;
        }
        result
    }
    /// Herlaadt uitsluitend een geldige marker; geen stil vers begin na corruptie.
    pub fn load<B: Storage>(b: &mut B, database: Name) -> Result<Self> {
        let path = database.suffix(".replica")?;
        let value = Marker::decode(&local::read(b, &path, 65536)?)?;
        Ok(Self { value, path })
    }
}
impl<B: Storage> CleanMarker<B> for LocalMarker {
    fn invalidate(&mut self, b: &mut B) -> Result {
        self.value.clean = false;
        self.save(b)
    }
}
