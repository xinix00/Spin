//! Een DNS-stubresolver: één A-vraag per keer over UDP, en het antwoord
//! streng gelezen.
//!
//! Dit module bezit het draadformaat (RFC 1035 §4): een vraag bouwen
//! ([`encode_query`]) en een antwoord lezen ([`parse_answer`]). De socket,
//! de termijn en de herhaling staan in [`super::Net::resolve`]; hier is
//! alles synchroon en zonder allocatie, zodat elke kromme vorm van een
//! antwoord een host-test is.
//!
//! Waarom zo weinig: een app vraagt een handvol namen (`pool.ntp.org`,
//! `github.com` en de CDN waar een release naartoe stuurt), en een
//! recursieve resolver op het LAN of bij de provider doet het echte werk.
//! Geen cache (een download per uur vraagt niets twee keer kort na elkaar),
//! geen AAAA (het slot-LAN is IPv4), geen TCP-terugval (een A-antwoord past
//! altijd in 512 bytes; een afgekapt antwoord is een fout, geen uitnodiging).
//!
//! Streng lezen is de verdediging van een stubresolver zonder DNSSEC: het
//! id moet kloppen, de vraag in het antwoord moet de onze zijn, en een
//! A-record telt alleen voor de gevraagde naam of een CNAME-doel daarvan.
//! Een vervalst antwoord moet zo id, poort én naam raden.

use core::fmt;

/// De poort van een DNS-server.
pub const PORT: u16 = 53;

/// De grootste DNS-boodschap over UDP zonder EDNS (RFC 1035 §4.2.1).
pub const UDP_MAX: usize = 512;

/// De langste naam in tekst, zonder slotpunt (RFC 1035 §2.3.4: 255 op de
/// draad, min de lengtebytes).
pub const NAME_MAX: usize = 253;

/// De langste label (RFC 1035 §2.3.4).
pub const LABEL_MAX: usize = 63;

/// De grootste vraag: kop, naam op de draad (hoogstens 255) en type plus
/// klasse.
pub const QUERY_MAX: usize = HEADER_LEN + 255 + 4;

/// De vaste kop van een DNS-boodschap.
const HEADER_LEN: usize = 12;

/// Recordtype A: een IPv4-adres.
const TYPE_A: u16 = 1;

/// Recordtype CNAME: een andere naam voor dezelfde host.
const TYPE_CNAME: u16 = 5;

/// Klasse IN.
const CLASS_IN: u16 = 1;

/// QR: dit is een antwoord.
const FLAG_QR: u16 = 0x8000;

/// TC: het antwoord is afgekapt.
const FLAG_TC: u16 = 0x0200;

/// RD: vraag de server om recursie.
const FLAG_RD: u16 = 0x0100;

/// Hoeveel compressiewijzers een naam mag volgen: genoeg voor elk echt
/// antwoord, en een lus van wijzers eindigt hier in plaats van nooit.
const MAX_POINTERS: usize = 16;

/// Hoeveel CNAME-stappen een antwoord mag nemen voor het A-record komt.
const MAX_CNAMES: usize = 8;

/// Waarom een naam geen adres werd.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DnsError {
    /// De app heeft geen DNS-server (geen `DNS` in de env).
    NoServer,
    /// De naam is geen geldige hostnaam (leeg, een label te lang, een
    /// teken dat geen hostnaam is).
    BadName,
    /// Het antwoord is korter dan wat het zelf aankondigt, of de server zet
    /// de TC-vlag.
    Truncated,
    /// Het antwoord hoort bij een andere vraag (id).
    BadId {
        /// Het id van onze vraag.
        want: u16,
        /// Het id in het antwoord.
        got: u16,
    },
    /// Een vraag in plaats van een antwoord (geen QR-vlag).
    NotResponse,
    /// Het antwoord gaat over een andere naam of een ander type.
    Mismatch,
    /// De naam bestaat niet (RCODE 3).
    NxDomain,
    /// De server weigerde of faalde (een andere RCODE dan 0 en 3).
    Rcode(u8),
    /// Geen A-record voor de naam in het antwoord.
    NoAnswer,
    /// Het antwoord is geen geldige DNS-boodschap (een naam die uit de
    /// boodschap wijst, een lus van wijzers, een A-record van 5 bytes).
    Malformed,
    /// Twee pogingen zonder antwoord.
    Timeout {
        /// Hoeveel vragen er verstuurd zijn.
        attempts: u8,
    },
}

