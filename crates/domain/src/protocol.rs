//! Gegevenscontracten van het runnerprotocol; transport en uitvoering leven elders.
use crate::{Bytes, List, RawJson, WireMap};
use alloc::string::String;

/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_HELLO: &str = "hello";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_WELCOME: &str = "welcome";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_REQUEST: &str = "request";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_RESPONSE: &str = "response";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_CANCEL: &str = "cancel";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_STREAM_DATA: &str = "stream_data";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_STREAM_INPUT: &str = "stream_input";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_STREAM_RESIZE: &str = "stream_resize";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_STREAM_CLOSE: &str = "stream_close";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_STREAM_EXIT: &str = "stream_exit";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_GOODBYE: &str = "goodbye";
/// Protocolwaarde uit de Go-specificatie.
pub const MESSAGE_EVENT: &str = "event";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_START_RECORDING: &str = "capsule.start_recording";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_ACCEPTS: &str = "capsule.accepts";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_REMOVE_CAPSULES: &str = "capsule.remove_capsules";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_EXECUTE: &str = "capsule.execute";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_SEAL: &str = "capsule.seal";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_CANCEL_RECORDING: &str = "capsule.cancel_recording";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_MATERIALIZE: &str = "capsule.materialize";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_STOP: &str = "capsule.stop";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_PROBE_ENABLED: &str = "capsule.probe_enabled";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_START_ENABLED: &str = "capsule.start_enabled";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_START_INTERACTIVE: &str = "capsule.start_interactive";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_INSPECT_WORKSPACE: &str = "workspace.inspect";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_INSPECT_WORKSPACE_AT: &str = "workspace.inspect_at";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_INSPECT_RANGE: &str = "workspace.inspect_range";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_INJECT_ATTACHMENTS: &str = "workspace.inject_attachments";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_READ_TRACKED: &str = "files.read";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_WATCH_TRACKED: &str = "files.watch";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_TRACKED_CHANGED: &str = "files.changed";
/// Event: de laatste stand van een lang verzoek (opslaan, archiveren, uploaden).
pub const METHOD_PROGRESS: &str = "progress";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_BUNDLE_DELIVERABLE: &str = "deliverable.bundle";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_PLACE_DELIVERABLE: &str = "deliverable.place";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_CAPSULE_CHANGES: &str = "capsule.changes";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_WRITE_TRACKED: &str = "files.write";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_ACCEPT_WORKSPACE: &str = "workspace.accept";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_ACCEPT_REPOSITORY: &str = "repository.accept";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_MERGE_REPOSITORY: &str = "repository.merge";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_MERGE_WORKSPACE: &str = "workspace.merge";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_SYNC_WORKSPACE: &str = "workspace.sync";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_BROWSE_REPOSITORY: &str = "repository.browse";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_COMPARE_REPOSITORY: &str = "repository.compare";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_PULL_SNAPSHOT: &str = "snapshot.pull";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_REMOVE_SNAPSHOT: &str = "snapshot.remove";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_EXPORT_SNAPSHOT: &str = "snapshot.export";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_IMPORT_SNAPSHOT: &str = "snapshot.import";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_ARCHIVE_SNAPSHOT: &str = "snapshot.archive";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_HAS_SNAPSHOT: &str = "snapshot.has";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_START_APP: &str = "app.start";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_STOP_APP: &str = "app.stop";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_APP_STATUS: &str = "app.status";
/// Protocolwaarde uit de Go-specificatie.
pub const METHOD_APP_LOGS: &str = "app.logs";
/// Protocolwaarde uit de Go-specificatie.
pub const RUNNER_FULL: &str = "runner full: no room for another capsule";
/// Protocolwaarde uit de Go-specificatie.
pub const SNAPSHOT_MODE_PULL: &str = "docker-image-pull";
model! {
    /// Het veldcontract van `WireMessage`.
    WireMessage {
        /// `version` in het bestaande JSON-contract.
        version: i64 => ("version", true),
        /// `type` in het bestaande JSON-contract.
        r#type: String => ("type", false),
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", true),
        /// `method` in het bestaande JSON-contract.
        method: String => ("method", true),
        /// `instance_id` in het bestaande JSON-contract.
        instance_id: String => ("instance_id", true),
        /// `process` in het bestaande JSON-contract.
        process: String => ("process", true),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", true),
        /// `capabilities` in het bestaande JSON-contract.
        capabilities: crate::ClientCapabilities => ("capabilities", true),
        /// `client` in het bestaande JSON-contract.
        client: Option<crate::Client> => ("client", true),
        /// `payload` in het bestaande JSON-contract.
        payload: RawJson => ("payload", true),
        /// `data` in het bestaande JSON-contract.
        data: Bytes => ("data", true),
        /// `rows` in het bestaande JSON-contract.
        rows: u16 => ("rows", true),
        /// `cols` in het bestaande JSON-contract.
        cols: u16 => ("cols", true),
        /// `execution` in het bestaande JSON-contract.
        execution: Option<crate::engine::Execution> => ("execution", true),
        /// `error` in het bestaande JSON-contract.
        error: String => ("error", true),
        /// `idle` in het bestaande JSON-contract.
        idle: bool => ("idle", true),
        /// `streams` in het bestaande JSON-contract.
        streams: List<String> => ("streams", true),
        /// `streams_reported` in het bestaande JSON-contract.
        streams_reported: bool => ("streams_reported", true),
        /// `capsules` in het bestaande JSON-contract.
        capsules: Option<crate::engine::LiveCapsules> => ("capsules", true),
    }
}

