//! Platformhandvatten bevatten geen appstaat; iedere RNG heeft zijn eigen staat.
use alloc::string::String;
use applib::{App, EXEC, appnet::TcpListener, stacktask::Suspender, tcp::TcpConn};
use core::{
    future::Future,
    pin::pin,
    task::{Context, Poll},
    time::Duration,
};
use spin_domain::{Time, Timestamp};
use spin_runtime::{Clock, Platform};
use spin_security::Entropy;
use spin_server::{Error, Result};

pub(crate) fn failure(error: impl core::fmt::Display) -> Error {
    applib::log!("SPIN_PLATFORM_FAILED error={error}");
    Error::Http(503, "HopOS platform operation failed")
}
pub(crate) fn timestamp(app: &App) -> Result<Timestamp> {
    Timestamp::from_time(Time(
        app.wall_ns()
            .ok_or(Error::Http(503, "UTC clock not synchronized"))?,
    ))
    .map_err(Error::from)
}
#[derive(Clone, Copy)]
pub(crate) struct Monotonic;
impl Clock for Monotonic {
    fn millis(self) -> u64 {
        applib::clock::now_ns() / 1_000_000
    }
}
pub(crate) struct Native<'a> {
    pub(crate) app: &'static App,
    pub(crate) net: &'static applib::appnet::Net,
    pub(crate) listener: Option<TcpListener>,
    pub(crate) wait: Option<&'a Suspender>,
}
impl Platform for Native<'_> {
    type Connection = TcpConn;
    type Dial = crate::outbound::Dial;
    fn dial(&self) -> Result<Self::Dial> {
        Ok(crate::outbound::Dial {
            app: self.app,
            net: self.net,
        })
    }
    type Clock = Monotonic;
    fn accept(&mut self, context: &mut Context<'_>) -> Result<Option<(TcpConn, String)>> {
        let Some(listener) = &mut self.listener else {
            return Ok(None);
        };
        match pin!(listener.accept()).poll(context) {
            Poll::Pending => Ok(None),
            Poll::Ready(Err(error)) => Err(failure(error)),
            Poll::Ready(Ok(stream)) => {
                let remote = stream.remote().map_err(failure)?;
                let peer = spin_core::validation::text(format_args!(
                    "{}.{}.{}.{}",
                    remote.ip[0], remote.ip[1], remote.ip[2], remote.ip[3]
                ))
                .map_err(failure)?;
                Ok(Some((TcpConn::new(stream, EXEC.get()), peer)))
            }
        }
    }
    fn clock(&self) -> Monotonic {
        Monotonic
    }
    fn timestamp(&self) -> Result<Timestamp> {
        timestamp(self.app)
    }
    fn stopped(&self) -> bool {
        self.app.ctrl().kill_requested()
    }
    /// De eigenaar slaapt tot de deurbel of de vloer: 10 ms zolang er werk is
    /// dat alleen door pollen vordert, anders één seconde (de cadans van zijn
    /// onderhoud). Zo wekt een stille tenant de core niet honderd keer per
    /// seconde (handboek §4: de meetlat is wekken per seconde).
    fn idle(&mut self, mail: &spin_runtime::Mailbox, busy: bool) -> Result {
        let wait = self
            .wait
            .ok_or(Error::Http(503, "transport has no application stack"))?;
        let mut floor = pin!(
            EXEC.get()
                .after(Duration::from_millis(if busy { 10 } else { 1000 }))
        );
        let mut bell = pin!(mail.nudged());
        wait.wait(core::future::poll_fn(|cx| {
            if bell.as_mut().poll(cx).is_ready() || floor.as_mut().poll(cx).is_ready() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }))
        .map_err(|_| Error::Http(503, "server cancelled"))
    }
    fn log(message: core::fmt::Arguments<'_>) {
        applib::log!("{message}");
    }
}
pub(crate) struct Random(applib::rand::Rng);
impl Random {
    pub(crate) fn open(app: &App) -> Result<Self> {
        let random = applib::rand::Rng::open(app);
        // Een oudere kernel zonder seed mag geen voorspelbare capabilities krijgen.
        if random.origin() == applib::rand::Origin::None {
            return Err(Error::Http(503, "kernel entropy seed unavailable"));
        }
        Ok(Self(random))
    }
}
impl Entropy for Random {
    fn fill(&mut self, bytes: &mut [u8]) -> spin_security::Result {
        self.0.fill(bytes);
        Ok(())
    }
}
impl spin_store::IdSource for Random {
    fn next(&mut self, prefix: &str) -> spin_store::Result<String> {
        let mut bytes = [0; 16];
        self.fill(&mut bytes)?;
        let mut id = spin_domain::try_string(prefix)?;
        spin_domain::try_push_str(&mut id, "_")?;
        id.try_reserve(32)
            .map_err(|_| spin_domain::Error::OutOfMemory)?;
        for byte in bytes {
            use core::fmt::Write;
            write!(&mut id, "{byte:02x}").map_err(|_| spin_domain::Error::OutOfMemory)?;
        }
        Ok(id)
    }
}
pub(crate) struct Environment {
    pub(crate) app: &'static App,
    pub(crate) random: Random,
}
impl replica_hopos::Environment for Environment {
    fn random(&mut self, dst: &mut [u8]) -> replica_sqlite::Result {
        self.random.fill(dst).map_err(|_| replica_sqlite::Error::IO)
    }
    fn unix_millis(&mut self) -> replica_sqlite::Result<i64> {
        self.app
            .wall_ns()
            .and_then(|ns| i64::try_from(ns / 1_000_000).ok())
            .ok_or(replica_sqlite::Error::IO)
    }
}
