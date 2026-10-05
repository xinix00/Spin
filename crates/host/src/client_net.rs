//! De uitgaande socket blijft niet-blokkerend, ook tijdens de TLS-handshake.
//! TCP, TLS, ketenverificatie en de Mozilla-wortels zijn van Lean
//! (`leanhttp::host`, `WebDial`); hier staan alleen de wandklok en de
//! entropie van de host, en de fouttekst voor de logregels.
use crate::{net::Connection, storage::Random};
use leanhttp::Dial;
use spin_security::Entropy;
use std::time::{SystemTime, UNIX_EPOCH};

/// Een uitgaande verbinding: kaal voor `http://`, TLS voor `https://`.
pub(crate) type Transport = leanhttps::Link<Connection>;
/// Lean's webdialer op de klok en entropie van de host.
pub(crate) type Web<D> =
    leanhttps::WebDial<D, fn() -> Option<u64>, fn() -> Option<leantls::Entropy>>;
pub(crate) fn web<D: Dial>(inner: D) -> Web<D>
where
    D::Conn: Unpin,
{
    leanhttps::WebDial::new(inner, leantls::MOZILLA_ROOTS, unix_seconds, entropy)
}
fn unix_seconds() -> Option<u64> {
    Some(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs())
}
fn entropy() -> Option<leantls::Entropy> {
    let mut seed = [0; leantls::Entropy::LEN];
    Random::open().ok()?.fill(&mut seed).ok()?;
    Some(leantls::Entropy::new(std::mem::replace(
        &mut seed,
        [0; leantls::Entropy::LEN],
    )))
}
/// Een webverbinding naar `host:port` over de dial van de host: iedere
/// naam opnieuw opgezocht, ieder adres 500 ms, hoogstens zestien adressen.
/// De fout noemt de stap ("resolve: …", "connect ip:port: …", "tls: …").
pub(crate) async fn connect(host: &str, port: u16, encrypted: bool) -> std::io::Result<Transport> {
    let mut dial = crate::outbound::Dial::new();
    let target = leanhttp::Target {
        https: encrypted,
        host,
        port,
    };
    match dial.dial(target).await {
        Ok(transport) => Ok(transport),
        Err(error) => Err(dial.failure(error)),
    }
}

pub(crate) fn error(value: impl std::fmt::Debug) -> std::io::Error {
    match spin_core::validation::text(format_args!("network: {value:?}")) {
        Ok(message) => std::io::Error::other(message),
        Err(_) => std::io::Error::from(std::io::ErrorKind::OutOfMemory),
    }
}
/// Als [`error`], met de leesbare tekst van `value`.
pub(crate) fn reason(value: impl std::fmt::Display) -> std::io::Error {
    match spin_core::validation::text(format_args!("network: {value}")) {
        Ok(message) => std::io::Error::other(message),
        Err(_) => std::io::Error::from(std::io::ErrorKind::OutOfMemory),
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use leanhttp::{AsyncRead, AsyncWrite};
    use spin_store::IdSource;
    use std::{
        io::{BufRead, BufReader},
        net::TcpStream,
        path::PathBuf,
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };
    /// Eén al verbonden socket als dialer: de testnaam bestaat niet in DNS.
    struct Ready(Option<Connection>);
    impl Dial for Ready {
        type Conn = Connection;
        async fn dial(&mut self, _: leanhttp::Target<'_>) -> leanhttp::Result<Connection> {
            self.0.take().ok_or(leanhttp::Error::Connect)
        }
    }
    async fn secure<D: Dial<Conn = Connection>>(
        mut web: leanhttps::WebDial<
            D,
            impl FnMut() -> Option<u64>,
            impl FnMut() -> Option<leantls::Entropy>,
        >,
        host: &str,
        port: u16,
    ) -> std::io::Result<Transport> {
        let target = leanhttp::Target {
            https: true,
            host,
            port,
        };
        web.dial(target)
            .await
            .map_err(|failure| error((failure, web.last_error())))
    }
    struct Peer(Child, PathBuf);
    impl Drop for Peer {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    /// De host-socket (Lean's `TcpConn` op de executor-reactor) onder Lean's
    /// TLS: keten en naam tegen een testwortel, meerdere records heen en terug
    /// over de niet-blokkerende verbinding.
    #[test]
    fn tls_transport_checks_chain_name_and_flushes_multiple_records() {
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../third-party/rust/leantls/testdata");
        let temp =
            std::env::temp_dir().join(Random::open().unwrap().next("spin-tls-test").unwrap());
        std::fs::create_dir(&temp).unwrap();
        let built = Command::new("go")
            .args(["build", "-o"])
            .arg(temp.join("peer"))
            .arg(data.join("goserver/main.go"))
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let child = Command::new(temp.join("peer"))
            .arg("x509ecdsa")
            .arg(data.join("chain"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut peer = Peer(child, temp);
        let mut marker = String::new();
        BufReader::new(peer.0.stdout.take().unwrap())
            .read_line(&mut marker)
            .unwrap();
        let address = marker.split_whitespace().next().unwrap();
        let port = address.rsplit_once(':').unwrap().1.parse().unwrap();
        let root: &'static [u8] =
            Vec::leak(std::fs::read(data.join("chain/ecdsa-root.der")).unwrap());
        let test = |name: &'static str| {
            let raw = crate::net::connection(TcpStream::connect(address).unwrap()).unwrap();
            let web =
                leanhttps::WebDial::new(Ready(Some(raw)), root, || Some(1_790_640_000), entropy);
            secure(web, name, port)
        };
        crate::executor::block_on(async {
            let mut transport = test("leantls.test").await.unwrap();
            let bytes: Vec<u8> = (0..32_768_u32).map(|n| n.to_le_bytes()[0]).collect();
            let until = Instant::now() + Duration::from_secs(10);
            let mut at = 0;
            while at < bytes.len() {
                let n = std::future::poll_fn(|cx| {
                    assert!(Instant::now() < until);
                    transport.poll_write(cx, &bytes[at..])
                })
                .await
                .unwrap();
                assert!(n > 0);
                at += n;
            }
            std::future::poll_fn(|cx| {
                assert!(Instant::now() < until);
                transport.poll_flush(cx)
            })
            .await
            .unwrap();
            let mut actual = vec![0; bytes.len()];
            let mut at = 0;
            while at < actual.len() {
                let n = std::future::poll_fn(|cx| {
                    assert!(Instant::now() < until);
                    transport.poll_read(cx, &mut actual[at..])
                })
                .await
                .unwrap();
                assert!(n > 0);
                at += n;
            }
            assert_eq!(actual, bytes);
            drop(transport);
            assert!(test("wrong.example").await.is_err());
            // De Mozilla-wortels kennen de testwortel niet.
            let raw = crate::net::connection(TcpStream::connect(address).unwrap()).unwrap();
            assert!(
                secure(web(Ready(Some(raw))), "leantls.test", port)
                    .await
                    .is_err()
            );
            // De echte dial noemt de stap die faalde.
            let refused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let closed = refused.local_addr().unwrap().port();
            drop(refused);
            let failure = connect("127.0.0.1", closed, false).await.err().unwrap();
            assert!(
                failure
                    .to_string()
                    .starts_with(&format!("network: connect 127.0.0.1:{closed}:")),
                "{failure}"
            );
            let failure = connect("does-not-exist.invalid", 443, true)
                .await
                .err()
                .unwrap();
            assert!(
                failure.to_string().starts_with("network: resolve:"),
                "{failure}"
            );
        });
    }
}