impl fmt::Display for DnsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoServer => f.write_str("dns: no server (no DNS in the env)"),
            Self::BadName => f.write_str("dns: not a valid host name"),
            Self::Truncated => f.write_str("dns: truncated answer"),
            Self::BadId { want, got } => write!(f, "dns: answer id {got:#06x}, asked {want:#06x}"),
            Self::NotResponse => f.write_str("dns: a query, not an answer"),
            Self::Mismatch => f.write_str("dns: answer to a different question"),
            Self::NxDomain => f.write_str("dns: no such name (NXDOMAIN)"),
            Self::Rcode(r) => write!(f, "dns: server error rcode={r}"),
            Self::NoAnswer => f.write_str("dns: no A record in the answer"),
            Self::Malformed => f.write_str("dns: malformed answer"),
            Self::Timeout { attempts } => write!(f, "dns: no answer after {attempts} queries"),
        }
    }
}

/// Het resultaat van een DNS-stap.
pub type Result<T, E = DnsError> = core::result::Result<T, E>;

/// De naam zonder slotpunt, als hij een geldige hostnaam is: labels van 1
/// tot 63 tekens uit letters, cijfers, `-` en `_`, samen hoogstens
/// [`NAME_MAX`].
fn checked_name(host: &str) -> Result<&str> {
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.is_empty() || name.len() > NAME_MAX {
        return Err(DnsError::BadName);
    }
    for label in name.split('.') {
        let ok = (1..=LABEL_MAX).contains(&label.len())
            && label
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_');
        if !ok {
            return Err(DnsError::BadName);
        }
    }
    Ok(name)
}

/// Schrijft een A-vraag voor `host` met id `id` in `out`; geeft de lengte.
///
/// `out` moet [`QUERY_MAX`] bytes kunnen dragen; een kortere buffer is
/// [`DnsError::BadName`] niet waard en geeft [`DnsError::Truncated`].
pub fn encode_query(id: u16, host: &str, out: &mut [u8]) -> Result<usize> {
    let name = checked_name(host)?;
    let mut w = Writer { out, at: 0 };
    w.u16(id)?;
    w.u16(FLAG_RD)?;
    w.u16(1)?; // QDCOUNT
    w.u16(0)?;
    w.u16(0)?;
    w.u16(0)?;
    for label in name.split('.') {
        // Een label is hoogstens 63 (checked_name), dus past in een byte.
        w.u8(u8::try_from(label.len()).map_err(|_| DnsError::BadName)?)?;
        w.bytes(label.as_bytes())?;
    }
    w.u8(0)?;
    w.u16(TYPE_A)?;
    w.u16(CLASS_IN)?;
    Ok(w.at)
}

