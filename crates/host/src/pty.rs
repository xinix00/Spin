//! De kleine Unix-PTY-fundering: file descriptors en het child behoren aan één waarde.
use spin_core::process::Command;
use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    process::{Child, ExitStatus, Stdio},
};
pub(crate) struct Pty {
    child: Child,
    master: File,
    status: Option<ExitStatus>,
}
fn size(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: if rows == 0 { 30 } else { rows.min(500) },
        ws_col: if cols == 0 { 120 } else { cols.min(500) },
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}
impl Pty {
    pub(crate) fn spawn(command: &Command, rows: u16, cols: u16) -> io::Result<Self> {
        command.validate().map_err(io::Error::other)?;
        let mut master = -1;
        let mut slave = -1;
        let mut size = size(rows, cols);
        // SAFETY: openpty schrijft twee fd's naar geldige lokale ints. Naam en
        // termios zijn optioneel; de winsize blijft geldig tot de call terugkeert.
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: alleen een succesvolle openpty levert deze twee unieke fd's;
        // OwnedFd neemt ze ieder eenmaal over, ook op latere foutpaden.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        for fd in [&master, &slave] {
            // SAFETY: fd is levend; F_SETFD leest geen variadische pointer.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: deze fcntl-operaties raken alleen de open master die we bezitten.
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: F_SETFL verwacht een integer; de descriptor blijft in eigendom.
        if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        let mut child = std::process::Command::new(&command.program);
        child.args(&command.args);
        for (key, value) in command.environment.iter() {
            child.env(key, value);
        }
        child
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: de post-fork hook gebruikt uitsluitend setsid/ioctl en errno;
        // geen allocatie, locks of parentreferenties. Rust heeft stdin al op
        // de slave gezet. TIOCSCTTY koppelt alleen dit child aan zijn eigen PTY.
        unsafe {
            child.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                #[allow(clippy::useless_conversion)]
                // BSD declareert deze request als u32, Linux als c_ulong.
                let request: libc::c_ulong = libc::TIOCSCTTY.into();
                if libc::ioctl(0, request, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Self {
            child: child.spawn()?,
            master: File::from(master),
            status: None,
        })
    }
    pub(crate) fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self.master.read(bytes) {
            Err(error) if error.raw_os_error() == Some(libc::EIO) => Ok(0),
            result => {
                if result.as_ref().is_ok_and(|n| *n > 0) {
                    crate::executor::progress();
                }
                result
            }
        }
    }
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.master.write(bytes)?;
        if n > 0 {
            crate::executor::progress();
        }
        Ok(n)
    }
    pub(crate) fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        if rows == 0 || cols == 0 {
            return Ok(());
        }
        let size = size(rows, cols);
        // SAFETY: TIOCSWINSZ leest een geldige winsize, op onze levende master.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub(crate) fn status(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status)
    }
}
impl Drop for Pty {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::time::{Duration, Instant};
    fn until(pty: &mut Pty, marker: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut result = Vec::new();
        loop {
            assert!(
                Instant::now() < deadline,
                "PTY output timeout: {:?}",
                result
            );
            let mut bytes = [0; 8192];
            match pty.read(&mut bytes) {
                Ok(n) => {
                    result.extend_from_slice(&bytes[..n]);
                    if result.windows(marker.len()).any(|part| part == marker) {
                        return result;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("PTY read: {error}"),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn real_terminal_has_controlling_tty_resize_input_and_exit_status() {
        let mut command = Command::new("/bin/sh").unwrap();
        command.arg("-c").unwrap();
        command.arg("stty -echo; stty size; IFS= read -r line; stty size; printf 'got=%s\\n' \"$line\"; exit 7").unwrap();
        let mut pty = Pty::spawn(&command, 24, 80).unwrap();
        assert!(String::from_utf8_lossy(&until(&mut pty, b"24 80\r\n")).contains("24 80"));
        pty.resize(31, 121).unwrap();
        assert_eq!(pty.write(b"hello\n").unwrap(), 6);
        let output = until(&mut pty, b"got=hello\r\n");
        assert!(String::from_utf8_lossy(&output).contains("31 121"));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = pty.status().unwrap() {
                assert_eq!(status.code(), Some(7));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
