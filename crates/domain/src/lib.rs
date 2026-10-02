//! Spin-domein: contracten en zuivere regels, zonder I/O of gedeelde staat.
#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
extern crate alloc;
pub use hop_types::{Error, Map, Name, Time, TryClone, try_push, try_push_str, try_string};
/// Een faalbare bewerking, los van het domeinobject `Result`.
pub type Fallible<T = ()> = core::result::Result<T, Error>;

#[macro_use]
mod wire;
pub mod engine;
pub mod json;
mod models;
pub mod protocol;
mod rules;
pub mod state;
pub use models::*;
pub use rules::*;
pub use wire::{Bytes, List, MAX_ITEMS, RawJson, Timestamp, Wire, WireMap};
