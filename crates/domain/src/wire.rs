//! De JSON-grens bezit alleen waarden; alle allocaties kunnen falen.

use crate::{
    Error, Fallible, Map, Name, TryClone,
    json::{self, Value},
    try_push, try_string,
};
use alloc::{string::String, vec::Vec};

/// Hoogstens zoveel elementen in één verzameling van een API-object.
pub const MAX_ITEMS: usize = 100_000;

/// Een domeinwaarde met dezelfde JSON-vorm als de Go-specificatie.
pub trait Wire: Default + Sized {
    /// Valideert bekende velden voor strikte backup-import; dynamische JSON blijft vrij.
    fn check_fields(_value: &Value) -> Fallible {
        Ok(())
    }
    /// Leest een reeds begrensde JSON-waarde.
    fn from_value(value: &Value) -> Fallible<Self>;
    /// Bouwt de publieke JSON-waarde.
    fn to_value(&self) -> Fallible<Value>;
    /// Past een veld toe zoals Go: `null` wist scalars niet, maar wel pointers.
    fn merge_value(&mut self, value: &Value) -> Fallible {
        if !value.is_null() {
            *self = Self::from_value(value)?;
        }
        Ok(())
    }
    /// Of Go's `omitempty` dit veld weglaat.
    fn is_empty(&self) -> bool {
        false
    }
    /// Leest JSON met de limieten van de gedeelde Hop-parser.
    fn from_json(input: &[u8]) -> Fallible<Self> {
        Self::from_value(&json::parse(input)?)
    }
    /// Leest onder het expliciete bytebudget van deze ingang.
    fn from_json_with_limit(input: &[u8], limit: usize) -> Fallible<Self> {
        Self::from_value(&json::parse_with_limit(input, limit)?)
    }
    /// Weigert onbekende velden, ook in geneste structs, vóór de daadwerkelijke import.
    fn from_json_strict(input: &[u8], limit: usize) -> Fallible<Self> {
        let value = json::parse_with_limit(input, limit)?;
        Self::check_fields(&value)?;
        Self::from_value(&value)
    }
    /// Schrijft JSON zonder stille allocatiefouten.
    fn to_json(&self) -> Fallible<String> {
        json::to_string(&self.to_value()?)
    }
}

fn wrong(want: &'static str) -> Error {
    Error::WrongType {
        field: Name::new("value"),
        want,
    }
}

