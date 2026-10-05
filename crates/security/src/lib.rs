//! Geheimen in het bestaande Spin-formaat, zonder I/O of globale entropie.
#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
extern crate alloc;
use aes_gcm::{Aes256Gcm, KeyInit, aead::AeadInPlace};
use alloc::{string::String, vec::Vec};
use leancrypto::hmac::HmacSha256;
pub use leancrypto::{ct::eq as constant_time_eq, sha256::Sha256};
use spin_domain::{self as d, Wire, try_push_str, try_string};
use zeroize::Zeroize;

mod state;

/// Het prefix van iedere versleutelde waarde sinds de Go-generatie.
pub const ENCRYPTED_PREFIX: &str = "enc:v1:";
/// De bestaande PBKDF2-werklast voor nieuwe wachtwoorden.
pub const PASSWORD_ITERATIONS: u32 = 600_000;
/// De limiet voor het lezen van een opgeslagen wachtwoordhash.
pub const MAX_PASSWORD_ITERATIONS: u32 = 2_000_000;

/// Falen van een sleutel, bericht, entropiebron of faalbare allocatie.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// De masterkey moet exact 32 bytes zijn.
    KeyLength(usize),
    /// De runtime kon geen cryptografische willekeur leveren.
    Entropy(i32),
    /// Een opgeslagen geheim heeft geen ondersteund formaat.
    Payload,
    /// De sleutel, tag of het doel klopt niet.
    Authentication,
    /// De wachtwoordlengte is buiten het bestaande bereik van 12 tot 256 bytes.
    PasswordLength(usize),
    /// Een parser of allocatie weigerde de invoer.
    Data(d::Error),
}
impl From<d::Error> for Error {
    fn from(e: d::Error) -> Self {
        Self::Data(e)
    }
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::KeyLength(n) => write!(f, "master key must decode to exactly 32 bytes; got {n}"),
            Self::Entropy(code) => write!(f, "entropy unavailable: code={code}"),
            Self::Payload => f.write_str("invalid encrypted secret payload"),
            Self::Authentication => {
                f.write_str("cannot decrypt secret; the master key is missing or incorrect")
            }
            Self::PasswordLength(n) => write!(f, "password must contain 12 to 256 bytes; got {n}"),
            Self::Data(e) => e.fmt(f),
        }
    }
}
impl core::error::Error for Error {}
/// Het resultaat van een beveiligingsbewerking.
pub type Result<T = ()> = core::result::Result<T, Error>;

/// De runtime levert CSPRNG-bytes; bij falen wordt niets versleuteld.
pub trait Entropy {
    /// Vult de hele buffer met nieuwe cryptografische willekeur.
    fn fill(&mut self, bytes: &mut [u8]) -> Result;
}

