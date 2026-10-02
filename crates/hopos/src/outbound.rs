//! Native DNS, TCP en geverifieerde TLS; iedere dial krijgt verse platformentropie.
use crate::platform::Random;
use applib::{
    App, EXEC,
    appnet::{Net, TcpStream},
    tcp::TcpConn,
};
use core::{
    task::{Context, Poll},
    time::Duration,
};
use leanhttp::{AsyncRead, AsyncWrite, IoError};
use spin_security::Entropy;
// Dezelfde Mozilla NSS-wortels als de macOS-runner en Hop.
const ROOTS: &[u8] = include_bytes!("../../host/src/roots.der");
pub(crate) struct Dial {
    pub(crate) app: &'static App,
    pub(crate) net: &'static Net,
}
#[allow(clippy::large_enum_variant)] // Eén eigenaar van de complete verbinding, zonder extra heapobject.
pub(crate) enum Transport {
    Plain(TcpConn),
    Tls(leanhttps::TlsConn<TcpConn>),
}
struct Ready(Option<TcpConn>);
impl leanhttp::Dial for Ready {
    type Conn = TcpConn;
    async fn dial(&mut self, _: leanhttp::Target<'_>) -> leanhttp::Result<TcpConn> {
        self.0.take().ok_or(leanhttp::Error::Connect)
    }
}
impl leanhttp::Dial for Dial {
    type Conn = Transport;
    fn is_encrypted(&self) -> bool {
        true
    }
    async fn dial(&mut self, target: leanhttp::Target<'_>) -> leanhttp::Result<Transport> {
        let address = self.net.resolve(target.host).await.map_err(|error| {
            applib::log!("SPIN_OUTBOUND_DNS_FAILED error={error:?}");
            leanhttp::Error::Connect
        })?;
        let stream = TcpStream::connect_timeout(address, target.port, Duration::from_secs(10))
            .await
            .map_err(|error| {
                applib::log!("SPIN_OUTBOUND_TCP_FAILED error={error:?}");
                leanhttp::Error::Connect
            })?;
        let mut raw = TcpConn::new(stream, EXEC.get());
        if !target.https {
            return Ok(Transport::Plain(raw));
        }
        let roots =
            leantls::Roots::from_concatenated_der(ROOTS).map_err(|_| leanhttp::Error::Connect)?;
        let now = self.app.wall_ns().ok_or(leanhttp::Error::Connect)? / 1_000_000_000;
        let verifier = leantls::ChainVerifier::new(roots, now);
        let mut seed = [0; leantls::Entropy::LEN];
        Random::open(self.app)
            .map_err(|_| leanhttp::Error::Connect)?
            .fill(&mut seed)
            .map_err(|_| leanhttp::Error::Connect)?;
        raw.set_read_timeout(Some(Duration::from_secs(20)))?;
        raw.set_write_timeout(Some(Duration::from_secs(20)))?;
        let mut tls = leanhttps::TlsDial::new(
            Ready(Some(raw)),
            leantls::Trust::Chain(&verifier),
            move || {
                leantls::Entropy::new(core::mem::replace(&mut seed, [0; leantls::Entropy::LEN]))
            },
        );
        let mut conn = match tls.dial(target).await {
            Ok(conn) => conn,
            Err(error) => {
                applib::log!(
                    "SPIN_OUTBOUND_TLS_FAILED error={error:?} detail={:?}",
                    tls.last_error()
                );
                return Err(error);
            }
        };
        conn.set_read_timeout(None)?;
        conn.set_write_timeout(None)?;
        Ok(Transport::Tls(conn))
    }
}
impl AsyncRead for Transport {
    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<Result<usize, IoError>> {
        match self {
            Self::Plain(c) => c.poll_read(cx, bytes),
            Self::Tls(c) => c.poll_read(cx, bytes),
        }
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
        match self {
            Self::Plain(c) => c.poll_write(cx, bytes),
            Self::Tls(c) => c.poll_write(cx, bytes),
        }
    }
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        match self {
            Self::Plain(c) => c.poll_flush(cx),
            Self::Tls(c) => c.poll_flush(cx),
        }
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
