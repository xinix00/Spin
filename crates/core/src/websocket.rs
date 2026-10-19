//! RFC 6455-framing met expliciete rollen, berichtlimiet en fragmentlevensloop.
use crate::validation::{invalid, text};
use alloc::{string::String, vec::Vec};
use spin_domain as d;
/// Een ongeldige peer sluit zijn eigen verbinding, nooit de actor.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// Een handshake of frame voldoet niet aan het protocol.
    Protocol,
    /// Het bericht overschrijdt zijn vastgelegde budget.
    TooLarge,
    /// Een tekstbericht of sluitreden is geen UTF-8.
    Utf8,
    /// Een allocatie of encoding faalde.
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
            Self::Protocol => f.write_str("invalid WebSocket frame"),
            Self::TooLarge => f.write_str("WebSocket message exceeds budget"),
            Self::Utf8 => f.write_str("invalid WebSocket UTF-8"),
            Self::Data(e) => e.fmt(f),
        }
    }
}
impl core::error::Error for Error {}
/// Resultaat van een handshake of framebewerking.
pub type Result<T = ()> = core::result::Result<T, Error>;
/// Berekent Sec-WebSocket-Accept voor een canonieke 16-byte clientnonce.
/// SHA-1 wordt uitsluitend gebruikt voor de door RFC 6455 vereiste handshake.
pub fn accept(key: &str) -> Result<String> {
    // leanbase64 decodeert strikt: één tekst per nonce, dus geen omweg terug.
    let canonical = key.len() == 24
        && leanbase64::STANDARD
            .decode(key.as_bytes())
            .is_ok_and(|nonce| nonce.len() == 16);
    if !canonical {
        return Err(Error::Protocol);
    }
    let mut hash = leancrypto::sha1::Sha1::new();
    hash.update(key.as_bytes());
    hash.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    leanbase64::STANDARD
        .encode(&hash.finish())
        .map_err(|_| Error::Data(d::Error::OutOfMemory))
}
/// De lokale rol bepaalt of binnenkomende frames een masker moeten dragen.
#[derive(Clone, Copy)]
pub enum Role {
    /// Een server ontvangt uitsluitend gemaskeerde clientframes.
    Server,
    /// Een client ontvangt uitsluitend ongemaskeerde serverframes.
    Client,
}
/// Eén volledig bericht of controlframe; fragments zijn intern begrensd.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// Eén UTF-8-tekstbericht.
    Text(String),
    /// Eén binair bericht.
    Binary(Vec<u8>),
    /// Ping moet dezelfde payload als pong terugkrijgen.
    Ping(Vec<u8>),
    /// De peer bevestigt zijn bereikbaarheid.
    Pong(Vec<u8>),
    /// Close bevat nul bytes of een geldige code plus UTF-8-reden.
    Close(Vec<u8>),
}
/// De verbinding bezit één invoerbuffer en hoogstens één onvolledig bericht.
pub struct Decoder {
    role: Role,
    limit: usize,
    input: Vec<u8>,
    fragments: Vec<u8>,
    opcode: Option<u8>,
    closed: bool,
}
impl Decoder {
    /// Stelt de volledige berichtgrens vast vóór de eerste byte.
    pub fn new(role: Role, limit: usize) -> Self {
        Self {
            role,
            limit,
            input: Vec::new(),
            fragments: Vec::new(),
            opcode: None,
            closed: false,
        }
    }
    /// Neemt zoveel bytes als in de framebuffer passen; decodeer vóór verder lezen.
    pub fn push(&mut self, input: &[u8]) -> Result<usize> {
        if self.closed {
            return Err(Error::Protocol);
        }
        let room = self
            .limit
            .max(125)
            .checked_add(14)
            .ok_or(Error::TooLarge)?
            .saturating_sub(self.input.len());
        let n = room.min(input.len());
        self.input
            .try_reserve(n)
            .map_err(|_| d::Error::OutOfMemory)?;
        self.input
            .extend_from_slice(input.get(..n).ok_or(Error::Protocol)?);
        Ok(n)
    }
    /// Leest een frame; de aanroeper herhaalt tot geen volledig event meer klaarstaat.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        loop {
            if self.closed {
                return Ok(None);
            }
            let Some((&first, rest)) = self.input.split_first() else {
                return Ok(None);
            };
            let Some(&second) = rest.first() else {
                return Ok(None);
            };
            let fin = first & 0x80 != 0;
            let op = first & 15;
            let masked = second & 0x80 != 0;
            if first & 0x70 != 0
                || !matches!(op, 0 | 1 | 2 | 8 | 9 | 10)
                || masked != matches!(self.role, Role::Server)
                || (op >= 8 && !fin)
            {
                return Err(Error::Protocol);
            }
            let small = second & 127;
            let mut head = 2;
            let size = match small {
                126 => {
                    let Some(bytes) = self.input.get(2..4) else {
                        return Ok(None);
                    };
                    head = 4;
                    let n = usize::from(u16::from_be_bytes(
                        bytes.try_into().map_err(|_| Error::Protocol)?,
                    ));
                    if n < 126 {
                        return Err(Error::Protocol);
                    }
                    n
                }
                127 => {
                    let Some(bytes) = self.input.get(2..10) else {
                        return Ok(None);
                    };
                    head = 10;
                    let n = u64::from_be_bytes(bytes.try_into().map_err(|_| Error::Protocol)?);
                    if n < 65536 || n >> 63 != 0 {
                        return Err(Error::Protocol);
                    }
                    usize::try_from(n).map_err(|_| Error::TooLarge)?
                }
                n => usize::from(n),
            };
            if op >= 8 && size > 125 {
                return Err(Error::Protocol);
            }
            if op < 8 && size > self.limit {
                return Err(Error::TooLarge);
            }
            let mut mask = [0; 4];
            if masked {
                let Some(bytes) = self.input.get(head..head + 4) else {
                    return Ok(None);
                };
                mask.copy_from_slice(bytes);
                head += 4;
            }
            let end = head.checked_add(size).ok_or(Error::TooLarge)?;
            let Some(bytes) = self.input.get(head..end) else {
                return Ok(None);
            };
            let mut body = Vec::new();
            body.try_reserve_exact(size)
                .map_err(|_| d::Error::OutOfMemory)?;
            for (i, byte) in bytes.iter().enumerate() {
                body.push(byte ^ mask[i % 4]);
            }
            self.input.drain(..end);
            match op {
                8 => {
                    check_close(&body)?;
                    self.closed = true;
                    self.fragments.clear();
                    self.opcode = None;
                    return Ok(Some(Event::Close(body)));
                }
                9 => return Ok(Some(Event::Ping(body))),
                10 => return Ok(Some(Event::Pong(body))),
                1 | 2 if self.opcode.is_some() => return Err(Error::Protocol),
                1 | 2 if fin => return message(op, body).map(Some),
                1 | 2 => {
                    self.opcode = Some(op);
                    self.fragments = body;
                }
                0 => {
                    let op = self.opcode.ok_or(Error::Protocol)?;
                    if self
                        .fragments
                        .len()
                        .checked_add(body.len())
                        .is_none_or(|n| n > self.limit)
                    {
                        return Err(Error::TooLarge);
                    }
                    self.fragments
                        .try_reserve(body.len())
                        .map_err(|_| d::Error::OutOfMemory)?;
                    self.fragments.extend_from_slice(&body);
                    if fin {
                        self.opcode = None;
                        return message(op, core::mem::take(&mut self.fragments)).map(Some);
                    }
                }
                _ => return Err(Error::Protocol),
            }
        }
    }
}
fn message(op: u8, body: Vec<u8>) -> Result<Event> {
    if op == 1 {
        Ok(Event::Text(
            String::from_utf8(body).map_err(|_| Error::Utf8)?,
        ))
    } else {
        Ok(Event::Binary(body))
    }
}
fn check_close(body: &[u8]) -> Result {
    if body.is_empty() {
        return Ok(());
    }
    let code = u16::from_be_bytes(
        body.get(..2)
            .ok_or(Error::Protocol)?
            .try_into()
            .map_err(|_| Error::Protocol)?,
    );
    if !matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999) {
        return Err(Error::Protocol);
    }
    core::str::from_utf8(body.get(2..).ok_or(Error::Protocol)?).map_err(|_| Error::Utf8)?;
    Ok(())
}
/// Maakt één volledig frame; clients leveren per frame een verse willekeurige mask.
pub fn encode(op: u8, body: &[u8], mask: Option<[u8; 4]>) -> Result<Vec<u8>> {
    if !matches!(op, 1 | 2 | 8 | 9 | 10) || (op >= 8 && body.len() > 125) {
        return Err(Error::Protocol);
    }
    if op == 1 {
        core::str::from_utf8(body).map_err(|_| Error::Utf8)?;
    }
    if op == 8 {
        check_close(body)?;
    }
    let mut out = Vec::new();
    out.try_reserve_exact(body.len().checked_add(14).ok_or(Error::TooLarge)?)
        .map_err(|_| d::Error::OutOfMemory)?;
    out.push(0x80 | op);
    let flag = if mask.is_some() { 0x80 } else { 0 };
    if body.len() < 126 {
        out.push(flag | u8::try_from(body.len()).map_err(|_| Error::TooLarge)?);
    } else if let Ok(size) = u16::try_from(body.len()) {
        out.push(flag | 126);
        out.extend_from_slice(&size.to_be_bytes());
    } else {
        out.push(flag | 127);
        out.extend_from_slice(
            &u64::try_from(body.len())
                .map_err(|_| Error::TooLarge)?
                .to_be_bytes(),
        );
    }
    let key = mask.unwrap_or_default();
    if mask.is_some() {
        out.extend_from_slice(&key);
    }
    for (i, byte) in body.iter().enumerate() {
        out.push(byte ^ key[i % 4]);
    }
    Ok(out)
}
/// Waar een runner of service de server bereikt; een HTTPS-URL wordt nooit
/// verlaagd naar plaintext of naar een andere identiteit.
#[derive(Debug, PartialEq)]
pub struct Endpoint {
    /// `host[:poort]` zoals in de URL, voor de Host-kop.
    pub authority: String,
    /// De host zonder poort of IPv6-haken.
    pub host: String,
    /// De poort, 443 of 80 als de URL er geen noemt.
    pub port: u16,
    /// Het WebSocket-pad, onder het voorvoegsel van de URL.
    pub path: String,
    /// `https`/`wss`.
    pub encrypted: bool,
}
/// Leest `SPIN_SERVER`; het pad wordt `/api/runner/ws`, onder een eventueel voorvoegsel.
pub fn endpoint(url: &str) -> d::Fallible<Endpoint> {
    let bad = |why| invalid("server", why);
    let (scheme, tail) = url.trim().split_once("://").ok_or(bad("invalid URL"))?;
    let encrypted = match scheme {
        "https" | "wss" => true,
        "http" | "ws" => false,
        _ => return Err(bad("unsupported URL scheme")),
    };
    let tail = tail.split(['?', '#']).next().unwrap_or("");
    let (authority, prefix) = tail.split_once('/').unwrap_or((tail, ""));
    if authority.is_empty()
        || authority.contains('@')
        || tail.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(bad("invalid URL"));
    }
    let (host, port) = if let Some(ipv6) = authority.strip_prefix('[') {
        let (host, suffix) = ipv6.split_once(']').ok_or(bad("invalid IPv6 address"))?;
        let port = if suffix.is_empty() {
            ""
        } else {
            suffix.strip_prefix(':').ok_or(bad("invalid port"))?
        };
        (host, port)
    } else {
        authority.rsplit_once(':').unwrap_or((authority, ""))
    };
    if host.is_empty() {
        return Err(bad("URL has no host"));
    }
    let port = if port.is_empty() {
        if encrypted { 443 } else { 80 }
    } else {
        port.parse::<u16>().map_err(|_| bad("invalid port"))?
    };
    if port == 0 {
        return Err(bad("invalid port"));
    }
    let prefix = prefix.trim_end_matches('/');
    let path = if prefix.is_empty() {
        d::try_string("/api/runner/ws")?
    } else {
        text(format_args!("/{prefix}/api/runner/ws"))?
    };
    Ok(Endpoint {
        authority: d::try_string(authority)?,
        host: d::try_string(host)?,
        port,
        path,
        encrypted,
    })
}
/// De upgrade van een client; `key` is de base64-nonce, `token` de bearer.
pub fn upgrade_request(endpoint: &Endpoint, key: &str, token: &str) -> d::Fallible<String> {
    let unsafe_text = |s: &str| s.chars().any(|c| c.is_control() || c.is_whitespace());
    if unsafe_text(&endpoint.authority)
        || unsafe_text(&endpoint.path)
        || !endpoint.path.starts_with('/')
        || token.is_empty()
        || unsafe_text(token)
    {
        return Err(invalid("server", "invalid endpoint or token"));
    }
    text(format_args!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\nAuthorization: Bearer {token}\r\n\r\n",
        endpoint.path, endpoint.authority
    ))
}
/// Hoe de server op een client-upgrade antwoordde.
#[derive(Debug, PartialEq)]
pub enum Upgrade {
    /// 101 met de juiste accept: de socket is open.
    Accepted,
    /// 401/403: het token is geweigerd; opnieuw proberen helpt niet.
    Denied,
    /// 409: dezelfde identiteit is nog verbonden; die komt vanzelf vrij.
    Busy,
}
/// Toetst de antwoordkop tot en met de lege regel; al het andere is [`Error::Protocol`].
pub fn check_upgrade(head: &[u8], expected: &str) -> Result<Upgrade> {
    let head = core::str::from_utf8(head).map_err(|_| Error::Protocol)?;
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    match first.split_whitespace().nth(1) {
        Some("401" | "403") => return Ok(Upgrade::Denied),
        Some("409") => return Ok(Upgrade::Busy),
        _ => {}
    }
    if !first.starts_with("HTTP/1.1 101 ") && first != "HTTP/1.1 101" {
        return Err(Error::Protocol);
    }
    let (mut upgrade, mut connection, mut accepted) = (false, false, false);
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(Error::Protocol)?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("Sec-WebSocket-Accept") {
            if accepted || value != expected {
                return Err(Error::Protocol);
            }
            accepted = true;
        } else if name.eq_ignore_ascii_case("Upgrade") {
            upgrade |= value.eq_ignore_ascii_case("websocket");
        } else if name.eq_ignore_ascii_case("Connection") {
            connection |= value
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("upgrade"));
        } else if name.eq_ignore_ascii_case("Sec-WebSocket-Extensions")
            || name.eq_ignore_ascii_case("Sec-WebSocket-Protocol")
        {
            return Err(Error::Protocol);
        }
    }
    if accepted && upgrade && connection {
        Ok(Upgrade::Accepted)
    } else {
        Err(Error::Protocol)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoints_keep_scheme_identity_and_prefix() {
        let e = endpoint("https://haas.getspin.app").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.path.as_str(), e.encrypted),
            ("haas.getspin.app", 443, "/api/runner/ws", true)
        );
        let e = endpoint("http://[::1]:8080/spin/").unwrap();
        assert_eq!(
            (
                e.authority.as_str(),
                e.host.as_str(),
                e.port,
                e.path.as_str()
            ),
            ("[::1]:8080", "::1", 8080, "/spin/api/runner/ws")
        );
        for bad in [
            "ftp://x",
            "https://u@x",
            "https://",
            "https://x:0",
            "https://x y",
        ] {
            assert!(endpoint(bad).is_err(), "{bad}");
        }
        assert!(upgrade_request(&e, "k", "tok en").is_err());
    }
    #[test]
    fn upgrade_answers_are_classified() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let ok = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
        let expected = accept(key).unwrap();
        assert_eq!(
            check_upgrade(ok.as_bytes(), &expected),
            Ok(Upgrade::Accepted)
        );
        assert_eq!(
            check_upgrade(b"HTTP/1.1 401 Unauthorized\r\n\r\n", &expected),
            Ok(Upgrade::Denied)
        );
        assert_eq!(
            check_upgrade(b"HTTP/1.1 409 Conflict\r\n\r\n", &expected),
            Ok(Upgrade::Busy)
        );
        assert!(check_upgrade(ok.replace("s3p", "x3p").as_bytes(), &expected).is_err());
        assert!(check_upgrade(b"HTTP/1.1 200 OK\r\n\r\n", &expected).is_err());
    }
    #[test]
    fn rfc_handshake_and_known_masked_frame() {
        assert_eq!(
            accept("dGhlIHNhbXBsZSBub25jZQ==").unwrap(),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert!(accept("not-a-websocket-key").is_err());
        // Niet-canoniek (ongebruikte bits gezet) of geen 16 bytes: geweigerd.
        assert!(accept("dGhlIHNhbXBsZSBub25jZR==").is_err());
        assert!(accept("dGhlIHNhbXBsZSBub25jZQ").is_err());
        assert!(accept("dGhlIHNhbXBsZSBub25jZWE=").is_err());
        let bytes = [
            0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
        ];
        let mut decoder = Decoder::new(Role::Server, 1024);
        for byte in &bytes[..bytes.len() - 1] {
            decoder.push(&[*byte]).unwrap();
            assert_eq!(decoder.next_event().unwrap(), None);
        }
        decoder.push(&bytes[bytes.len() - 1..]).unwrap();
        assert_eq!(
            decoder.next_event().unwrap(),
            Some(Event::Text(String::from("Hello")))
        );
    }
    #[test]
    fn fragmentation_control_utf8_and_canonical_lengths_are_checked() {
        let mut decoder = Decoder::new(Role::Client, 8);
        decoder
            .push(&[0x01, 2, 0xe2, 0x82, 0x89, 1, b'?', 0x80, 1, 0xac])
            .unwrap();
        assert_eq!(
            decoder.next_event().unwrap(),
            Some(Event::Ping(alloc::vec![b'?']))
        );
        assert_eq!(
            decoder.next_event().unwrap(),
            Some(Event::Text(String::from("€")))
        );
        let mut decoder = Decoder::new(Role::Client, 8);
        decoder.push(&[0x82, 126, 0, 1, 0]).unwrap();
        assert_eq!(decoder.next_event(), Err(Error::Protocol));
        let mut decoder = Decoder::new(Role::Server, 8);
        decoder.push(&[0x81, 0]).unwrap();
        assert_eq!(decoder.next_event(), Err(Error::Protocol));
        let mut decoder = Decoder::new(Role::Client, 8);
        decoder.push(&[0x82, 9]).unwrap();
        assert_eq!(decoder.next_event(), Err(Error::TooLarge));
        let mut decoder = Decoder::new(Role::Client, 8);
        decoder.push(&[0x88, 2, 3, 0xed]).unwrap();
        assert_eq!(decoder.next_event(), Err(Error::Protocol));
    }
    #[test]
    fn binary_lengths_round_trip_and_total_fragment_budget_is_enforced() {
        for size in [0, 125, 126, 65535, 65536] {
            let input = alloc::vec![17; size];
            let frame = encode(2, &input, Some([1, 2, 3, 4])).unwrap();
            let mut decoder = Decoder::new(Role::Server, 70000);
            assert_eq!(decoder.push(&frame).unwrap(), frame.len());
            assert_eq!(decoder.next_event().unwrap(), Some(Event::Binary(input)));
        }
        let mut decoder = Decoder::new(Role::Client, 4);
        decoder.push(&[0x02, 3, 1, 2, 3, 0x80, 2, 4, 5]).unwrap();
        assert_eq!(decoder.next_event(), Err(Error::TooLarge));
    }
}
