//! De Spin-server als eigenaar van Store en tijdelijke browserautorisatie.
#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
extern crate alloc;
use alloc::string::String;
use spin_domain::{self as d, Map, Timestamp, TryClone, Wire, json::Value};
use spin_security::Entropy;
use spin_store::{IdSource, Persistence, Store};
mod acp;
mod actions;
mod external;
mod oauth;
mod pulls;
pub use external::{NetworkRequest, NetworkResponse, NetworkWait};
mod apps;
mod auth;
mod backup;
mod capsules;
mod code;
mod login_operations;
mod management;
mod materialize;
mod operations;
mod options;
mod restore;
mod storage;
pub use backup::{BackupWait, Download};
pub use capsules::CapsuleWait;
pub use operations::OperationWait;
mod attachments;
mod deliverables;
/// Transportonafhankelijke HTTP-verzoeken en antwoorden.
pub mod http;
mod routes;
mod runners;
mod terminal;
mod uploads;
mod worker_api;
mod workflow_mcp;
mod workspace;
pub use acp::ChatLink;
pub use auth::PasswordWork;
pub use http::{Request, Response};
pub use runners::{RunnerEvent, RunnerLink};
pub use terminal::TerminalLink;
pub use uploads::UploadWait;

/// Een geweigerd verzoek of een fout aan de domein-/opslaggrens.
#[derive(Debug)]
pub enum Error {
    /// De HTTP-laag weigert het verzoek met een vaste publieke reden.
    Http(u16, &'static str),
    /// Domein-, parser- of allocatiefout.
    Data(d::Error),
    /// De Store kon de opdracht niet uitvoeren.
    Store(spin_store::Error),
    /// Versleuteling of entropie faalde.
    Security(spin_security::Error),
}
impl From<d::Error> for Error {
    fn from(e: d::Error) -> Self {
        Self::Data(e)
    }
}
impl From<spin_store::Error> for Error {
    fn from(e: spin_store::Error) -> Self {
        Self::Store(e)
    }
}
impl From<spin_security::Error> for Error {
    fn from(e: spin_security::Error) -> Self {
        Self::Security(e)
    }
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Http(code, why) => write!(f, "HTTP {code}: {why}"),
            Self::Data(e) => e.fmt(f),
            Self::Store(e) => e.fmt(f),
            Self::Security(e) => e.fmt(f),
        }
    }
}
impl core::error::Error for Error {}
impl Error {
    /// Publieke fouten bevatten geen paden, database-inhoud of geheimen.
    pub fn response(&self) -> Result<Response> {
        let (code, why) = match self {
            Self::Http(code, why) => (*code, *why),
            Self::Store(spin_store::Error::NotFound) => (404, "not found"),
            Self::Store(spin_store::Error::Conflict(why)) => (409, *why),
            Self::Store(spin_store::Error::NoWork) => (204, ""),
            Self::Store(spin_store::Error::StaleActivation) => (409, "stale activation"),
            Self::Store(spin_store::Error::LoginsBusy) => (409, "all logins are in use"),
            Self::Data(d::Error::Invalid { .. })
            | Self::Store(spin_store::Error::Data(d::Error::Invalid { .. })) => {
                (400, "invalid request")
            }
            Self::Security(spin_security::Error::PasswordLength(_)) => {
                (409, "password must contain 12 to 256 bytes")
            }
            _ => (500, "request could not be completed"),
        };
        if code == 204 {
            return Response::empty(code);
        }
        Response::json(code, &http::object(&[("error", Value::string(why)?)])?)
    }
}
/// Resultaat van een serveropdracht.
pub type Result<T = ()> = core::result::Result<T, Error>;
/// Werk dat de runtime tussen andere verbindingen mag uitvoeren.
// Geen verborgen heapallocatie voor een variant; de runtime heeft een vaste pool.
#[allow(clippy::large_enum_variant)]
pub enum Outcome {
    /// Het volledige HTTP-antwoord is beschikbaar.
    Response(Response),
    /// Begrensde CPU-stappen voor het wachtwoord, zonder de Store te lenen.
    Password(PasswordWork),
    /// Een geauthenticeerd abonnement op de zichtbare dashboardstaat.
    State(StateWatch),
    /// Een geautoriseerde, nog niet aangemelde runnerverbinding.
    Runner(RunnerLink),
    /// Een runner voert werk uit terwijl deze HTTP-verbinding wacht.
    Capsule(CapsuleWait),
    /// Een geautoriseerde browserterminal, gekoppeld aan één capsule.
    Terminal(TerminalLink),
    /// Een browser volgt een door de app beheerde agentsessie.
    Chat(ChatLink),
    /// Een upload wordt zonder lange database-lening geverifieerd.
    Upload(UploadWait),
    /// Een providerverzoek wacht buiten de app-eigenaar op HTTPS.
    Network(NetworkWait),
    /// Beheer wacht op bevestigde runnerstops vóór de Job-graaf verandert.
    Operation(OperationWait),
    /// Eén consistente databasebackup in begrensde netwerkblokken.
    Download(Download),
}
/// Een browserabonnement bewaart uitsluitend de hash van de sessiecapability.
pub struct StateWatch {
    token_hash: String,
}
/// De netwerk-runtime levert cryptografische willekeur en objectidentiteiten.
pub trait Runtime: Entropy + IdSource {}
impl<T: Entropy + IdSource> Runtime for T {}
/// De server bezit alle duurzame en vluchtige app-staat; transport krijgt waarden.
pub struct Server<P: Persistence> {
    store: Store<P>,
    csrf: Map<auth::Csrf>,
    attempts: Map<auth::Attempt>,
    runners: alloc::vec::Vec<spin_core::runner::Peer>,
    calls: alloc::vec::Vec<capsules::Call>,
    runner_cursor: usize,
    last_launch_sweep: Option<d::Time>,
    display_version: u64,
    terminals: alloc::vec::Vec<terminal::Terminal>,
    agents: alloc::vec::Vec<acp::Agent>,
    internal_url: String,
    uploads: alloc::vec::Vec<uploads::Upload>,
    app_starts: Map<apps::Start>,
    public_url: String,
    oauth_env: Map<d::GitOAuthConfiguration>,
    oauth_attempts: Map<oauth::Attempt>,
    network: alloc::vec::Vec<external::Call>,
    refresh_after: Map<u64>,
    refresh_checked: Option<d::Time>,
    operations: alloc::vec::Vec<operations::Operation>,
    login_operations: alloc::vec::Vec<login_operations::Work>,
    options: alloc::vec::Vec<options::Work>,
    export: Option<backup::Export>,
    backup_tickets: Map<backup::Ticket>,
    restores: alloc::vec::Vec<restore::Job>,
    watch_stamps: Map<String>,
    attachment_stamps: Map<String>,
    storage_report: Value,
    storage_started: Option<d::Time>,
}
impl<P: Persistence> Server<P> {
    /// Neemt de geopende Store over vóór de listener start.
    pub fn new(store: Store<P>) -> Self {
        Self {
            store,
            csrf: Map::new(),
            attempts: Map::new(),
            runners: alloc::vec::Vec::new(),
            calls: alloc::vec::Vec::new(),
            runner_cursor: 0,
            last_launch_sweep: None,
            display_version: 0,
            terminals: alloc::vec::Vec::new(),
            agents: alloc::vec::Vec::new(),
            internal_url: String::new(),
            uploads: alloc::vec::Vec::new(),
            app_starts: Map::new(),
            public_url: String::new(),
            oauth_env: Map::new(),
            oauth_attempts: Map::new(),
            network: alloc::vec::Vec::new(),
            refresh_after: Map::new(),
            refresh_checked: None,
            operations: alloc::vec::Vec::new(),
            login_operations: alloc::vec::Vec::new(),
            options: alloc::vec::Vec::new(),
            export: None,
            backup_tickets: Map::new(),
            restores: alloc::vec::Vec::new(),
            watch_stamps: Map::new(),
            attachment_stamps: Map::new(),
            storage_report: Value::Null,
            storage_started: None,
        }
    }
    /// De platformadapter publiceert Replica-werk tussen app-opdrachten.
    pub fn maintain_storage(&mut self, now: &Timestamp) -> Result {
        let result = self.store.maintain(now);
        self.refresh_storage(now)?;
        result?;
        self.store.collect_blob_garbage()?;
        self.prune_snapshot(now)
    }
    /// De bevestigde staatversie is de trigger voor samengevoegde browserupdates.
    pub fn version(&self) -> u64 {
        self.store.version().saturating_add(self.display_version)
    }
    fn display_changed(&mut self) {
        self.display_version = self.display_version.saturating_add(1);
    }
    /// De boot-schil herstelt vluchtige runtimeverwijzingen vóór de listener werk aanneemt.
    pub fn recover(&mut self, now: &Timestamp) -> Result {
        self.store.recover_runtime_status(now)?;
        self.restore_agents(now)?;
        Ok(())
    }
    /// Start een verzoek; langdurige wachtwoordberekening verlaat de actor als werk.
    pub fn begin(
        &mut self,
        req: Request<'_>,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Outcome> {
        if let Some(response) = self.restore_status(&req, now)? {
            return Ok(Outcome::Response(response));
        }
        if self.backup_active() && !(req.method == "GET" && req.path == "/healthz") {
            return Err(Error::Http(
                503,
                "database writes paused while a backup streams; retry shortly",
            ));
        }
        if req.body.len() > 3 << 20 {
            return Err(Error::Http(413, "request body too large"));
        }
        if let Some(outcome) = self.workflow_mcp(&req, now, runtime)? {
            return Ok(outcome);
        }
        if req.body.len() > http::MAX_BODY {
            return Err(Error::Http(413, "request body too large"));
        }
        if let Some(outcome) = self.upload_route(&req, now, runtime)? {
            return Ok(outcome);
        }
        if let Some(response) = self.public_deliverable(&req, now)? {
            return Ok(Outcome::Response(response));
        }
        if req.path.starts_with("/api/auth/") {
            return self.auth_request(req, now, runtime);
        }
        if req.method == "GET" && req.path == "/healthz" {
            return Ok(Outcome::Response(Response::json(
                200,
                &http::object(&[
                    ("status", Value::string("ok")?),
                    ("version", Value::string(env!("CARGO_PKG_VERSION"))?),
                    ("storage", self.storage_report.try_clone()?),
                ])?,
            )?));
        }
        if worker_api::worker_path(req.path) && self.valid_worker(&req) {
            if req.method == "GET" && req.path == "/api/runner/ws" {
                if !auth::origin(&req) {
                    return Err(Error::Http(403, "invalid request origin"));
                }
                return Ok(Outcome::Runner(self.worker_link()));
            }
            return self
                .worker_route(&req, now, runtime)?
                .map(Outcome::Response)
                .ok_or(Error::Http(404, "not found"));
        }
        let identity = self
            .identity(&req, now)?
            .ok_or(Error::Http(401, "authentication required"))?;
        if req.method == "GET" && req.path == "/api/state/ws" {
            if !auth::origin(&req) {
                return Err(Error::Http(403, "invalid request origin"));
            }
            return Ok(Outcome::State(StateWatch {
                token_hash: identity.1.token_hash,
            }));
        }
        if req.is_mutation() {
            auth::check_csrf(&req, &identity.1)?;
        }
        if let Some(response) = self.replica_route(&req, &identity.0, now, runtime)? {
            return Ok(Outcome::Response(response));
        }
        if let Some(outcome) =
            self.backup_route(&req, &identity.0, &identity.1.token_hash, now, runtime)?
        {
            return Ok(outcome);
        }
        if req.method == "GET" && req.path == "/api/runner/ws" {
            if !auth::origin(&req) {
                return Err(Error::Http(403, "invalid request origin"));
            }
            return Ok(Outcome::Runner(Self::browser_runner(identity.1.token_hash)));
        }
        if let Some(response) = self.worker_route(&req, now, runtime)? {
            return Ok(Outcome::Response(response));
        }
        if let Some(outcome) =
            self.terminal_route(&req, &identity.0.username, &identity.1.token_hash, runtime)?
        {
            return Ok(outcome);
        }
        if let Some(outcome) = self.chat_route(
            &req,
            &identity.0.username,
            &identity.1.token_hash,
            now,
            runtime,
        )? {
            return Ok(outcome);
        }
        if let Some(outcome) = self.human_accept_route(&req, &identity.0.username, now, runtime)? {
            return Ok(outcome);
        }
        if let Some(response) = self.attachment_route(&req, &identity.0.username, now, runtime)? {
            return Ok(Outcome::Response(response));
        }
        if let Some(response) = self.deliverable_download(&req)? {
            return Ok(Outcome::Response(response));
        }
        if let Some(outcome) = self.code_route(&req, &identity.0.username, now, runtime)? {
            return Ok(outcome);
        }
        if let Some(outcome) =
            self.oauth_route(&req, &identity.0, &identity.1.token_hash, now, runtime)?
        {
            return Ok(outcome);
        }
        if let Some(outcome) = self.app_route(&req, &identity.0.username, now, runtime)? {
            return Ok(outcome);
        }
        if let Some(outcome) = self.login_operation_route(&req, &identity.0, now, runtime)? {
            return Ok(outcome);
        }
        if let Some(response) = self.management_route(&req, &identity.0)? {
            return Ok(Outcome::Response(response));
        }
        if let Some(outcome) = self.artifact_operation_route(&req, &identity.0, now, runtime)? {
            return Ok(outcome);
        }
        if let Some(outcome) = self.operation_route(&req, &identity.0.username, now, runtime)? {
            return Ok(outcome);
        }
        if let Some(response) = self.options_route(&req, &identity.0.username, now, runtime)? {
            return Ok(Outcome::Response(response));
        }
        if let Some(outcome) = self.probe_route(&req, &identity.0.username, now, runtime)? {
            return Ok(outcome);
        }
        if let Some(outcome) = self.session_file_route(&req, &identity.0.username, now, runtime)? {
            return Ok(outcome);
        }
        if let Some(outcome) = self.capsule_route(&req, &identity.0.username, now, runtime)? {
            return Ok(outcome);
        }
        Ok(Outcome::Response(
            self.route(req, identity.0, now, runtime)?,
        ))
    }
    /// Voltooit wachtwoordwerk opnieuw onder de Store-eigenaar en controleert intrekkingen.
    pub fn finish_password(
        &mut self,
        work: PasswordWork,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Response> {
        self.finish_auth(work, now, runtime)
    }
    /// Iedere live stream verliest toegang zodra haar browsersessie is ingetrokken.
    pub fn state_for_watch(&mut self, watch: &StateWatch, now: &Timestamp) -> Result<String> {
        let (user, _) = self.store.authenticate_session(&watch.token_hash, now)?;
        Ok(self.state_for(&user)?.to_json()?)
    }
    /// Controleert ook een stille verbinding op verlopen of ingetrokken autorisatie.
    pub fn validate_watch(&mut self, watch: &StateWatch, now: &Timestamp) -> Result {
        self.store.authenticate_session(&watch.token_hash, now)?;
        Ok(())
    }
    fn decode<T: Wire>(req: &Request<'_>) -> Result<T> {
        Ok(T::from_json_strict(req.body, http::MAX_BODY)?)
    }
}

#[cfg(test)]
mod tests;