/// AES-256-GCM met Go's nonce/ciphertext/tag-indeling en doelgebonden AAD.
pub struct Cipher {
    cipher: Aes256Gcm,
    key: [u8; 32],
}
impl Drop for Cipher {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}
impl Cipher {
    /// Neemt één 256-bit masterkey over.
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new(&key.into()),
            key,
        }
    }
    /// Leest een raw of gepadde base64-masterkey.
    pub fn from_encoded(encoded: &str) -> Result<Self> {
        let mut raw = decode_base64(encoded.trim())?;
        let key: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| Error::KeyLength(raw.len()))?;
        raw.zeroize();
        Ok(Self::new(key))
    }
    /// Het portable formaat voor de admin-only backup.
    pub fn portable_key(&self) -> Result<String> {
        encode_base64(&self.key, false)
    }
    /// Een lege waarde blijft leeg; anders wordt een nieuwe nonce gebruikt.
    pub fn encrypt(
        &self,
        value: &str,
        purpose: &str,
        entropy: &mut impl Entropy,
    ) -> Result<String> {
        if value.is_empty() {
            return Ok(String::new());
        }
        let mut nonce = [0; 12];
        entropy.fill(&mut nonce)?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(value.len().checked_add(28).ok_or(d::Error::OutOfMemory)?)
            .map_err(|_| d::Error::OutOfMemory)?;
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(value.as_bytes());
        let body = payload.get_mut(12..).ok_or(Error::Payload)?;
        let tag = self
            .cipher
            .encrypt_in_place_detached(&nonce.into(), purpose.as_bytes(), body)
            .map_err(|_| Error::Authentication)?;
        payload.extend_from_slice(&tag);
        let mut out = try_string(ENCRYPTED_PREFIX)?;
        try_push_str(&mut out, &encode_base64(&payload, false)?)?;
        Ok(out)
    }
    /// Verifieert de tag én het doel voordat het geheim wordt teruggegeven.
    pub fn decrypt(&self, value: &str, purpose: &str) -> Result<String> {
        if value.is_empty() {
            return Ok(String::new());
        }
        let encoded = value.strip_prefix(ENCRYPTED_PREFIX).ok_or(Error::Payload)?;
        let mut payload = decode_base64(encoded)?;
        let split = payload
            .len()
            .checked_sub(16)
            .filter(|n| *n >= 12)
            .ok_or(Error::Payload)?;
        let nonce: [u8; 12] = payload
            .get(..12)
            .ok_or(Error::Payload)?
            .try_into()
            .map_err(|_| Error::Payload)?;
        let tag: [u8; 16] = payload
            .get(split..)
            .ok_or(Error::Payload)?
            .try_into()
            .map_err(|_| Error::Payload)?;
        let body = payload.get_mut(12..split).ok_or(Error::Payload)?;
        self.cipher
            .decrypt_in_place_detached(&nonce.into(), purpose.as_bytes(), body, &tag.into())
            .map_err(|_| Error::Authentication)?;
        let plaintext = core::str::from_utf8(body).map_err(|_| Error::Payload)?;
        let out = try_string(plaintext);
        payload.zeroize();
        Ok(out?)
    }
}

/// SHA-256 in één keer.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes)
}
/// Standaard-base64 (`+/`) op de bestaande draad, met of zonder `=`-padding.
pub fn encode_base64(bytes: &[u8], padded: bool) -> Result<String> {
    let mut out = leanbase64::STANDARD
        .encode(bytes)
        .map_err(|_| d::Error::OutOfMemory)?;
    if !padded {
        while out.ends_with('=') {
            out.pop();
        }
    }
    Ok(out)
}
/// URL-base64 (`-_`, zonder padding): tokens en de PKCE-challenge.
pub fn encode_base64_url(bytes: &[u8]) -> Result<String> {
    Ok(leanbase64::URL
        .encode(bytes)
        .map_err(|_| d::Error::OutOfMemory)?)
}
/// Leest het raw of gepadde standaardalfabet; andere alfabetten zijn ongeldig.
/// Het decoderen is dat van [`d::Bytes`] (leanbase64), met dezelfde fouten.
pub fn decode_base64(encoded: &str) -> Result<Vec<u8>> {
    let mut padded = try_string(encoded)?;
    let count = encoded
        .bytes()
        .filter(|c| *c != b'\r' && *c != b'\n')
        .count();
    if !encoded.contains('=') {
        if count % 4 == 1 {
            return Err(Error::Payload);
        }
        for _ in 0..(4 - count % 4) % 4 {
            try_push_str(&mut padded, "=")?;
        }
    }
    let bytes = d::Bytes::from_value(&d::json::Value::String(padded))?;
    Ok(bytes.0.unwrap_or_default())
}

