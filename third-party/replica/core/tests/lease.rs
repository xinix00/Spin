//! De host serialiseert starts; de lease detecteert verlies en schrijft nooit blind over.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use replica_core::{
    Error,
    lease::{Acquisition, Lease},
    local::{self, Name},
    time::Time,
};
use support::Fs;
fn now(n: i64) -> Time {
    Time::unix(1_790_765_296 + n, 0).unwrap()
}
fn foreign(fs: &mut Fs, expires: Time) {
    let json = format!(
        "{{\"owner\":\"other\",\"expires_at\":\"{}\"}}",
        expires.encode().unwrap()
    );
    local::write(fs, &Name::new("db.lease").unwrap(), json.as_bytes()).unwrap();
}
#[test]
fn renews_on_age_and_loses_unconfirmed_ownership() {
    let mut fs = Fs::default();
    let mut lease = Lease::new(&mut fs, Name::new("db").unwrap()).unwrap();
    assert_eq!(lease.acquire(&mut fs, now(0)).unwrap(), Acquisition::Owned);
    let ops = fs.ops;
    lease.renew(&mut fs, now(4)).unwrap();
    assert_eq!(fs.ops, ops);
    lease.renew(&mut fs, now(5)).unwrap();
    assert!(fs.ops > ops);
    foreign(&mut fs, now(50));
    assert_eq!(lease.renew(&mut fs, now(10)), Err(Error::LeaseLost));
    assert_eq!(lease.renew(&mut fs, now(11)), Err(Error::LeaseLost));
    lease.release(&mut fs).unwrap();
    assert!(
        fs.data("db.lease")
            .unwrap()
            .windows(5)
            .any(|p| p == b"other")
    );
}
#[test]
fn expired_takeover_waits_and_corrupt_file_is_never_free() {
    let mut fs = Fs::default();
    foreign(&mut fs, now(10));
    let mut lease = Lease::new(&mut fs, Name::new("db").unwrap()).unwrap();
    assert_eq!(
        lease.acquire(&mut fs, now(0)).unwrap(),
        Acquisition::Wait(now(5))
    );
    assert_eq!(
        lease.acquire(&mut fs, now(11)).unwrap(),
        Acquisition::Wait(now(12))
    );
    assert_eq!(lease.acquire(&mut fs, now(12)).unwrap(), Acquisition::Owned);
    local::write(&mut fs, &Name::new("db.lease").unwrap(), b"{broken").unwrap();
    assert!(lease.renew(&mut fs, now(17)).is_err());
    assert_eq!(lease.renew(&mut fs, now(18)), Err(Error::LeaseLost));
    let mut next = Lease::new(&mut fs, Name::new("db").unwrap()).unwrap();
    assert!(next.acquire(&mut fs, now(20)).is_err());
    assert_eq!(fs.data("db.lease").unwrap(), b"{broken");
}
#[test]
fn expired_clock_or_failed_sync_stops_writer() {
    let mut fs = Fs::default();
    let mut lease = Lease::new(&mut fs, Name::new("db").unwrap()).unwrap();
    assert!(lease.acquire(&mut fs, Time::unix(1, 0).unwrap()).is_err());
    lease.acquire(&mut fs, now(0)).unwrap();
    assert_eq!(lease.renew(&mut fs, now(30)), Err(Error::LeaseLost));
    let mut fs = Fs::default();
    let mut lease = Lease::new(&mut fs, Name::new("db").unwrap()).unwrap();
    lease.acquire(&mut fs, now(0)).unwrap();
    fs.fail = Some((fs.ops + 3, true));
    assert!(lease.renew(&mut fs, now(5)).is_err());
    fs.fail = None;
    assert_eq!(lease.renew(&mut fs, now(6)), Err(Error::LeaseLost));
}
