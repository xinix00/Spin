//! Spin-logica zonder I/O: eigenaars ontvangen opdrachten als waarden.
#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
extern crate alloc;
/// ACP-framing, RPC-eigendom en onderhandelde agentsessies.
pub mod acp;
pub mod layers;
pub mod orchestrator;
/// De instructies en hervatberichten voor workflowagents.
pub mod prompts;
pub mod runner;
pub mod validation;
pub mod workflow;

/// Hervatbare uploads met begrensde tickets en expliciete I/O-bevestigingen.
pub mod upload;

/// Credential-vrije Git-remotes en apprecepten.
pub mod git;

/// Tar-koppen en laagpaden zonder bestands-I/O of onbegrensde bodybuffers.
pub mod archive;
/// Streaming databasebackups met ZIP64 en een vaste geheugenlimiet.
pub mod backup;
/// ZIP-bundels, CRCs en previewtypes.
pub mod bundle;
/// Docker-capsules; opdrachten gaan door een expliciete procespoort.
pub mod docker;
/// Expliciete, begrensde opdrachten voor de host-procesadapter.
pub mod process;
/// WebSocket-handshake en begrensde frames voor browsers en runners.
pub mod websocket;
/// Runnerzijde van het protocol: tickets, herhaling en antwoordbudgetten.
pub mod worker;