impl Wire for String {
    fn from_value(v: &Value) -> Fallible<Self> {
        try_string(v.as_str().ok_or_else(|| wrong("a string"))?)
    }
    fn to_value(&self) -> Fallible<Value> {
        Value::string(self)
    }
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

impl Wire for bool {
    fn from_value(v: &Value) -> Fallible<Self> {
        v.as_bool().ok_or_else(|| wrong("a boolean"))
    }
    fn to_value(&self) -> Fallible<Value> {
        Ok(Value::Bool(*self))
    }
    fn is_empty(&self) -> bool {
        !self
    }
}

macro_rules! integer {
    ($($ty:ty),*) => {$(
        impl Wire for $ty {
            fn from_value(v: &Value) -> Fallible<Self> {
                let n = v.as_i64().ok_or_else(|| wrong("an integer"))?;
                Self::try_from(n).map_err(|_| Error::OutOfRange { field: Name::new("integer") })
            }
            fn to_value(&self) -> Fallible<Value> { Ok(Value::int(i64::from(*self))) }
            fn is_empty(&self) -> bool { *self == 0 }
        }
    )*};
}
integer!(i64, i32, u16, u8);

impl<T: Wire> Wire for Option<T> {
    fn check_fields(value: &Value) -> Fallible {
        if value.is_null() {
            Ok(())
        } else {
            T::check_fields(value)
        }
    }
    fn from_value(v: &Value) -> Fallible<Self> {
        if v.is_null() {
            Ok(None)
        } else {
            Ok(Some(T::from_value(v)?))
        }
    }
    fn to_value(&self) -> Fallible<Value> {
        match self {
            Some(v) => v.to_value(),
            None => Ok(Value::Null),
        }
    }
    fn is_empty(&self) -> bool {
        self.is_none()
    }
    fn merge_value(&mut self, v: &Value) -> Fallible {
        if v.is_null() {
            *self = None;
        } else {
            self.get_or_insert_with(T::default).merge_value(v)?;
        }
        Ok(())
    }
}

impl Wire for Value {
    fn from_value(v: &Value) -> Fallible<Self> {
        v.try_clone()
    }
    fn to_value(&self) -> Fallible<Value> {
        self.try_clone()
    }
    fn is_empty(&self) -> bool {
        self.is_null()
    }
}

/// Ruwe JSON onderscheidt een afwezig veld van de expliciete waarde `null`.
#[derive(Debug, Default, PartialEq)]
pub struct RawJson(pub Option<Value>);
impl TryClone for RawJson {
    fn try_clone(&self) -> Fallible<Self> {
        Ok(Self(self.0.try_clone()?))
    }
}
impl Wire for RawJson {
    fn from_value(v: &Value) -> Fallible<Self> {
        Ok(Self(Some(v.try_clone()?)))
    }
    fn to_value(&self) -> Fallible<Value> {
        self.0.as_ref().map_or(Ok(Value::Null), TryClone::try_clone)
    }
    fn is_empty(&self) -> bool {
        self.0.is_none()
    }
    fn merge_value(&mut self, v: &Value) -> Fallible {
        *self = Self::from_value(v)?;
        Ok(())
    }
}

/// Een lijst die het verschil tussen Go's `nil` en een lege slice bewaart.
#[derive(Debug, PartialEq)]
pub struct List<T>(Option<Vec<T>>);

impl<T> Default for List<T> {
    fn default() -> Self {
        Self(None)
    }
}
impl<T> List<T> {
    /// Verwijdert elementen zonder de overige volgorde te veranderen.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        if let Some(values) = &mut self.0 {
            values.retain(keep);
        }
    }
    /// Maakt een niet-nulle lege lijst.
    pub const fn new() -> Self {
        Self(Some(Vec::new()))
    }
    /// Leent de elementen, ook wanneer de lijst `null` is.
    pub fn as_slice(&self) -> &[T] {
        self.0.as_deref().unwrap_or_default()
    }
    /// Voegt één element toe binnen het vastgelegde budget.
    pub fn push(&mut self, value: T) -> Fallible {
        if self.len() >= MAX_ITEMS {
            return Err(Error::TooMany {
                field: Name::new("list"),
                max: MAX_ITEMS,
            });
        }
        try_push(self.0.get_or_insert_with(Vec::new), value)
    }
    /// Leent de elementen veranderbaar.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.0.as_deref_mut().unwrap_or_default()
    }
    /// Draagt de elementen over aan de nieuwe eigenaar.
    pub fn into_vec(self) -> Vec<T> {
        self.0.unwrap_or_default()
    }
}
impl<T> core::ops::Deref for List<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}
impl<T: TryClone> TryClone for List<T> {
    fn try_clone(&self) -> Fallible<Self> {
        Ok(Self(self.0.try_clone()?))
    }
}
impl<T: Wire> Wire for List<T> {
    fn check_fields(value: &Value) -> Fallible {
        if value.is_null() {
            return Ok(());
        }
        for item in value.as_array().ok_or_else(|| wrong("an array"))? {
            T::check_fields(item)?;
        }
        Ok(())
    }
    fn from_value(v: &Value) -> Fallible<Self> {
        if v.is_null() {
            return Ok(Self::default());
        }
        let mut out = Self::new();
        for item in v.as_array().ok_or_else(|| wrong("an array"))? {
            out.push(T::from_value(item)?)?;
        }
        Ok(out)
    }
    fn to_value(&self) -> Fallible<Value> {
        let Some(items) = &self.0 else {
            return Ok(Value::Null);
        };
        let mut out = Vec::new();
        out.try_reserve_exact(items.len())
            .map_err(|_| Error::OutOfMemory)?;
        for item in items {
            out.push(item.to_value()?);
        }
        Ok(Value::Array(out))
    }
    fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
    fn merge_value(&mut self, v: &Value) -> Fallible {
        *self = Self::from_value(v)?;
        Ok(())
    }
}

