//! Advisory databaselease; de host blijft verantwoordelijk voor exclusieve recreate-starts.
use crate::{
    Error, Result,
    local::{self, Name},
    manifest::{hex, json_error},
    string,
    time::Time,
};
use alloc::string::String;
use hop_types::json::{self, Object, Value};
use replica_sqlite::Storage;
/// Geen compare-and-swap: dit protocol detecteert overname, het is geen atomair proceslock.
pub struct Lease {
    path: Name,
    owner: String,
    state: State,
    last: Time,
    expires: Time,
}
#[derive(Clone, Copy)]
enum State {
    Idle,
    Settling(Time),
    Owned,
    Lost,
    Released,
}
/// Het actorwerk dat volgt op een acquisitiepoging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquisition {
    /// De lease is bevestigd; vernieuw uiterlijk iedere vijf seconden.
    Owned,
    /// Geef de executor vrij tot dit tijdstip en probeer opnieuw.
    Wait(Time),
}
fn after(now: Time, seconds: i64) -> Result<Time> {
    Time::unix(
        now.seconds().checked_add(seconds).ok_or(Error::Limit)?,
        now.nanos(),
    )
}
fn read<B: Storage>(b: &mut B, path: &Name) -> Result<Option<(String, Time)>> {
    if !b.exists(path.cstr()?)? {
        return Ok(None);
    }
    let value = json::parse(&local::read(b, path, 4096)?).map_err(json_error)?;
    let o = value.as_object().ok_or(Error::Corrupt)?;
    let owner = o
        .get("owner")
        .and_then(Value::as_str)
        .ok_or(Error::Corrupt)?;
    let expires = Time::parse(
        o.get("expires_at")
            .and_then(Value::as_str)
            .ok_or(Error::Corrupt)?,
    )?;
    if owner.is_empty() || owner.len() > 128 || expires == Time::ZERO {
        return Err(Error::Corrupt);
    }
    Ok(Some((string(owner)?, expires)))
}
impl Lease {
    /// Een verse 96-bit eigenaar-ID zoals Go; mislukte entropie geeft geen lease uit.
    pub fn new<B: Storage>(b: &mut B, database: Name) -> Result<Self> {
        let mut bytes = [0; 32];
        b.random(&mut bytes[..12])?;
        let encoded = hex(&bytes);
        let owner = string(core::str::from_utf8(&encoded[..24]).map_err(|_| Error::State)?)?;
        Ok(Self {
            path: database.suffix(".lease")?,
            owner,
            state: State::Idle,
            last: Time::ZERO,
            expires: Time::ZERO,
        })
    }
    fn write<B: Storage>(&mut self, b: &mut B, now: Time) -> Result {
        let expires = after(now, 30)?;
        let mut value = Object::new();
        value
            .push("owner", Value::String(string(&self.owner)?))
            .map_err(json_error)?;
        value
            .push("expires_at", Value::String(expires.encode()?))
            .map_err(json_error)?;
        let bytes = json::to_string(&Value::Object(value)).map_err(json_error)?;
        local::write(b, &self.path, bytes.as_bytes())?;
        self.last = now;
        self.expires = expires;
        Ok(())
    }
    /// Een gebroken leasebestand is geen bewijs dat de database vrij is.
    pub fn acquire<B: Storage>(&mut self, b: &mut B, now: Time) -> Result<Acquisition> {
        if now.seconds() < 1_577_836_800 {
            return Err(Error::State);
        }
        match self.state {
            State::Owned => {
                self.renew(b, now)?;
                return Ok(Acquisition::Owned);
            }
            State::Lost | State::Released => return Err(Error::LeaseLost),
            State::Settling(until) => {
                if now < until {
                    return Ok(Acquisition::Wait(until));
                }
                if let Some((owner, expires)) = read(b, &self.path)?
                    && owner == self.owner
                    && now < expires
                {
                    self.state = State::Owned;
                    return Ok(Acquisition::Owned);
                }
                self.state = State::Idle;
            }
            State::Idle => {}
        }
        let previous = read(b, &self.path)?;
        if let Some((owner, expires)) = &previous
            && *owner != self.owner
            && now < *expires
        {
            return Ok(Acquisition::Wait((*expires).min(after(now, 5)?)));
        }
        let takeover = previous
            .as_ref()
            .is_some_and(|(owner, _)| *owner != self.owner);
        self.write(b, now)?;
        if takeover {
            let until = after(now, 1)?;
            self.state = State::Settling(until);
            Ok(Acquisition::Wait(until))
        } else {
            self.state = State::Owned;
            Ok(Acquisition::Owned)
        }
    }
    /// Vernieuw op leeftijd. Na elke fout mag de actor geen SQL-writes meer toelaten.
    pub fn renew<B: Storage>(&mut self, b: &mut B, now: Time) -> Result {
        if !matches!(self.state, State::Owned) {
            return Err(Error::LeaseLost);
        }
        if now < self.last || now >= self.expires {
            self.state = State::Lost;
            return Err(Error::LeaseLost);
        }
        if now < after(self.last, 5)? {
            return Ok(());
        }
        let result = (|| {
            let (owner, expires) = read(b, &self.path)?.ok_or(Error::LeaseLost)?;
            if owner != self.owner || now >= expires {
                return Err(Error::LeaseLost);
            }
            self.write(b, now)
        })();
        if result.is_err() {
            self.state = State::Lost;
        }
        result
    }
    /// Verwijdert alleen de eigen lease; een overnemer verliest zijn lease nooit door ons.
    /// Drop doet geen I/O: na crash of afgebroken cleanup verloopt de lease vanzelf.
    pub fn release<B: Storage>(&mut self, b: &mut B) -> Result {
        if matches!(self.state, State::Released) {
            return Ok(());
        }
        self.state = State::Released;
        if let Some((owner, _)) = read(b, &self.path)?
            && owner == self.owner
        {
            b.remove(self.path.cstr()?, true)?;
        }
        Ok(())
    }
}