/// PBKDF2-HMAC-SHA256 als hervatbare CPU-taak, zonder allocatie per ronde.
/// De HMAC is die van leancrypto; de toestand na de sleutel wordt per ronde
/// gekloond, dus elke ronde kost twee compressies, zoals eerder.
pub struct PasswordDeriver {
    mac: HmacSha256,
    value: [u8; 32],
    result: [u8; 32],
    remaining: u32,
}
impl PasswordDeriver {
    /// Bereidt de eerste ronde en de herbruikbare HMAC-sleutel voor.
    pub fn new(password: &[u8], salt: &[u8], iterations: u32) -> Result<Self> {
        if iterations == 0 || iterations > MAX_PASSWORD_ITERATIONS {
            return Err(Error::Payload);
        }
        let mac = HmacSha256::new(password);
        let mut first = mac.clone();
        first.update(salt);
        first.update(&1u32.to_be_bytes());
        let value = first.finish();
        Ok(Self {
            mac,
            value,
            result: value,
            remaining: iterations - 1,
        })
    }
    /// Voert hoogstens `rounds` rondes uit, waarna de executor weer kan lopen.
    pub fn step(&mut self, rounds: u32) -> bool {
        for _ in 0..rounds.min(self.remaining) {
            let mut round = self.mac.clone();
            round.update(&self.value);
            self.value = round.finish();
            for (result, value) in self.result.iter_mut().zip(self.value) {
                *result ^= value;
            }
            self.remaining -= 1;
        }
        self.remaining == 0
    }
    /// Alleen een voltooide afleiding geeft een sleutel af.
    pub fn result(&self) -> Option<[u8; 32]> {
        if self.remaining == 0 {
            Some(self.result)
        } else {
            None
        }
    }
}
impl Drop for PasswordDeriver {
    fn drop(&mut self) {
        self.value.zeroize();
        self.result.zeroize();
    }
}

/// Dezelfde 32-byte PBKDF2-uitvoer als `pbkdf2SHA256` in de Go-server.
pub fn derive_password(password: &[u8], salt: &[u8], iterations: u32) -> Result<[u8; 32]> {
    let mut task = PasswordDeriver::new(password, salt, iterations)?;
    task.step(iterations);
    task.result().ok_or(Error::Payload)
}
/// Maakt een nieuw Go-compatibel wachtwoordhash met zestien verse saltbytes.
pub fn hash_password(password: &str, entropy: &mut impl Entropy) -> Result<String> {
    if !(12..=256).contains(&password.len()) {
        return Err(Error::PasswordLength(password.len()));
    }
    let mut salt = [0; 16];
    entropy.fill(&mut salt)?;
    let mut key = derive_password(password.as_bytes(), &salt, PASSWORD_ITERATIONS)?;
    let mut out = try_string("pbkdf2-sha256$600000$")?;
    try_push_str(&mut out, &encode_base64(&salt, false)?)?;
    try_push_str(&mut out, "$")?;
    try_push_str(&mut out, &encode_base64(&key, false)?)?;
    key.zeroize();
    Ok(out)
}
/// Verifieert een bestaand wachtwoord zonder vergelijking met variabele stoptijd.
pub fn verify_password(password: &str, encoded: &str) -> Result<bool> {
    if password.len() > 256 {
        return Ok(false);
    }
    let mut parts = encoded.split('$');
    if parts.next() != Some("pbkdf2-sha256") {
        return Ok(false);
    }
    let Some(rounds) = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n > 0 && *n <= MAX_PASSWORD_ITERATIONS)
    else {
        return Ok(false);
    };
    let Some(salt) = parts.next() else {
        return Ok(false);
    };
    let Some(expected) = parts.next() else {
        return Ok(false);
    };
    if parts.next().is_some() || salt.contains('=') || expected.contains('=') {
        return Ok(false);
    }
    let (Ok(salt), Ok(expected)) = (decode_base64(salt), decode_base64(expected)) else {
        return Ok(false);
    };
    if expected.len() != 32 {
        return Ok(false);
    }
    let mut actual = derive_password(password.as_bytes(), &salt, rounds)?;
    let matches = constant_time_eq(&actual, &expected);
    actual.zeroize();
    Ok(matches)
}

/// Een hash in lowercase hex, voor tokens, objecten en idempotency.
pub fn digest_hex(bytes: &[u8]) -> Result<String> {
    let mut out = String::new();
    out.try_reserve_exact(64)
        .map_err(|_| d::Error::OutOfMemory)?;
    for byte in sha256(bytes) {
        for nibble in [byte >> 4, byte & 15] {
            out.push(char::from(if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + nibble - 10
            }));
        }
    }
    Ok(out)
}
