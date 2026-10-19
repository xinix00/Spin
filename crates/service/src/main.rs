//! spin-service: de bewoner op HopOS die een Hop-cluster aan Spin toont.
#![cfg_attr(target_os = "none", no_std, no_main)]
#![deny(unsafe_code)]
#[cfg(target_os = "none")]
extern crate alloc;
#[cfg(target_os = "none")]
mod hopos;
#[cfg(target_os = "none")]
applib::main!(hopos::main);
#[cfg(not(target_os = "none"))]
fn main() {
    eprintln!("spin-service requires a native HopOS target");
    std::process::exit(1);
}
