//! De hostgrens van Spin: bestanden en entropie voor dezelfde no_std-logica.
#![deny(unsafe_code)]
/// De bestandsadapter bezit een exclusieve database-directory en haar handvatten.
pub mod storage;

#[allow(unsafe_code)]
mod executor;
mod net;
mod outbound;
/// De HTTP-hostruntime met een vaste pool en één Store-eigenaar.
pub mod server;

mod apps;
mod archive;
mod blob_client;
mod bundles;
mod client_net;
mod images;
/// Niet-blokkerende stdio en eigendom van hostprocessen.
pub mod process;
#[allow(unsafe_code)]
mod pty;
/// De hostrunner bezit RPC-taken en houdt ze levend tijdens reconnects.
pub mod runner;
mod runner_agent;
mod runner_socket;
mod runner_stream;
mod runner_terminal;
mod runner_watch;