model! {
    /// Het veldcontract van `AcceptsReply`.
    AcceptsReply {
        /// `accepts` in het bestaande JSON-contract.
        accepts: bool => ("accepts", false),
        /// `running` in het bestaande JSON-contract.
        running: i64 => ("running", false),
        /// `limit` in het bestaande JSON-contract.
        limit: i64 => ("limit", false),
    }
}

model! {
    /// Het veldcontract van `RemoveCapsulesPayload`.
    RemoveCapsulesPayload {
        /// `compositions` in het bestaande JSON-contract.
        compositions: List<String> => ("compositions", false),
        /// `recordings` in het bestaande JSON-contract.
        recordings: List<String> => ("recordings", false),
    }
}

model! {
    /// Het veldcontract van `StartRecordingPayload`.
    StartRecordingPayload {
        /// `recording` in het bestaande JSON-contract.
        recording: crate::Recording => ("recording", false),
        /// `parents` in het bestaande JSON-contract.
        parents: List<crate::Artifact> => ("parents", false),
        /// `stack` in het bestaande JSON-contract.
        stack: Option<crate::engine::RecordingStack> => ("stack", true),
    }
}

model! {
    /// Het veldcontract van `RecordingPayload`.
    RecordingPayload {
        /// `recording` in het bestaande JSON-contract.
        recording: crate::Recording => ("recording", false),
    }
}

model! {
    /// Het veldcontract van `ExecutePayload`.
    ExecutePayload {
        /// `recording` in het bestaande JSON-contract.
        recording: crate::Recording => ("recording", false),
        /// `input` in het bestaande JSON-contract.
        input: String => ("input", false),
    }
}

model! {
    /// Het veldcontract van `MaterializePayload`.
    MaterializePayload {
        /// `composition` in het bestaande JSON-contract.
        composition: crate::Composition => ("composition", false),
        /// `artifacts` in het bestaande JSON-contract.
        artifacts: List<crate::Artifact> => ("artifacts", false),
        /// `authentication` in het bestaande JSON-contract.
        authentication: Option<crate::engine::GitAuthentication> => ("authentication", true),
    }
}

model! {
    /// Het veldcontract van `RuntimePayload`.
    RuntimePayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
    }
}

model! {
    /// Het veldcontract van `WorkspacePathPayload`.
    WorkspacePathPayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", true),
    }
}

model! {
    /// Het veldcontract van `MergePayload`.
    MergePayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
        /// `merge` in het bestaande JSON-contract.
        merge: crate::engine::WorkspaceMerge => ("merge", false),
    }
}

model! {
    /// Het veldcontract van `SyncPayload`.
    SyncPayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
        /// `sync` in het bestaande JSON-contract.
        sync: crate::engine::WorkspaceSync => ("sync", false),
    }
}

model! {
    /// Het veldcontract van `RepositoryBrowsePayload`.
    RepositoryBrowsePayload {
        /// `browse` in het bestaande JSON-contract.
        browse: crate::engine::RepositoryBrowse => ("browse", false),
    }
}

model! {
    /// Het veldcontract van `RepositoryComparePayload`.
    RepositoryComparePayload {
        /// `comparison` in het bestaande JSON-contract.
        comparison: crate::engine::RepositoryComparison => ("comparison", false),
    }
}

model! {
    /// Het veldcontract van `RepositoryAcceptPayload`.
    RepositoryAcceptPayload {
        /// `acceptance` in het bestaande JSON-contract.
        acceptance: crate::engine::RepositoryAcceptance => ("acceptance", false),
    }
}

model! {
    /// Het veldcontract van `RepositoryMergePayload`.
    RepositoryMergePayload {
        /// `merge` in het bestaande JSON-contract.
        merge: crate::engine::RepositoryMerge => ("merge", false),
    }
}

model! {
    /// Het veldcontract van `AppPayload`.
    AppPayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", true),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", false),
        /// `services` in het bestaande JSON-contract.
        services: List<crate::AppService> => ("services", true),
        /// `hosts` in het bestaande JSON-contract.
        hosts: List<String> => ("hosts", true),
        /// `service` in het bestaande JSON-contract.
        service: String => ("service", true),
        /// `tail` in het bestaande JSON-contract.
        tail: i64 => ("tail", true),
    }
}

model! {
    /// Het veldcontract van `AppStatusResult`.
    AppStatusResult {
        /// `services` in het bestaande JSON-contract.
        services: List<crate::AppServiceRuntime> => ("services", false),
    }
}

model! {
    /// Het veldcontract van `AppLogsResult`.
    AppLogsResult {
        /// `output` in het bestaande JSON-contract.
        output: String => ("output", false),
    }
}

