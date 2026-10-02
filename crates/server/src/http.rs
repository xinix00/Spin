//! Headers en JSON met dezelfde cache- en browsergrenzen als de Go-server.
use crate::{Error, Result};
use alloc::{string::String, vec::Vec};
use spin_domain::{
    self as d, List, Wire,
    json::{Object, Value},
    try_string,
};
/// De JSON-bodygrens; grote snapshots gebruiken aparte gestroomde routes.
pub const MAX_BODY: usize = 1 << 20;
/// Een geleend verzoek; host en peer komen van de HTTP-parser en socket.
pub struct Request<'a> {
    /// HTTP-methode in hoofdletters.
    pub method: &'a str,
    /// Gedecodeerd URL-pad zonder querystring.
    pub path: &'a str,
    /// Ongewijzigde querystring; waarden worden afzonderlijk gedecodeerd.
    pub raw_query: &'a str,
    /// Headers met behoud van herhaalde waarden.
    pub headers: &'a [(&'a str, &'a str)],
    /// De begrensde request-body.
    pub body: &'a [u8],
    /// Het socket-peeradres, zonder onbetrouwbare forwardingheaders.
    pub peer: &'a str,
    /// De listener of expliciete public-URL-configuratie gebruikt HTTPS.
    pub secure: bool,
}
impl Request<'_> {
    /// Eerste geldige querywaarde, met dezelfde plus- en percentdecodering als HTTP.
    pub fn query(&self, key: &str) -> Result<String> {
        for pair in self.raw_query.split('&') {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if decode_query(name)?.as_deref() == Some(key)
                && let Some(value) = decode_query(value)?
            {
                return Ok(value);
            }
        }
        Ok(String::new())
    }
    /// Leest één header zonder onderscheid in hoofdletters.
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map_or("", |(_, value)| value)
    }
    /// Een mutatie met cookieauthenticatie vereist CSRF en origincontrole.
    pub fn is_mutation(&self) -> bool {
        matches!(self.method, "POST" | "PUT" | "PATCH" | "DELETE")
    }
}
/// Volledig antwoord; grote stromen krijgen een eigen transporttaak.
pub struct Response {
    /// HTTP-status.
    pub status: u16,
    /// Headers, inclusief aparte Set-Cookie-regels.
    pub headers: List<(String, String)>,
    /// De body; HEAD wordt door het transport zonder body verzonden.
    pub body: Vec<u8>,
}
impl Response {
    /// Een leeg antwoord met de standaard beveiligings- en no-cacheheaders.
    pub fn empty(status: u16) -> Result<Self> {
        let mut response = Self {
            status,
            headers: List::new(),
            body: Vec::new(),
        };
        for (name, value) in [
            (
                "Cache-Control",
                "no-store, no-cache, must-revalidate, max-age=0",
            ),
            ("CDN-Cache-Control", "no-store"),
            ("Cloudflare-CDN-Cache-Control", "no-store"),
            ("Surrogate-Control", "no-store"),
            ("Pragma", "no-cache"),
            ("Expires", "0"),
            ("X-Content-Type-Options", "nosniff"),
            ("X-Frame-Options", "DENY"),
            ("Referrer-Policy", "no-referrer"),
            (
                "Permissions-Policy",
                "camera=(), microphone=(), geolocation=()",
            ),
            (
                "Content-Security-Policy",
                "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; object-src 'none'; img-src 'self' data:; media-src 'self' data:; font-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; connect-src 'self'",
            ),
        ] {
            response.header(name, value)?;
        }
        Ok(response)
    }
    /// Serialiseert het bestaande JSON-contract zonder tweede bodykopie.
    pub fn json(status: u16, value: &impl Wire) -> Result<Self> {
        let mut response = Self::empty(status)?;
        response.header("Content-Type", "application/json")?;
        response.body = value.to_json()?.into_bytes();
        Ok(response)
    }
    /// Voegt een header toe; CR/LF uit externe tekst kan nooit een nieuwe header vormen.
    pub fn header(&mut self, name: &str, value: &str) -> Result {
        if name.is_empty()
            || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || value.contains(['\r', '\n', '\0'])
        {
            return Err(Error::Http(500, "invalid response header"));
        }
        self.headers.push((try_string(name)?, try_string(value)?))?;
        Ok(())
    }
}
pub(crate) fn object(fields: &[(&str, Value)]) -> Result<Value> {
    use d::TryClone;
    let mut out = Object::default();
    for (key, value) in fields {
        out.push(key, value.try_clone()?)?;
    }
    Ok(Value::Object(out))
}

fn decode_query(value: &str) -> Result<Option<String>> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| d::Error::OutOfMemory)?;
    let bytes = value.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => result.push(b' '),
            b'%' => {
                let Some(pair) = bytes.get(at + 1..at + 3) else {
                    return Ok(None);
                };
                let Some(high) = (pair[0] as char).to_digit(16) else {
                    return Ok(None);
                };
                let Some(low) = (pair[1] as char).to_digit(16) else {
                    return Ok(None);
                };
                result.push((high * 16 + low) as u8);
                at += 2;
            }
            byte => result.push(byte),
        }
        at += 1;
    }
    Ok(String::from_utf8(result).ok())
}