/// Een map met het nulgedrag en de gesorteerde sleutels van Go.
#[derive(Debug, PartialEq)]
pub struct WireMap<T>(Option<Map<T>>);
impl<T> Default for WireMap<T> {
    fn default() -> Self {
        Self(None)
    }
}
impl<T> WireMap<T> {
    /// Een niet-nulle lege map.
    pub const fn new() -> Self {
        Self(Some(Map::new()))
    }
    /// Zoekt een sleutel.
    pub fn get(&self, key: &str) -> Option<&T> {
        self.0.as_ref()?.get(key)
    }
    /// Leent één waarde exclusief.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut T> {
        self.0.as_mut()?.get_mut(key)
    }
    /// Verwijdert één waarde en draagt het eigendom over.
    pub fn remove(&mut self, key: &str) -> Option<T> {
        self.0.as_mut()?.remove(key)
    }
    /// Het aantal sleutels.
    pub fn len(&self) -> usize {
        self.0.as_ref().map_or(0, Map::len)
    }
    /// Of de map geen sleutels bevat.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Loopt alle sleutels in vaste volgorde af.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &T)> {
        self.0.iter().flat_map(Map::iter)
    }
    /// Loopt de waarden onder de exclusieve lening af.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut T)> {
        self.0.iter_mut().flat_map(Map::iter_mut)
    }
    /// Houdt alleen de waarden waarvoor de eigenaar kiest.
    pub fn retain(&mut self, keep: impl FnMut(&str, &mut T) -> bool) {
        if let Some(map) = self.0.as_mut() {
            map.retain(keep);
        }
    }
    /// Voegt een sleutel toe of vervangt zijn waarde.
    pub fn insert(&mut self, key: String, value: T) -> Fallible<Option<T>> {
        let map = self.0.get_or_insert_with(Map::new);
        if map.len() >= MAX_ITEMS && !map.contains_key(&key) {
            return Err(Error::TooMany {
                field: Name::new("map"),
                max: MAX_ITEMS,
            });
        }
        map.insert(key, value)
    }
}
impl<T: TryClone> TryClone for WireMap<T> {
    fn try_clone(&self) -> Fallible<Self> {
        Ok(Self(self.0.try_clone()?))
    }
}
impl<T: Wire> Wire for WireMap<T> {
    fn check_fields(value: &Value) -> Fallible {
        if value.is_null() {
            return Ok(());
        }
        for (_, item) in value.as_object().ok_or_else(|| wrong("an object"))?.iter() {
            T::check_fields(item)?;
        }
        Ok(())
    }
    fn from_value(v: &Value) -> Fallible<Self> {
        if v.is_null() {
            return Ok(Self::default());
        }
        let mut out = Self(Some(Map::new()));
        for (key, value) in v.as_object().ok_or_else(|| wrong("an object"))?.iter() {
            out.insert(try_string(key)?, T::from_value(value)?)?;
        }
        Ok(out)
    }
    fn to_value(&self) -> Fallible<Value> {
        let Some(map) = &self.0 else {
            return Ok(Value::Null);
        };
        let mut out = json::Object::new();
        for (key, value) in map.iter() {
            out.push(key, value.to_value()?)?;
        }
        Ok(Value::Object(out))
    }
    fn is_empty(&self) -> bool {
        self.0.as_ref().is_none_or(Map::is_empty)
    }
    fn merge_value(&mut self, v: &Value) -> Fallible {
        if v.is_null() {
            *self = Self::default();
            return Ok(());
        }
        for (key, value) in v.as_object().ok_or_else(|| wrong("an object"))?.iter() {
            self.insert(try_string(key)?, T::from_value(value)?)?;
        }
        Ok(())
    }
}

/// Tijd op de draad; de oorspronkelijke zone en precisie blijven behouden.
#[derive(Debug, Default, PartialEq)]
pub struct Timestamp(String);
impl Timestamp {
    /// Bouwt een UTC-tijd uit de runtimeklok zonder formatteringsallocaties te verbergen.
    pub fn from_time(time: hop_types::Time) -> Fallible<Self> {
        let mut out = String::new();
        time.write_rfc3339(&mut out)?;
        Ok(Self(out))
    }
    /// Leest de gedeelde numerieke tijd voor vergelijkingen.
    pub fn time(&self) -> Fallible<hop_types::Time> {
        hop_types::Time::parse_rfc3339(self.as_str())
    }
    /// Leent RFC 3339, inclusief de Go-nultijd.
    pub fn as_str(&self) -> &str {
        if self.0.is_empty() {
            "0001-01-01T00:00:00Z"
        } else {
            &self.0
        }
    }
}
impl TryClone for Timestamp {
    fn try_clone(&self) -> Fallible<Self> {
        Ok(Self(try_string(&self.0)?))
    }
}
impl Wire for Timestamp {
    fn from_value(v: &Value) -> Fallible<Self> {
        let out = Self(String::from_value(v)?);
        out.time()?;
        Ok(out)
    }
    fn to_value(&self) -> Fallible<Value> {
        Value::string(self.as_str())
    }
}

/// Binaire data; `encoding/json` schrijft dit als base64, niet als getallen.
#[derive(Debug, Default, PartialEq)]
pub struct Bytes(pub Option<Vec<u8>>);
impl TryClone for Bytes {
    fn try_clone(&self) -> Fallible<Self> {
        Ok(Self(self.0.try_clone()?))
    }
}

