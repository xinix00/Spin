//! Host-TCP is van Lean (`leanhttp::host`); hier staat alleen hoe een
//! niet-blokkerende socket op de pollronde van de host-executor wacht.
use leanhttp::host::{Reactor, TcpConn};
use std::{net::TcpStream, os::fd::AsRawFd, task::Waker};

/// De host-executor als reactor: een socket die niet verder kan, meldt zijn
/// interesse voor de volgende `poll`-ronde (`executor::idle`).
#[derive(Clone, Copy, Default)]
pub struct Executor;
impl Reactor for Executor {
    const BLOCKING: bool = false;
    fn wait(&self, socket: &TcpStream, write: bool, _: &Waker) {
        crate::executor::wait_for(
            socket.as_raw_fd(),
            if write { libc::POLLOUT } else { libc::POLLIN },
        );
    }
}

/// Een niet-blokkerende TCP-verbinding met de termijnen van leanhttp.
pub(crate) type Connection = TcpConn<Executor>;

/// Neemt een aangenomen of verbonden socket over.
pub(crate) fn connection(socket: TcpStream) -> std::io::Result<Connection> {
    TcpConn::new(socket, Executor, None)
}
