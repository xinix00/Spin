//! De uitgaande socket blijft niet-blokkerend, ook tijdens de TLS-handshake.
use crate::{net::Connection, storage::Random};
use leanhttp::{AsyncRead, AsyncWrite, Dial, IoError};
use spin_security::Entropy;
use std::{
    net::TcpStream,
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Mozilla NSS-wortels uit lean v3.1.1 (db6724745a2c579382c54a68c73c18d643d40038),
// leantls/testdata/github/mozilla-roots.der. Dezelfde set als Hop hostnet.
const ROOTS: &[u8] = include_bytes!("roots.der");
#[allow(clippy::large_enum_variant)] // Eén verbinding bezit haar TLS-staat; geen extra allocatie op iedere dial.
pub(crate) enum Transport {
    Plain(Connection),
    Tls(leanhttps::TlsConn<Connection>),
}
impl Transport {
    pub(crate) async fn connect(
        socket: TcpStream,
        host: &str,
        port: u16,
        encrypted: bool,
    ) -> std::io::Result<Self> {
        let raw = Connection::new(socket)?;
        if !encrypted {
            return Ok(Self::Plain(raw));
        }
        let roots = leantls::Roots::from_concatenated_der(ROOTS).map_err(error)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(error)?
            .as_secs();
        Self::secure(raw, host, port, roots, now).await
    }
    async fn secure(
        mut raw: Connection,
        host: &str,
        port: u16,
        roots: leantls::Roots<'_>,
        now: u64,
    ) -> std::io::Result<Self> {
        raw.set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(error)?;
        raw.set_write_timeout(Some(Duration::from_secs(20)))
            .map_err(error)?;
        let verifier = leantls::ChainVerifier::new(roots, now);
        let mut seed = [0; leantls::Entropy::LEN];
        Random::open()?.fill(&mut seed).map_err(error)?;
        let mut tls = leanhttps::TlsDial::new(
            Ready(Some(raw)),
            leantls::Trust::Chain(&verifier),
            move || {
                // Ready accepteert één dial; de entropie wordt precies één keer verbruikt.
                leantls::Entropy::new(std::mem::replace(&mut seed, [0; leantls::Entropy::LEN]))
            },
        );
        let mut socket = match tls
            .dial(leanhttp::Target {
                https: true,
                host,
                port,
            })
            .await
        {
            Ok(socket) => socket,
            Err(failure) => return Err(error((failure, tls.last_error()))),
        };
        socket.set_read_timeout(None).map_err(error)?;
        socket.set_write_timeout(None).map_err(error)?;
        Ok(Self::Tls(socket))
    }
    pub(crate) fn read(
        &mut self,
        cx: &mut Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<Result<usize, IoError>> {
        match self {
            Self::Plain(raw) => raw.poll_read(cx, bytes),
            Self::Tls(tls) => tls.poll_read(cx, bytes),
        }
    }
    pub(crate) fn write(
        &mut self,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize, IoError>> {
        match self {
            Self::Plain(raw) => raw.poll_write(cx, bytes),
            Self::Tls(tls) => tls.poll_write(cx, bytes),
        }
    }
    pub(crate) fn flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        match self {
            Self::Plain(raw) => raw.poll_flush(cx),
            Self::Tls(tls) => tls.poll_flush(cx),
        }
    }
}
impl AsyncRead for Transport {
    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<Result<usize, IoError>> {
        self.read(cx, bytes)
    }
    fn set_read_timeout(&mut self, value: Option<Duration>) -> Result<(), IoError> {
        match self {
            Self::Plain(c) => c.set_read_timeout(value),
            Self::Tls(c) => c.set_read_timeout(value),
        }
    }
}
impl AsyncWrite for Transport {
    fn poll_write(&mut self, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<Result<usize, IoError>> {
        self.write(cx, bytes)
    }
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        self.flush(cx)
    }
    fn set_write_timeout(&mut self, value: Option<Duration>) -> Result<(), IoError> {
        match self {
            Self::Plain(c) => c.set_write_timeout(value),
            Self::Tls(c) => c.set_write_timeout(value),
        }
    }
}
impl leanhttp::Close for Transport {
    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        match self {
            Self::Plain(c) => c.poll_close(cx),
            Self::Tls(c) => c.poll_close(cx),
        }
    }
}
struct Ready(Option<Connection>);
impl Dial for Ready {
    type Conn = Connection;
    async fn dial(&mut self, _: leanhttp::Target<'_>) -> leanhttp::Result<Connection> {
        self.0.take().ok_or(leanhttp::Error::Connect)
    }
}

pub(crate) fn error(value: impl std::fmt::Debug) -> std::io::Error {
    match spin_core::validation::text(format_args!("network: {value:?}")) {
        Ok(message) => std::io::Error::other(message),
        Err(_) => std::io::Error::from(std::io::ErrorKind::OutOfMemory),
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use spin_store::IdSource;
    use std::{
        io::{BufRead, BufReader},
        path::PathBuf,
        process::{Child, Command, Stdio},
        time::Instant,
    };
    struct Peer(Child, PathBuf);
    impl Drop for Peer {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
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
        let root = std::fs::read(data.join("chain/ecdsa-root.der")).unwrap();
        let roots = leantls::Roots::from_concatenated_der(&root).unwrap();
        assert_eq!(
            leantls::Roots::from_concatenated_der(ROOTS).unwrap().len(),
            119
        );
        crate::executor::block_on(async {
            let raw = Connection::new(TcpStream::connect(address).unwrap()).unwrap();
            let mut transport = Transport::secure(raw, "leantls.test", port, roots, 1_790_640_000)
                .await
                .unwrap();
            let bytes: Vec<u8> = (0..32_768_u32).map(|n| n.to_le_bytes()[0]).collect();
            let until = Instant::now() + Duration::from_secs(10);
            let mut at = 0;
            while at < bytes.len() {
                let n = std::future::poll_fn(|cx| {
                    assert!(Instant::now() < until);
                    transport.write(cx, &bytes[at..])
                })
                .await
                .unwrap();
                assert!(n > 0);
                at += n;
            }
            std::future::poll_fn(|cx| {
                assert!(Instant::now() < until);
                transport.flush(cx)
            })
            .await
            .unwrap();
            let mut actual = vec![0; bytes.len()];
            let mut at = 0;
            while at < actual.len() {
                let n = std::future::poll_fn(|cx| {
                    assert!(Instant::now() < until);
                    transport.read(cx, &mut actual[at..])
                })
                .await
                .unwrap();
                assert!(n > 0);
                at += n;
            }
            assert_eq!(actual, bytes);
            drop(transport);
            let raw = Connection::new(TcpStream::connect(address).unwrap()).unwrap();
            assert!(
                Transport::secure(raw, "wrong.example", port, roots, 1_790_640_000)
                    .await
                    .is_err()
            );
            assert!(
                Transport::connect(
                    TcpStream::connect(address).unwrap(),
                    "leantls.test",
                    port,
                    true
                )
                .await
                .is_err()
            );
        });
    }
}
