//! De schil op HopOS: de config uit de env, Hop via hoplib, Spin via de runner-socket.
//!
//! | env                  | betekenis                                             |
//! |----------------------|-------------------------------------------------------|
//! | `SPIN_SERVER`        | de Spin waar de service zich meldt (`https://…`)      |
//! | `SPIN_WORKER_TOKEN`  | het worker-token van die Spin, zoals een runner      |
//! | `SPIN_SERVICE_TAG`   | welke jobs hij toont: die met tag `spin` gelijk hieraan |
//! | `SPIN_SERVICE_NAME`  | zijn naam in Spin (standaard `hop`)                   |
//! | `HOP_API_KEY`        | de HMAC-sleutel van Hop; verlaat het cluster nooit    |
//! | `HOP_AGENT`          | de agent-API (standaard `http://HOP:8080`, de eigen node) |
//! | `DNS`                | de resolver voor `SPIN_SERVER`                        |
mod hop;
mod link;

use alloc::string::String;
use applib::{App, EXEC, log};
use spin_core::worker::Worker;
use sync::{LocalCell, Signal};

/// Antwoorden en events wachten hier tot de socket ze kwijt kan, ook over een
/// reconnect heen; een herhaald verzoek krijgt zijn bewaarde antwoord.
pub(crate) static OWNER: LocalCell<Worker> = LocalCell::cell(Worker::new());
/// De bel van de socket: er ligt iets in [`OWNER`].
pub(crate) static BELL: Signal = Signal::new();
/// Er veranderde iets in Hop, of Spin kent de status nog niet: duw hem opnieuw.
pub(crate) static DIRTY: Signal = Signal::new();
/// Na een welcome gaat de status mee, ook als hij niet veranderde.
pub(crate) static FORCE: Signal = Signal::new();

/// Wat de service uit zijn env leest.
pub(crate) struct Config {
    pub(crate) server: String,
    pub(crate) token: String,
    pub(crate) name: String,
}

fn env(app: &App, key: &str) -> String {
    String::from(app.env(key).map_or("", str::trim))
}

/// De start van de bewoner: zonder geldige config is er niets te tonen.
pub(crate) async fn main(app: &'static App) {
    let config = Config {
        server: env(app, "SPIN_SERVER"),
        token: env(app, "SPIN_WORKER_TOKEN"),
        name: match env(app, "SPIN_SERVICE_NAME") {
            name if name.is_empty() => String::from("hop"),
            name => name,
        },
    };
    let tag = env(app, "SPIN_SERVICE_TAG");
    if config.server.is_empty() || config.token.is_empty() || tag.is_empty() {
        log!(
            "SPIN_SERVICE_CONFIG error=SPIN_SERVER, SPIN_WORKER_TOKEN and SPIN_SERVICE_TAG are required"
        );
        app.exit(2);
    }
    if let Err(error) = applib::appnet::up(app) {
        log!("SPIN_SERVICE_NET error={error:?}");
        app.exit(1);
    }
    let agent = hop::agent_url(app.env("HOP_AGENT"));
    let key = app.env("HOP_API_KEY").filter(|k| !k.is_empty());
    let hop = match hop::start(&agent, key, tag) {
        Ok(hop) => hop,
        Err(error) => {
            log!("SPIN_SERVICE_CONFIG error={error}");
            app.exit(2);
        }
    };
    if EXEC.get().spawn(hop::watch(hop)).is_err() || EXEC.get().spawn(hop::publish(hop)).is_err() {
        log!("SPIN_SERVICE_START error=cannot spawn the event watcher");
        app.exit(1);
    }
    log!(
        "SPIN_SERVICE_START name={} server={} agent={agent}",
        config.name,
        config.server
    );
    link::run(app, config, hop).await;
}