/// Leest het antwoord `msg` op de A-vraag `id` naar `host`: het eerste
/// A-record voor `host` of voor een CNAME-doel daarvan.
pub fn parse_answer(id: u16, host: &str, msg: &[u8]) -> Result<[u8; 4]> {
    let name = checked_name(host)?;
    let mut r = Reader { msg, at: 0 };
    let got = r.u16()?;
    if got != id {
        return Err(DnsError::BadId { want: id, got });
    }
    let flags = r.u16()?;
    if flags & FLAG_QR == 0 {
        return Err(DnsError::NotResponse);
    }
    if flags & FLAG_TC != 0 {
        return Err(DnsError::Truncated);
    }
    let qd = r.u16()?;
    let an = r.u16()?;
    r.u16()?; // NSCOUNT: niet nodig.
    r.u16()?; // ARCOUNT: niet nodig.
    // De vraag moet de onze zijn, ook bij een fout-RCODE: anders is het
    // antwoord niet van ons en zegt zijn RCODE niets.
    if qd != 1 {
        return Err(DnsError::Mismatch);
    }
    let q = r.name()?;
    let (qtype, qclass) = (r.u16()?, r.u16()?);
    if !q.is(name) || qtype != TYPE_A || qclass != CLASS_IN {
        return Err(DnsError::Mismatch);
    }
    match (flags & 0x000f) as u8 {
        0 => {}
        3 => return Err(DnsError::NxDomain),
        rc => return Err(DnsError::Rcode(rc)),
    }
    // De naam waar een A-record nu voor telt: de gevraagde, of het doel van
    // een CNAME daarvan (een CDN antwoordt vaak met een keten).
    let mut want = q;
    let mut cnames = 0;
    for _ in 0..an {
        let owner = r.name()?;
        let (rtype, rclass) = (r.u16()?, r.u16()?);
        r.u32()?; // TTL: geen cache, dus niet nodig.
        let len = usize::from(r.u16()?);
        let data_at = r.at;
        let data = r.take(len)?;
        if rclass != CLASS_IN || !owner.same(&want) {
            continue;
        }
        match rtype {
            TYPE_A => {
                return <[u8; 4]>::try_from(data).map_err(|_| DnsError::Malformed);
            }
            TYPE_CNAME => {
                cnames += 1;
                if cnames > MAX_CNAMES {
                    return Err(DnsError::Malformed);
                }
                want = Reader { msg, at: data_at }.name()?;
            }
            _ => {}
        }
    }
    Err(DnsError::NoAnswer)
}

/// Een schrijver in een vaste buffer.
struct Writer<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl Writer<'_> {
    fn bytes(&mut self, b: &[u8]) -> Result<()> {
        let end = self.at.checked_add(b.len()).ok_or(DnsError::Truncated)?;
        self.out
            .get_mut(self.at..end)
            .ok_or(DnsError::Truncated)?
            .copy_from_slice(b);
        self.at = end;
        Ok(())
    }

    fn u8(&mut self, v: u8) -> Result<()> {
        self.bytes(&[v])
    }

    fn u16(&mut self, v: u16) -> Result<()> {
        self.bytes(&v.to_be_bytes())
    }
}

/// Een lezer over een ontvangen boodschap; elke stap buiten de boodschap is
/// [`DnsError::Truncated`].
struct Reader<'a> {
    msg: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(DnsError::Truncated)?;
        let b = self.msg.get(self.at..end).ok_or(DnsError::Truncated)?;
        self.at = end;
        Ok(b)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?.first().copied().unwrap_or(0))
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([
            b.first().copied().unwrap_or(0),
            b.get(1).copied().unwrap_or(0),
        ]))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok((u32::from(self.u16()?) << 16) | u32::from(self.u16()?))
    }

    /// Leest een naam hier (met compressie, RFC 1035 §4.1.4) en zet de
    /// lezer achter zijn vorm op de draad. De naam zelf blijft een wijzer in
    /// de boodschap: vergelijken loopt de labels opnieuw af.
    fn name(&mut self) -> Result<Name<'a>> {
        let start = self.at;
        // Eerst de vorm op de draad overslaan: labels tot een 0 of een
        // wijzer (die is altijd het einde).
        loop {
            let len = self.u8()?;
            match len & 0xc0 {
                0x00 if len == 0 => break,
                0x00 => {
                    self.take(usize::from(len))?;
                }
                0xc0 => {
                    self.u8()?;
                    break;
                }
                // 0x40 en 0x80 zijn gereserveerd (RFC 6891 schrapte de
                // uitgebreide labels).
                _ => return Err(DnsError::Malformed),
            }
        }
        let n = Name {
            msg: self.msg,
            at: start,
        };
        // Eén keer helemaal lopen: een lus of een wijzer naar buiten is hier
        // een fout, niet later bij het vergelijken.
        let mut total = 0usize;
        for label in n.labels() {
            total = total.saturating_add(label?.len()).saturating_add(1);
            if total > NAME_MAX + 2 {
                return Err(DnsError::Malformed);
            }
        }
        Ok(n)
    }
}

/// Een naam in een ontvangen boodschap: waar hij begint.
#[derive(Copy, Clone)]
struct Name<'a> {
    msg: &'a [u8],
    at: usize,
}

