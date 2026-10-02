//! Replica's existing advisory lease gets its own task, including while SQLite or S3 parks.
use super::*;
use core::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};
use replica_core::{
    lease::{Acquisition, Lease},
    local::Name,
    time::Time,
};
use replica_sqlite::asynchronous::Bridge;

struct LeaseClock {
    wall: u64,
    mono: u64,
}
impl LeaseClock {
    fn now(&mut self, app: &App) -> Result<Time> {
        let mono = applib::clock::now_ns();
        let wall = app
            .wall_ns()
            .ok_or(Error::Http(503, "lease clock unavailable"))?;
        // Hop can refine its initial UTC estimate with NTP after the app starts.
        // A backwards correction must not reverse an already acquired lease's clock.
        self.wall = wall.max(self.wall.saturating_add(mono.saturating_sub(self.mono)));
        self.mono = mono;
        Time::unix(
            i64::try_from(self.wall / 1_000_000_000).map_err(failure)?,
            (self.wall % 1_000_000_000) as u32,
        )
        .map_err(|error| failure(format_args!("{error:?}")))
    }
}
/// The caller's database task starts only after acquire and is cancelled before releasing the lease.
pub(super) async fn guard<F: Future<Output = Result>>(
    app: &'static App,
    net: &'static appnet::Net,
    root: &str,
    work: impl FnOnce() -> F,
) -> Result {
    let mut clock = LeaseClock {
        wall: 0,
        mono: applib::clock::now_ns(),
    };
    let mut files = Files::new(
        net.system_client(),
        root,
        Environment {
            app,
            random: Random::open(app)?,
        },
    )
    .map_err(|error| failure(format_args!("{error:?}")))?;
    // SAFETY: This stack uses only Replica's safe file/lease API, never a SQLite engine.
    let mut lease = unsafe {
        Task::new(256 << 10, |s| -> Result<Lease> {
            let wait = Wait(s);
            let mut backend = Bridge::new(&mut files, &wait);
            let mut lease = Lease::new(
                &mut backend,
                Name::new("spin.sqlite").map_err(|error| failure(format_args!("{error:?}")))?,
            )
            .map_err(|error| failure(format_args!("{error:?}")))?;
            loop {
                if app.ctrl().kill_requested() {
                    return Err(Error::Http(503, "lease acquisition cancelled"));
                }
                match lease
                    .acquire(&mut backend, clock.now(app)?)
                    .map_err(|error| failure(format_args!("{error:?}")))?
                {
                    Acquisition::Owned => return Ok(lease),
                    Acquisition::Wait(_) => {
                        s.wait(EXEC.get().after(Duration::from_millis(100)))
                            .map_err(|_| Error::Http(503, "lease acquisition cancelled"))?;
                    }
                }
            }
        })
    }
    .map_err(|_| Error::Http(503, "lease stack allocation failed"))?
    .await?;
    applib::log!("SPIN_LEASE_ACQUIRED");
    let result = {
        let mut running = pin!(work());
        // SAFETY: A separate stack and backend own only the lease sidecar. They never enter SQLite.
        let heartbeat = unsafe {
            Task::new(256 << 10, |s| -> Result {
                let wait = Wait(s);
                let mut backend = Bridge::new(&mut files, &wait);
                loop {
                    lease
                        .renew(&mut backend, clock.now(app)?)
                        .map_err(|error| failure(format_args!("{error:?}")))?;
                    s.wait(EXEC.get().after(Duration::from_secs(1)))
                        .map_err(|_| Error::Http(503, "lease heartbeat cancelled"))?;
                }
            })
        }
        .map_err(|_| Error::Http(503, "lease heartbeat allocation failed"));
        match heartbeat {
            Err(error) => Err(error),
            Ok(heartbeat) => {
                let mut heartbeat = pin!(heartbeat);
                poll_fn(|cx| {
                    if let Poll::Ready(result) = heartbeat.as_mut().poll(cx) {
                        applib::log!("SPIN_LEASE_LOST");
                        return Poll::Ready(
                            result.and(Err(Error::Http(503, "database lease lost"))),
                        );
                    }
                    running.as_mut().poll(cx)
                })
                .await
            }
        }
        // Both scoped tasks drop here: cancellation unwinds any parked SQL before release.
    };
    // SAFETY: The heartbeat and app task have ended; this stack only releases its own sidecar.
    let released = unsafe {
        Task::new(256 << 10, |s| -> Result {
            let wait = Wait(s);
            let mut backend = Bridge::new(&mut files, &wait);
            lease
                .release(&mut backend)
                .map_err(|error| failure(format_args!("{error:?}")))
        })
    }
    .map_err(|_| Error::Http(503, "lease release allocation failed"))?
    .await;
    result.and(released)
}
