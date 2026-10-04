//! Native Spin-server: HopOS-netwerk en Replica-opslag hebben één eigenaar.
#![cfg_attr(target_os = "none", no_std, no_main)]
#![deny(unsafe_code)]
#[cfg(target_os = "none")]
extern crate alloc;
#[cfg(target_os = "none")]
#[allow(unsafe_code)]
mod boot;
#[cfg(target_os = "none")]
mod conn;
#[cfg(target_os = "none")]
mod outbound;
#[cfg(target_os = "none")]
mod platform;
#[cfg(target_os = "none")]
mod s3;
#[cfg(target_os = "none")]
applib::main!(boot::main);
#[cfg(not(target_os = "none"))]
fn main() {
    eprintln!(
        "spin-hopos-server requires a native HopOS target; use spin-client for macOS runners"
    );
    std::process::exit(1);
}