model! {
    /// Het veldcontract van `EnabledPayload`.
    EnabledPayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
        /// `enablement` in het bestaande JSON-contract.
        enablement: crate::Enablement => ("enablement", false),
        /// `request` in het bestaande JSON-contract.
        request: RawJson => ("request", true),
    }
}

model! {
    /// Het veldcontract van `InteractivePayload`.
    InteractivePayload {
        /// `recording` in het bestaande JSON-contract.
        recording: crate::Recording => ("recording", false),
        /// `input` in het bestaande JSON-contract.
        input: String => ("input", false),
        /// `rows` in het bestaande JSON-contract.
        rows: u16 => ("rows", false),
        /// `cols` in het bestaande JSON-contract.
        cols: u16 => ("cols", false),
    }
}

model! {
    /// Het veldcontract van `InspectRangePayload`.
    InspectRangePayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
        /// `comparison` in het bestaande JSON-contract.
        comparison: crate::engine::WorkspaceComparison => ("comparison", false),
    }
}

model! {
    /// Het veldcontract van `AttachmentPayload`.
    AttachmentPayload {
        /// `target_path` in het bestaande JSON-contract.
        target_path: String => ("target_path", false),
        /// `data` in het bestaande JSON-contract.
        data: Bytes => ("data", false),
    }
}

model! {
    /// Het veldcontract van `BundleDeliverablePayload`.
    BundleDeliverablePayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", false),
    }
}

model! {
    /// Het veldcontract van `PlaceDeliverablePayload`.
    PlaceDeliverablePayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
        /// `target` in het bestaande JSON-contract.
        target: String => ("target", false),
        /// `bundle` in het bestaande JSON-contract.
        bundle: crate::DeliverableBundle => ("bundle", false),
    }
}

model! {
    /// Het veldcontract van `TrackedFilesPayload`.
    TrackedFilesPayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
        /// `paths` in het bestaande JSON-contract.
        paths: List<String> => ("paths", true),
        /// `excludes` in het bestaande JSON-contract.
        excludes: List<String> => ("excludes", true),
        /// `files` in het bestaande JSON-contract.
        files: WireMap<Bytes> => ("files", true),
    }
}

model! {
    /// Het veldcontract van `InjectAttachmentsPayload`.
    InjectAttachmentsPayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
        /// `attachments` in het bestaande JSON-contract.
        attachments: List<AttachmentPayload> => ("attachments", false),
    }
}

model! {
    /// Het veldcontract van `AcceptWorkspacePayload`.
    AcceptWorkspacePayload {
        /// `runtime` in het bestaande JSON-contract.
        runtime: crate::CapsuleRuntime => ("runtime", false),
        /// `acceptance` in het bestaande JSON-contract.
        acceptance: crate::engine::WorkspaceAcceptance => ("acceptance", false),
    }
}

model! {
    /// Het veldcontract van `SnapshotPayload`.
    SnapshotPayload {
        /// `snapshot` in het bestaande JSON-contract.
        snapshot: crate::CapsuleSnapshot => ("snapshot", false),
    }
}

model! {
    /// Het veldcontract van `SnapshotPullPayload`.
    SnapshotPullPayload {
        /// `snapshot` in het bestaande JSON-contract.
        snapshot: crate::CapsuleSnapshot => ("snapshot", false),
        /// `size` in het bestaande JSON-contract.
        size: i64 => ("size", false),
    }
}

model! {
    /// Het veldcontract van `PresenceResult`.
    PresenceResult {
        /// `present` in het bestaande JSON-contract.
        present: bool => ("present", false),
    }
}

model! {
    /// Het veldcontract van `ArchiveResult`.
    ArchiveResult {
        /// `ref` in het bestaande JSON-contract.
        r#ref: String => ("ref", false),
        /// `digest` in het bestaande JSON-contract.
        digest: String => ("digest", false),
        /// `size` in het bestaande JSON-contract.
        size: i64 => ("size", false),
    }
}

model! {
    /// Het veldcontract van `StreamResponse`.
    StreamResponse {
        /// `stream_id` in het bestaande JSON-contract.
        stream_id: String => ("stream_id", false),
    }
}

/// De versie van het runnerprotocol.
pub const PROTOCOL_VERSION: i64 = 1;
/// De Go-runnerverbinding nam frames van maximaal 24 MiB aan.
pub const MAX_MESSAGE_BYTES: usize = 24 << 20;
impl WireMessage {
    /// Leest één runnerbericht binnen het bestaande framebudget.
    pub fn decode(input: &[u8]) -> crate::Fallible<Self> {
        <Self as crate::Wire>::from_json_with_limit(input, MAX_MESSAGE_BYTES)
    }
    /// Alleen een ondersteunde hello met instance-ID opent een verbinding.
    pub fn is_supported_hello(&self) -> bool {
        self.r#type == MESSAGE_HELLO
            && self.version == PROTOCOL_VERSION
            && !self.instance_id.trim().is_empty()
    }
}
/// Of een runner weigert omdat zijn capsulebudget vol is.
pub fn is_runner_full(error: &str) -> bool {
    error.contains(RUNNER_FULL)
}
