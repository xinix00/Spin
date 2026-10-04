//! Een aangenomen socket met termijnen zonder eigen timer.
//!
//! applib's `TcpConn` zet voor elke lees- en schrijftermijn een wekker op
//! het timerwiel van de executor, en die plek blijft bezet zolang de
//! termijn loopt. Het wiel heeft 32 plekken (`applib::rt::TIMERS`), Spin
//! neemt tot 64 verbindingen aan en leanhttp houdt op elke stille
//! keep-alive-verbinding een leestermijn van 60 s. Boven de 32 telt de
//! executor `timer_overflows`, wekt de taak meteen weer en de hele app-core
//! spint op 100% tot er een plek vrijkomt (handboek: "Timers zijn schaars").
//!
//! Hier bewaart de verbinding haar termijnen zelf, als absoluut tijdstip op
//! de klok van de executor. Een poll die `Pending` geeft terwijl een termijn
//! loopt, meldt dat tijdstip met `fetch_min` in [`NEXT_DEADLINE`]; de
//! boot-lus (`boot.rs`) zet die vlag aan het begin van elke ronde op
//! `u64::MAX`, pollt elke levende verbinding (zodat elke lopende termijn
//! opnieuw gemeld wordt) en slaapt daarna met één `until` op de vroegste.
//! Eén timer voor alle sockets in plaats van één per socket, en de termijnen
//! blijven exact: de lus wordt op de vroegste gewekt en de volgende poll van
//! die verbinding ziet `nu >= termijn`.
//!
//! Dezelfde semantiek als `TcpConn`: een termijn is een deadline vanaf het
//! moment van zetten, blijft staan tot de volgende `set_*_timeout`, en een
//! verstreken termijn geeft `IoError::TimedOut` tot hij gewist wordt.
use applib::{EXEC, appnet::TcpStream, tcp::TcpConn};
use core::{
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    task::{Context, Poll},
    time::Duration,
};
use leanhttp::{AsyncRead, AsyncWrite, Close, IoError};

/// De vroegste lopende termijn van een socket die deze ronde `Pending` gaf;
/// `u64::MAX` als er geen is. De boot-lus reset hem per ronde.
pub(crate) static NEXT_DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Een aangenomen verbinding voor leanhttp; zie de moduledocumentatie.
pub(crate) struct Conn {
    inner: TcpConn,
    /// Leestermijn als absoluut tijdstip (`applib::clock::now_ns`).
    read: Option<u64>,
    /// Schrijftermijn als absoluut tijdstip.
    write: Option<u64>,
}
impl Conn {
    /// Neemt `stream` over. De binnenste `TcpConn` krijgt expliciet geen
    /// termijnen, zodat applib nooit een wekker op het wiel zet; daarna
    /// roept niets hier nog zijn `set_*_timeout` aan.
    pub(crate) fn new(stream: TcpStream) -> Self {
        let mut inner = TcpConn::new(stream, EXEC.get());
        let _ = inner.set_read_timeout(None);
        let _ = inner.set_write_timeout(None);
        Self {
            inner,
            read: None,
            write: None,
        }
    }
}
/// Een relatieve termijn als tijdstip op de klok; `None` wist hem.
fn arm(timeout: Option<Duration>) -> Option<u64> {
    timeout.map(|d| {
        let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        applib::clock::now_ns().saturating_add(ns)
    })
}
/// Het antwoord op een `Pending` van de stroom: verstreken is `TimedOut`,
/// anders wordt de termijn gemeld voor de tik van de boot-lus. De waker
/// staat al bij de stack (de stroom zette hem bij zijn `Pending`).
fn pending<T>(deadline: Option<u64>) -> Poll<Result<T, IoError>> {
    if let Some(at) = deadline {
        if applib::clock::now_ns() >= at {
            return Poll::Ready(Err(IoError::TimedOut));
        }
        NEXT_DEADLINE.fetch_min(at, Relaxed);
    }
    Poll::Pending
}
impl AsyncRead for Conn {
    /// Leest van de stroom; bij `Pending` beslist de eigen leestermijn.
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, IoError>> {
        match self.inner.poll_read(cx, buf) {
            Poll::Pending => pending(self.read),
            ready => ready,
        }
    }
    /// Zet de leestermijn vanaf nu, alleen in dit object; geen timer.
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.read = arm(timeout);
        Ok(())
    }
}
impl AsyncWrite for Conn {
    /// Schrijft naar de stroom; bij `Pending` beslist de eigen schrijftermijn.
    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, IoError>> {
        match self.inner.poll_write(cx, buf) {
            Poll::Pending => pending(self.write),
            ready => ready,
        }
    }
    /// De stroom buffert niets zelf; direct door.
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        self.inner.poll_flush(cx)
    }
    /// Zet de schrijftermijn vanaf nu, alleen in dit object; geen timer.
    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.write = arm(timeout);
        Ok(())
    }
}
impl Close for Conn {
    /// Sluit de stroom (synchroon in applib: FIN na de gebufferde data).
    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        self.inner.poll_close(cx)
    }
    /// Door naar de stroom; applib meldt geen groei.
    fn has_grown(&self) -> bool {
        self.inner.has_grown()
    }
}
