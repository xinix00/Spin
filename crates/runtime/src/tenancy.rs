//! Bounded domain table. Each mailbox belongs to exactly one independent Store.
use super::*;
use spin_core::validation::text;
use spin_domain::Wire;
use spin_domain::json::{Object, Value};
/// Maximum simultaneously opened domains on one native server.
pub const TENANTS: usize = 8;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Requested,
    Opening,
    Failed,
}
struct Entry {
    domain: String,
    stage: Stage,
}
/// Host router with lazy opening and independent per-domain connection state.
pub struct Tenants {
    entries: RefCell<[Option<Entry>; TENANTS]>,
    mail: Vec<Mailbox>,
    allowed: Vec<String>,
    single: Option<String>,
}
/// Canonical DNS host, using the original server's host rules.
pub fn normalize_host(host: &str) -> Result<String> {
    let mut host = host.trim();
    if host.starts_with('[') {
        return Err(spin_server::Error::Http(400, "invalid host"));
    }
    if host.bytes().filter(|b| *b == b':').count() == 1 {
        host = host.split_once(':').map_or(host, |(name, _)| name);
    }
    host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty()
        || host.len() > 253
        || host.starts_with(['.', '-'])
        || host.contains("..")
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    {
        return Err(spin_server::Error::Http(400, "invalid host"));
    }
    let mut out = try_string(host)?;
    out.make_ascii_lowercase();
    Ok(out)
}
impl Tenants {
    /// An explicit single database bypasses host isolation, matching SPIN_DATABASE.
    pub fn new(single: Option<&str>, domains: &str) -> Result<Self> {
        let mut allowed = Vec::new();
        for domain in domains
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
        {
            let domain = normalize_host(domain)?;
            if domain.parse::<core::net::Ipv4Addr>().is_ok() {
                return Err(spin_server::Error::Http(
                    500,
                    "SPIN_DOMAINS requires DNS hosts",
                ));
            }
            if !allowed.contains(&domain) {
                if allowed.len() == TENANTS {
                    return Err(spin_server::Error::Http(500, "too many configured domains"));
                }
                allowed.try_reserve(1).map_err(boundary)?;
                allowed.push(domain);
            }
        }
        let mut mail = Vec::new();
        mail.try_reserve_exact(TENANTS).map_err(boundary)?;
        mail.resize_with(TENANTS, Mailbox::default);
        let this = Self {
            entries: RefCell::new(core::array::from_fn(|_| None)),
            mail,
            allowed,
            single: single.map(try_string).transpose()?,
        };
        if let Some(domain) = &this.single {
            this.open(domain)?;
        } else {
            for domain in &this.allowed {
                this.open(domain)?;
            }
        }
        Ok(this)
    }
    /// Discover a previously persisted domain before its next HTTP visit.
    pub fn discover(&self, domain: &str) -> Result {
        let domain = normalize_host(domain)?;
        if self.single.is_none() && self.allowed(&domain) {
            self.open(&domain)?;
        }
        Ok(())
    }
    fn allowed(&self, domain: &str) -> bool {
        self.allowed.is_empty() || self.allowed.iter().any(|s| s == domain)
    }
    fn open(&self, domain: &str) -> Result<usize> {
        let mut entries = self.entries.borrow_mut();
        if let Some(index) = entries
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|e| e.domain == domain))
        {
            return Ok(index);
        }
        let index = entries
            .iter()
            .position(Option::is_none)
            .ok_or(spin_server::Error::Http(503, "tenant capacity reached"))?;
        entries[index] = Some(Entry {
            domain: try_string(domain)?,
            stage: Stage::Requested,
        });
        Ok(index)
    }
    /// Transfer one opening request into the boot shell, never holding a borrow across I/O.
    pub fn pending(&self) -> Result<Option<(usize, String)>> {
        let mut entries = self.entries.borrow_mut();
        for (index, entry) in entries.iter_mut().enumerate() {
            if let Some(entry) = entry
                && entry.stage == Stage::Requested
            {
                let domain = try_string(&entry.domain)?;
                entry.stage = Stage::Opening;
                return Ok(Some((index, domain)));
            }
        }
        Ok(None)
    }
    /// An initialization failure remains visible without exposing storage credentials.
    pub fn failed(&self, index: usize) {
        if let Some(Some(entry)) = self.entries.borrow_mut().get_mut(index) {
            entry.stage = Stage::Failed;
        }
    }
    /// Retry failed initialization after the boot shell's bounded delay.
    pub fn retry(&self, index: usize) {
        if let Some(Some(entry)) = self.entries.borrow_mut().get_mut(index)
            && entry.stage == Stage::Failed
        {
            entry.stage = Stage::Requested;
        }
    }
    /// Stable mailbox address for this tenant's parked task.
    pub fn mailbox(&self, index: usize) -> &Mailbox {
        &self.mail[index]
    }
}
fn json_response(status: u16, body: Value) -> Result<Route<'static>> {
    Ok(Route::Response(Response::json(status, &body)?))
}
fn fields(values: &[(&str, Value)]) -> Result<Value> {
    use spin_domain::TryClone;
    let mut object = Object::default();
    for (key, value) in values {
        object.push(key, value.try_clone()?)?;
    }
    Ok(Value::Object(object))
}
impl Routing for Tenants {
    fn occupied(&self, index: usize) -> bool {
        self.mail.iter().any(|mail| mail.occupied(index))
    }
    fn route(&self, host: &str, path: &str, method: &str) -> Result<Route<'_>> {
        let domain = if let Some(single) = &self.single {
            try_string(single)?
        } else {
            let domain = normalize_host(host)?;
            if domain.parse::<core::net::Ipv4Addr>().is_ok() {
                if path == "/healthz" && matches!(method, "GET" | "HEAD") {
                    return json_response(
                        200,
                        fields(&[
                            ("status", Value::string("ok")?),
                            ("version", Value::string(env!("CARGO_PKG_VERSION"))?),
                            (
                                "tenants",
                                Value::int(self.mail.iter().filter(|m| m.active()).count() as i64),
                            ),
                        ])?,
                    );
                }
                return Err(spin_server::Error::Http(404, "Spin answers on its domains"));
            }
            if !self.allowed(&domain) {
                return Err(spin_server::Error::Http(404, "unknown domain"));
            }
            domain
        };
        let index = self.open(&domain)?;
        let mail = &self.mail[index];
        if mail.active() {
            if path == "/api/opening" {
                return json_response(200, fields(&[("opening", false.to_value()?)])?);
            }
            return Ok(Route::Owner(mail));
        }
        let failed = self.entries.borrow()[index]
            .as_ref()
            .is_some_and(|e| e.stage == Stage::Failed);
        let message = if failed {
            "Openen mislukt; de server probeert het opnieuw"
        } else {
            "Deze Spin wordt geopend"
        };
        let body = fields(&[
            ("opening", (!failed).to_value()?),
            (
                "stage",
                Value::string(if failed { "failed" } else { "opening" })?,
            ),
            ("message", Value::string(message)?),
            ("error", Value::string(message)?),
            ("failure", Value::string(if failed { message } else { "" })?),
            ("started_at", Value::string("")?),
        ])?;
        let mut response =
            if path == "/api/opening" || path.starts_with("/api/") || path == "/healthz" {
                Response::json(if path == "/api/opening" { 200 } else { 503 }, &body)?
            } else {
                let mut response = Response::empty(503)?;
                response.header("Content-Type", "text/html; charset=utf-8")?;
                response.body = text(format_args!("{OPENING_PAGE}"))?.into_bytes();
                response
            };
        response.header("Retry-After", "2")?;
        Ok(Route::Response(response))
    }
}
const OPENING_PAGE: &str = r#"<!doctype html><html lang="nl"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Spin wordt geopend</title><style>body{font:16px system-ui;background:#10151c;color:#e9edf1;display:grid;min-height:90vh;place-content:center}main{max-width:30em;padding:2em;border:1px solid #344050;border-radius:1em}</style><main><h1>Spin</h1><p id="status">Deze Spin wordt geopend…</p></main><script>async function poll(){try{let r=await fetch('/api/opening',{cache:'no-store'});let s=await r.json();if(s.opening===false&&!s.failure){location.reload();return}document.getElementById('status').textContent=s.failure||s.message||'Verbinden…'}catch(e){}setTimeout(poll,2000)}poll()</script></html>"#;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn host_aliases_share_one_owner_but_distinct_domains_never_do() {
        let tenants = Tenants::new(None, "alpha.test,beta.test").unwrap();
        let (alpha, name) = tenants.pending().unwrap().unwrap();
        assert_eq!(name, "alpha.test");
        let (beta, _) = tenants.pending().unwrap().unwrap();
        assert_ne!(alpha, beta);
        tenants.mailbox(alpha).active.set(true);
        tenants.mailbox(beta).active.set(true);
        let Route::Owner(a) = tenants
            .route(" ALPHA.TEST.:443 ", "/api/state", "GET")
            .unwrap()
        else {
            panic!()
        };
        let Route::Owner(b) = tenants.route("beta.test", "/api/state", "GET").unwrap() else {
            panic!()
        };
        assert!(core::ptr::eq(a, tenants.mailbox(alpha)));
        assert!(!core::ptr::eq(a, b));
        a.slots.0.borrow_mut()[9].occupied = true;
        assert!(tenants.occupied(9));
        assert!(!b.occupied(9));
        assert!(tenants.pending().unwrap().is_none());
    }
    #[test]
    fn rejection_and_health_do_not_open_a_database() {
        let tenants = Tenants::new(None, "").unwrap();
        for host in [
            "",
            "evil/a",
            "alpha.test, beta.test",
            "[::1]",
            "a..b",
            "-a",
            "a:80:90",
        ] {
            assert!(tenants.route(host, "/", "GET").is_err(), "{host}");
        }
        assert!(tenants.route("127.0.0.1", "/api/state", "GET").is_err());
        let Route::Response(response) = tenants.route("127.0.0.1", "/healthz", "GET").unwrap()
        else {
            panic!()
        };
        assert_eq!(response.status, 200);
        assert!(tenants.pending().unwrap().is_none());
        let restricted = Tenants::new(None, "allowed.test").unwrap();
        assert!(restricted.route("forbidden.test", "/", "GET").is_err());
    }
    #[test]
    fn opening_capacity_and_failure_are_bounded_and_retryable() {
        let tenants = Tenants::new(None, "").unwrap();
        for index in 0..TENANTS {
            let host = text(format_args!("tenant{index}.test")).unwrap();
            let Route::Response(response) = tenants.route(&host, "/api/state", "GET").unwrap()
            else {
                panic!()
            };
            assert_eq!(response.status, 503);
            assert!(
                response
                    .headers
                    .iter()
                    .any(|(key, value)| key == "CDN-Cache-Control" && value == "no-store")
            );
        }
        assert!(tenants.route("overflow.test", "/", "GET").is_err());
        let (index, domain) = tenants.pending().unwrap().unwrap();
        tenants.failed(index);
        let Route::Response(response) = tenants.route(&domain, "/api/opening", "GET").unwrap()
        else {
            panic!()
        };
        assert!(
            core::str::from_utf8(&response.body)
                .unwrap()
                .contains("\"opening\":false")
        );
        tenants.retry(index);
        assert_eq!(tenants.pending().unwrap().unwrap().0, index);
    }
    #[test]
    fn explicit_single_database_keeps_legacy_any_host_behavior() {
        let tenants = Tenants::new(Some("spin"), "").unwrap();
        let (index, domain) = tenants.pending().unwrap().unwrap();
        assert_eq!(domain, "spin");
        tenants.mailbox(index).active.set(true);
        for host in ["127.0.0.1", "different.test", ""] {
            let Route::Owner(mail) = tenants.route(host, "/api/state", "GET").unwrap() else {
                panic!()
            };
            assert!(core::ptr::eq(mail, tenants.mailbox(index)));
        }
    }
}