impl<'a> Name<'a> {
    /// De labels, met wijzers gevolgd.
    fn labels(self) -> Labels<'a> {
        Labels {
            msg: self.msg,
            at: self.at,
            jumps: 0,
            done: false,
        }
    }

    /// Of deze naam `text` is (zonder slotpunt), hoofdletterongevoelig.
    fn is(self, text: &str) -> bool {
        let mut want = text.split('.');
        for label in self.labels() {
            match (label, want.next()) {
                (Ok(l), Some(w)) if l.eq_ignore_ascii_case(w.as_bytes()) => {}
                _ => return false,
            }
        }
        want.next().is_none()
    }

    /// Of twee namen in de boodschap gelijk zijn, hoofdletterongevoelig.
    fn same(self, other: &Name<'_>) -> bool {
        let mut b = other.labels();
        for la in self.labels() {
            match (la, b.next()) {
                (Ok(x), Some(Ok(y))) if x.eq_ignore_ascii_case(y) => {}
                _ => return false,
            }
        }
        b.next().is_none()
    }
}

/// De labels van een [`Name`].
struct Labels<'a> {
    msg: &'a [u8],
    at: usize,
    jumps: usize,
    done: bool,
}

impl<'a> Iterator for Labels<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            let Some(&len) = self.msg.get(self.at) else {
                self.done = true;
                return Some(Err(DnsError::Truncated));
            };
            if len & 0xc0 == 0xc0 {
                let lo = self.msg.get(self.at + 1).copied();
                let Some(lo) = lo else {
                    self.done = true;
                    return Some(Err(DnsError::Truncated));
                };
                self.jumps += 1;
                if self.jumps > MAX_POINTERS {
                    self.done = true;
                    return Some(Err(DnsError::Malformed));
                }
                self.at = (usize::from(len & 0x3f) << 8) | usize::from(lo);
                continue;
            }
            if len & 0xc0 != 0 {
                self.done = true;
                return Some(Err(DnsError::Malformed));
            }
            if len == 0 {
                self.done = true;
                return None;
            }
            let start = self.at + 1;
            let end = start + usize::from(len);
            let Some(label) = self.msg.get(start..end) else {
                self.done = true;
                return Some(Err(DnsError::Truncated));
            };
            self.at = end;
            return Some(Ok(label));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Een antwoord zoals een server het stuurt: de vraag teruggekaatst,
    /// en `answers` als (naam-bytes op de draad, type, data).
    pub(crate) fn answer(
        id: u16,
        flags: u16,
        host: &str,
        answers: &[(&[u8], u16, &[u8])],
    ) -> Vec<u8> {
        let mut q = [0u8; QUERY_MAX];
        let n = encode_query(id, host, &mut q).unwrap();
        let mut m = q[..n].to_vec();
        m[2..4].copy_from_slice(&(FLAG_QR | FLAG_RD | flags).to_be_bytes());
        m[6..8].copy_from_slice(&(answers.len() as u16).to_be_bytes());
        for (name, t, data) in answers {
            m.extend_from_slice(name);
            m.extend_from_slice(&t.to_be_bytes());
            m.extend_from_slice(&CLASS_IN.to_be_bytes());
            m.extend_from_slice(&300u32.to_be_bytes());
            m.extend_from_slice(&(data.len() as u16).to_be_bytes());
            m.extend_from_slice(data);
        }
        m
    }

    /// Een wijzer naar de vraagnaam (offset 12).
    pub(crate) const AT_QNAME: &[u8] = &[0xc0, 12];

    #[test]
    fn a_query_is_the_rfc_shape() {
        let mut out = [0u8; QUERY_MAX];
        let n = encode_query(0xbeef, "pool.ntp.org.", &mut out).unwrap();
        assert_eq!(
            &out[..n],
            &[
                0xbe, 0xef, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 4, b'p', b'o', b'o', b'l', 3, b'n',
                b't', b'p', 3, b'o', b'r', b'g', 0, 0, 1, 0, 1
            ][..]
        );
    }

    #[test]
    fn bad_names_are_refused_before_the_wire() {
        let mut out = [0u8; QUERY_MAX];
        let long = "a".repeat(64);
        let huge = ["abcdefgh"; 32].join(".");
        for host in [
            "",
            ".",
            "a..b",
            "exa mple.com",
            long.as_str(),
            huge.as_str(),
        ] {
            assert_eq!(
                encode_query(1, host, &mut out),
                Err(DnsError::BadName),
                "{host:?}"
            );
        }
        let mut small = [0u8; 8];
        assert_eq!(
            encode_query(1, "example.com", &mut small),
            Err(DnsError::Truncated)
        );
    }

    #[test]
    fn a_plain_answer_and_one_behind_a_cname_chain() {
        let m = answer(
            7,
            0,
            "github.com",
            &[(AT_QNAME, TYPE_A, &[140, 82, 121, 4])],
        );
        assert_eq!(parse_answer(7, "GitHub.com", &m), Ok([140, 82, 121, 4]));
        // objects.githubusercontent.com -> cdn.example (CNAME) -> A.
        let mut cname = Vec::new();
        cname.extend_from_slice(&[3, b'c', b'd', b'n', 7]);
        cname.extend_from_slice(b"example");
        cname.push(0);
        let host = "objects.githubusercontent.com";
        let cdn_at = 12 + host.len() + 2 + 4 + 2 + 10;
        let m = answer(
            9,
            0,
            host,
            &[
                (AT_QNAME, TYPE_CNAME, &cname),
                // Een A voor een naam waar niemand om vroeg telt niet.
                (&[1, b'x', 0], TYPE_A, &[6, 6, 6, 6]),
                (&[0xc0, cdn_at as u8], TYPE_A, &[185, 199, 108, 133]),
            ],
        );
        assert_eq!(parse_answer(9, host, &m), Ok([185, 199, 108, 133]));
    }

    #[test]
    fn crooked_answers_are_refused() {
        let good = answer(7, 0, "a.b", &[(AT_QNAME, TYPE_A, &[1, 2, 3, 4])]);
        // Afgekapt: op elke lengte korter dan het geheel.
        for cut in 0..good.len() {
            let r = parse_answer(7, "a.b", &good[..cut]);
            assert!(
                matches!(r, Err(DnsError::Truncated | DnsError::Malformed)),
                "cut {cut}: {r:?}"
            );
        }
        assert_eq!(
            parse_answer(8, "a.b", &good),
            Err(DnsError::BadId { want: 8, got: 7 })
        );
        assert_eq!(parse_answer(7, "a.c", &good), Err(DnsError::Mismatch));
        let tc = answer(7, FLAG_TC, "a.b", &[]);
        assert_eq!(parse_answer(7, "a.b", &tc), Err(DnsError::Truncated));
        let nx = answer(7, 3, "a.b", &[]);
        assert_eq!(parse_answer(7, "a.b", &nx), Err(DnsError::NxDomain));
        let refused = answer(7, 5, "a.b", &[]);
        assert_eq!(parse_answer(7, "a.b", &refused), Err(DnsError::Rcode(5)));
        // Alleen een AAAA: geen A.
        let aaaa = answer(7, 0, "a.b", &[(AT_QNAME, 28, &[0; 16])]);
        assert_eq!(parse_answer(7, "a.b", &aaaa), Err(DnsError::NoAnswer));
        let bad_a = answer(7, 0, "a.b", &[(AT_QNAME, TYPE_A, &[1, 2, 3, 4, 5])]);
        assert_eq!(parse_answer(7, "a.b", &bad_a), Err(DnsError::Malformed));
        // Een vraag is geen antwoord.
        let mut q = [0u8; QUERY_MAX];
        let n = encode_query(7, "a.b", &mut q).unwrap();
        assert_eq!(parse_answer(7, "a.b", &q[..n]), Err(DnsError::NotResponse));
        // Een wijzer naar zichzelf: een lus, geen hang.
        let lus = answer(7, 0, "a.b", &[(&[0xc0, 21], TYPE_A, &[1, 2, 3, 4])]);
        assert!(parse_answer(7, "a.b", &lus).is_err());
        // Een wijzer uit de boodschap.
        let out = answer(7, 0, "a.b", &[(&[0xc0, 0xff], TYPE_A, &[1, 2, 3, 4])]);
        assert!(parse_answer(7, "a.b", &out).is_err());
    }
}
