//! SQLite-opslag op HopOS: de async system-client, via de parkeerbare C-stack.
//!
//! Eén pad: iedere VFS-callback wacht met `applib::stacktask::Suspender::wait`
//! op de system-client, en `xSync` wordt `sys::Client::sync` (OP_SYNC). Er is
//! geen blokkerende of gepolde variant ernaast en geen geneste executor.
#![no_std]
#![forbid(unsafe_code)]
mod storage;
pub use storage::{Environment, Files, Wait};
