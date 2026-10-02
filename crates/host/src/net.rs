//! Niet-blokkerende sockets; iedere deadline geldt voor de hele lees-/schrijffase.
use leanhttp::{AsyncRead, AsyncWrite, Close, IoError};
use std::{
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    task::{Context, Poll},
    time::{Duration, Instant},
};
pub(crate) struct Connection {
    socket: TcpStream,
    read_at: Option<Instant>,
    write_at: Option<Instant>,
}
impl Connection {
    pub(crate) fn new(socket: TcpStream) -> std::io::Result<Self> {
        socket.set_nonblocking(true)?;
        socket.set_nodelay(true)?;
        Ok(Self {
            socket,
            read_at: None,
            write_at: None,
        })
    }
}
fn error(error: &std::io::Error) -> IoError {
    match error.kind() {
        std::io::ErrorKind::TimedOut => IoError::TimedOut,
        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::NotConnected => IoError::Closed,
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
            IoError::Reset
        }
        _ => IoError::Other,
    }
}
impl AsyncRead for Connection {
    fn poll_read(&mut self, _: &mut Context<'_>, dst: &mut [u8]) -> Poll<Result<usize, IoError>> {
        if self.read_at.is_some_and(|at| Instant::now() >= at) {
            return Poll::Ready(Err(IoError::TimedOut));
        }
        match self.socket.read(dst) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(error(&e))),
        }
    }
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.read_at = timeout.and_then(|d| Instant::now().checked_add(d));
        Ok(())
    }
}
impl AsyncWrite for Connection {
    fn poll_write(&mut self, _: &mut Context<'_>, src: &[u8]) -> Poll<Result<usize, IoError>> {
        if self.write_at.is_some_and(|at| Instant::now() >= at) {
            return Poll::Ready(Err(IoError::TimedOut));
        }
        match self.socket.write(src) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(error(&e))),
        }
    }
    fn poll_flush(&mut self, _: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        Poll::Ready(Ok(()))
    }
    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.write_at = timeout.and_then(|d| Instant::now().checked_add(d));
        Ok(())
    }
}
impl Close for Connection {
    fn poll_close(&mut self, _: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        let _ = self.socket.shutdown(Shutdown::Both);
        Poll::Ready(Ok(()))
    }
}
