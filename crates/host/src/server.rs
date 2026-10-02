//! Hostschil voor integratietests van dezelfde native serverruntime.
use crate::{executor, net::Connection, storage::Random};
use spin_domain::{Time, Timestamp};
pub use spin_runtime::{CONNECTIONS, PASSWORD_TASKS};
use spin_runtime::{Clock, Platform};
use spin_server::{Result, Server};
use spin_store::Persistence;
use std::{
    net::TcpListener,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
/// UTC aan de hostgrens.
pub fn timestamp() -> std::io::Result<Timestamp> {
    let nanos = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos(),
    )
    .map_err(std::io::Error::other)?;
    Timestamp::from_time(Time(nanos)).map_err(std::io::Error::other)
}
#[derive(Clone, Copy)]
struct HostClock(Instant);
impl Clock for HostClock {
    fn millis(self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}
struct Host<'a> {
    listener: TcpListener,
    stop: &'a AtomicBool,
    clock: HostClock,
}
fn boundary(error: std::io::Error) -> spin_server::Error {
    eprintln!("SPIN_PLATFORM_FAILED error={error}");
    spin_server::Error::Http(503, "host transport unavailable")
}
impl Platform for Host<'_> {
    type Connection = Connection;
    type Dial = crate::outbound::Dial;
    fn dial(&self) -> Result<Self::Dial> {
        Ok(crate::outbound::Dial)
    }
    type Clock = HostClock;
    fn accept(&mut self, _: &mut Context<'_>) -> Result<Option<(Connection, String)>> {
        match self.listener.accept() {
            Ok((socket, address)) => Ok(Some((
                Connection::new(socket).map_err(boundary)?,
                spin_core::validation::text(format_args!("{}", address.ip()))
                    .map_err(|_| spin_server::Error::Http(503, "peer allocation failed"))?,
            ))),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(boundary(e)),
        }
    }
    fn clock(&self) -> HostClock {
        self.clock
    }
    fn timestamp(&self) -> Result<Timestamp> {
        timestamp().map_err(boundary)
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
    fn idle(&mut self, _: &spin_runtime::Mailbox, _: bool) -> Result {
        executor::idle();
        Ok(())
    }
    fn log(message: core::fmt::Arguments<'_>) {
        eprintln!("{message}");
    }
}
/// Draait de testlistener tot shutdown; Drop sluit alle sockets.
pub fn serve<P: Persistence>(
    listener: TcpListener,
    server: &mut Server<P>,
    runtime: &mut Random,
    secure: bool,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    spin_runtime::serve(
        Host {
            listener,
            stop,
            clock: HostClock(Instant::now()),
        },
        server,
        runtime,
        secure,
    )
    .map_err(std::io::Error::other)
}