macro_rules! model {
    ($(#[$meta:meta])* $name:ident { $($(#[$fieldmeta:meta])* $field:ident : $ty:ty => ($key:literal, $omit:literal)),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Default, PartialEq)]
        pub struct $name { $($(#[$fieldmeta])* pub $field: $ty),* }
        impl crate::Wire for $name {
            fn check_fields(value: &crate::json::Value) -> crate::Fallible {
                if value.is_null() { return Ok(()); }
                let object = value.as_object().ok_or(crate::Error::WrongType { field: crate::Name::new(stringify!($name)), want: "an object" })?;
                for (key, value) in object.iter() {
                    $(if $key != "-" && key.eq_ignore_ascii_case($key) { <$ty as crate::Wire>::check_fields(value)?; continue; })*
                    return Err(crate::Error::Invalid { field: crate::Name::new(key), why: "unknown field" });
                }
                Ok(())
            }
            fn from_value(value: &crate::json::Value) -> crate::Fallible<Self> {
                let mut out = Self::default();
                out.merge_value(value)?;
                Ok(out)
            }
            fn merge_value(&mut self, value: &crate::json::Value) -> crate::Fallible {
                if value.is_null() { return Ok(()); }
                let object = value.as_object().ok_or(crate::Error::WrongType { field: crate::Name::new(stringify!($name)), want: "an object" })?;
                for (key, value) in object.iter() {
                    $(if $key != "-" && key.eq_ignore_ascii_case($key) { crate::Wire::merge_value(&mut self.$field, value)?; })*
                }
                Ok(())
            }
            fn to_value(&self) -> crate::Fallible<crate::json::Value> {
                let mut object = crate::json::Object::new();
                $(if $key != "-" && (!$omit || !crate::Wire::is_empty(&self.$field)) { object.push($key, crate::Wire::to_value(&self.$field)?)?; })*
                Ok(crate::json::Value::Object(object))
            }
        }
        impl crate::TryClone for $name {
            fn try_clone(&self) -> crate::Fallible<Self> { Ok(Self { $($field: crate::TryClone::try_clone(&self.$field)?),* }) }
        }
    };
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

impl Wire for Bytes {
    fn from_value(v: &Value) -> Fallible<Self> {
        if v.is_null() {
            return Ok(Self::default());
        }
        let text = v.as_str().ok_or_else(|| wrong("base64"))?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(text.len())
            .map_err(|_| Error::OutOfMemory)?;
        for c in text.bytes().filter(|c| *c != b'\r' && *c != b'\n') {
            buf.push(c);
        }
        if buf.len() % 4 != 0 {
            return Err(wrong("base64"));
        }
        let mut out = Vec::new();
        out.try_reserve_exact(buf.len() / 4 * 3)
            .map_err(|_| Error::OutOfMemory)?;
        let count = buf.len() / 4;
        for (i, group) in buf.chunks_exact(4).enumerate() {
            let mut n = 0u32;
            let mut pad = 0;
            for &c in group {
                n <<= 6;
                if c == b'=' {
                    pad += 1;
                } else {
                    if pad != 0 {
                        return Err(wrong("base64"));
                    }
                    n |= BASE64
                        .iter()
                        .position(|b| *b == c)
                        .ok_or_else(|| wrong("base64"))? as u32;
                }
            }
            if pad > 2 || (pad != 0 && i + 1 != count) {
                return Err(wrong("base64"));
            }
            out.push((n >> 16) as u8);
            if pad < 2 {
                out.push((n >> 8) as u8);
            }
            if pad < 1 {
                out.push(n as u8);
            }
        }
        Ok(Self(Some(out)))
    }
    fn to_value(&self) -> Fallible<Value> {
        let Some(bytes) = &self.0 else {
            return Ok(Value::Null);
        };
        let mut out = String::new();
        let len = bytes
            .len()
            .div_ceil(3)
            .checked_mul(4)
            .ok_or(Error::OutOfMemory)?;
        out.try_reserve_exact(len).map_err(|_| Error::OutOfMemory)?;
        for group in bytes.chunks(3) {
            let a = u32::from(group.first().copied().unwrap_or_default());
            let b = u32::from(group.get(1).copied().unwrap_or_default());
            let c = u32::from(group.get(2).copied().unwrap_or_default());
            let n = a << 16 | b << 8 | c;
            for shift in [18, 12, 6, 0] {
                let symbol = if (shift == 6 && group.len() < 2) || (shift == 0 && group.len() < 3) {
                    b'='
                } else {
                    BASE64
                        .get(((n >> shift) & 63) as usize)
                        .copied()
                        .ok_or_else(|| wrong("base64 index"))?
                };
                out.push(char::from(symbol));
            }
        }
        Ok(Value::String(out))
    }
    fn is_empty(&self) -> bool {
        self.0.as_ref().is_none_or(Vec::is_empty)
    }
    fn merge_value(&mut self, v: &Value) -> Fallible {
        *self = Self::from_value(v)?;
        Ok(())
    }
}
