//! De volledige veldcontracten van de state: het JSON-contract dat de database en de browser delen.
//! Regels en I/O blijven buiten deze declaraties.
use crate::{Bytes, List, RawJson, Timestamp, WireMap};
use alloc::string::String;

/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type JobStatus = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type WorkflowStatus = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type PhaseRunStatus = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type WorkflowExecutor = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type SessionStatus = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type ActivationStatus = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type TurnStatus = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type ResultStatus = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type CheckpointKind = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type ForkMode = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type ArtifactKind = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type ArtifactScope = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type CredentialScope = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type ArtifactSensitivity = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type RecordingStatus = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type RepositoryMode = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type MCPTransport = String;
/// Uitbreidbare tekstwaarde uit het Spin-protocol.
pub type UserRole = String;
/// Protocolwaarde uit de Go-specificatie.
pub const JOB_ACTIVE: &str = "active";
/// Protocolwaarde uit de Go-specificatie.
pub const JOB_COMPARING: &str = "comparing";
/// Protocolwaarde uit de Go-specificatie.
pub const JOB_REVIEW: &str = "review";
/// Protocolwaarde uit de Go-specificatie.
pub const JOB_DONE: &str = "done";
/// Protocolwaarde uit de Go-specificatie.
pub const JOB_CANCELLED: &str = "cancelled";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_BUSY: &str = "busy";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_PENDING: &str = "pending";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_DONE: &str = "done";
/// Protocolwaarde uit de Go-specificatie.
pub const PHASE_RUN_QUEUED: &str = "queued";
/// Protocolwaarde uit de Go-specificatie.
pub const PHASE_RUN_RUNNING: &str = "running";
/// Protocolwaarde uit de Go-specificatie.
pub const PHASE_RUN_PENDING: &str = "pending";
/// Protocolwaarde uit de Go-specificatie.
pub const PHASE_RUN_ACCEPTED: &str = "accepted";
/// Protocolwaarde uit de Go-specificatie.
pub const PHASE_RUN_REJECTED: &str = "rejected";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_TARGET_NEXT: &str = "NEXT";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_TARGET_SELF: &str = "SELF";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_TARGET_DONE: &str = "DONE";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_TARGET_ASK_USER: &str = "ASK_USER";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_EXECUTOR_AGENT: &str = "agent";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_EXECUTOR_ACTION: &str = "action";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_EXECUTOR_EXPOSE: &str = "expose";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_ACTION_GIT_PULL_REQUEST: &str = "git.pull_request.create";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKFLOW_ACTION_GIT_MERGE: &str = "git.merge";
/// Protocolwaarde uit de Go-specificatie.
pub const DELIVERABLE_KIND_MARKDOWN: &str = "markdown";
/// Protocolwaarde uit de Go-specificatie.
pub const DELIVERABLE_KIND_PDF: &str = "pdf";
/// Protocolwaarde uit de Go-specificatie.
pub const DELIVERABLE_KIND_IMAGE: &str = "image";
/// Protocolwaarde uit de Go-specificatie.
pub const DELIVERABLE_KIND_FOLDER: &str = "folder";
/// Protocolwaarde uit de Go-specificatie.
pub const DELIVERABLE_KIND_FILE: &str = "file";
/// Protocolwaarde uit de Go-specificatie.
pub const DELIVERABLE_DIRECTORY: &str = "/root/deliverables";
/// Protocolwaarde uit de Go-specificatie.
pub const SESSION_QUEUED: &str = "queued";
/// Protocolwaarde uit de Go-specificatie.
pub const SESSION_CLAIMED: &str = "claimed";
/// Protocolwaarde uit de Go-specificatie.
pub const SESSION_RUNNING: &str = "running";
/// Protocolwaarde uit de Go-specificatie.
pub const SESSION_FROZEN: &str = "frozen";
/// Protocolwaarde uit de Go-specificatie.
pub const SESSION_COMPLETED: &str = "completed";
/// Protocolwaarde uit de Go-specificatie.
pub const SESSION_CANCELLED: &str = "cancelled";
/// Protocolwaarde uit de Go-specificatie.
pub const ACTIVATION_CLAIMED: &str = "claimed";
/// Protocolwaarde uit de Go-specificatie.
pub const ACTIVATION_RUNNING: &str = "running";
/// Protocolwaarde uit de Go-specificatie.
pub const ACTIVATION_ENDED: &str = "ended";
/// Protocolwaarde uit de Go-specificatie.
pub const TURN_RUNNING: &str = "running";
/// Protocolwaarde uit de Go-specificatie.
pub const TURN_COMPLETED: &str = "completed";
/// Protocolwaarde uit de Go-specificatie.
pub const RESULT_SUCCESS: &str = "success";
/// Protocolwaarde uit de Go-specificatie.
pub const RESULT_PARTIAL: &str = "partial";
/// Protocolwaarde uit de Go-specificatie.
pub const RESULT_FAILED: &str = "failed";
/// Protocolwaarde uit de Go-specificatie.
pub const CHECKPOINT_BASELINE: &str = "baseline";
/// Protocolwaarde uit de Go-specificatie.
pub const CHECKPOINT_SESSION_START: &str = "session_start";
/// Protocolwaarde uit de Go-specificatie.
pub const CHECKPOINT_TURN_END: &str = "turn_end";
/// Protocolwaarde uit de Go-specificatie.
pub const CHECKPOINT_MANUAL: &str = "manual";
/// Protocolwaarde uit de Go-specificatie.
pub const CHECKPOINT_RESULT: &str = "result";
/// Protocolwaarde uit de Go-specificatie.
pub const CHECKPOINT_CRASH: &str = "crash";
/// Protocolwaarde uit de Go-specificatie.
pub const FORK_FULL: &str = "full";
/// Protocolwaarde uit de Go-specificatie.
pub const FORK_FILESYSTEM: &str = "filesystem";
/// Protocolwaarde uit de Go-specificatie.
pub const FORK_RESULT: &str = "result";
/// Protocolwaarde uit de Go-specificatie.
pub const FORK_ROOT: &str = "root";
/// Protocolwaarde uit de Go-specificatie.
pub const FORK_CRITIC: &str = "critic";
/// Protocolwaarde uit de Go-specificatie.
pub const FORK_SYNTHESIS: &str = "synthesis";
/// Protocolwaarde uit de Go-specificatie.
pub const ARTIFACT_TOOL: &str = "tool";
/// Protocolwaarde uit de Go-specificatie.
pub const ARTIFACT_CREDENTIAL: &str = "credential";
/// Protocolwaarde uit de Go-specificatie.
pub const ARTIFACT_CONFIG: &str = "config";
/// Protocolwaarde uit de Go-specificatie.
pub const ARTIFACT_WORKSPACE: &str = "workspace";
/// Protocolwaarde uit de Go-specificatie.
pub const ARTIFACT_SESSION: &str = "session";
/// Protocolwaarde uit de Go-specificatie.
pub const ARTIFACT_RESULT: &str = "result";
/// Protocolwaarde uit de Go-specificatie.
pub const SCOPE_GLOBAL: &str = "global";
/// Protocolwaarde uit de Go-specificatie.
pub const SCOPE_TEAM: &str = "team";
/// Protocolwaarde uit de Go-specificatie.
pub const SCOPE_PROJECT: &str = "project";
/// Protocolwaarde uit de Go-specificatie.
pub const SCOPE_USER: &str = "user";
/// Protocolwaarde uit de Go-specificatie.
pub const CREDENTIAL_SCOPE_USER: &str = "user";
/// Protocolwaarde uit de Go-specificatie.
pub const CREDENTIAL_SCOPE_GLOBAL: &str = "global";
/// Protocolwaarde uit de Go-specificatie.
pub const CREDENTIAL_SCOPE_PUBLIC: &str = "public";
/// Protocolwaarde uit de Go-specificatie.
pub const SENSITIVITY_PUBLIC: &str = "public";
/// Protocolwaarde uit de Go-specificatie.
pub const SENSITIVITY_PRIVATE: &str = "private";
/// Protocolwaarde uit de Go-specificatie.
pub const SENSITIVITY_SECRET: &str = "secret";
/// Protocolwaarde uit de Go-specificatie.
pub const RECORDING_OPEN: &str = "recording";
/// Protocolwaarde uit de Go-specificatie.
pub const RECORDING_COMPLETED: &str = "completed";
/// Protocolwaarde uit de Go-specificatie.
pub const RECORDING_CANCELLED: &str = "cancelled";
/// Protocolwaarde uit de Go-specificatie.
pub const REPOSITORY_MODE_CHANGE: &str = "change";
/// Protocolwaarde uit de Go-specificatie.
pub const REPOSITORY_MODE_REFERENCE: &str = "reference";
/// Protocolwaarde uit de Go-specificatie.
pub const WORKSPACE_ROOT: &str = "/workspace";
/// Protocolwaarde uit de Go-specificatie.
pub const MCP_TRANSPORT_STDIO: &str = "stdio";
/// Protocolwaarde uit de Go-specificatie.
pub const MCP_TRANSPORT_HTTP: &str = "http";
/// Protocolwaarde uit de Go-specificatie.
pub const USER_ADMIN: &str = "admin";
/// Protocolwaarde uit de Go-specificatie.
pub const USER_MEMBER: &str = "member";
/// Protocolwaarde uit de Go-specificatie.
pub const BRAINSTORM_PHASE_ID: &str = "brainstorm";
model! {
    /// Het veldcontract van `WorkflowAction`.
    WorkflowAction {
        /// `type` in het bestaande JSON-contract.
        r#type: String => ("type", false),
    }
}

model! {
    /// Het veldcontract van `WorkflowTransition`.
    WorkflowTransition {
        /// `target` in het bestaande JSON-contract.
        target: String => ("target", false),
        /// `ask_user` in het bestaande JSON-contract.
        ask_user: bool => ("ask_user", true),
        /// `max` in het bestaande JSON-contract.
        max: i64 => ("max", true),
        /// `exhausted` in het bestaande JSON-contract.
        exhausted: String => ("exhausted", true),
    }
}

model! {
    /// Het veldcontract van `DeliverableDefinition`.
    DeliverableDefinition {
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `description` in het bestaande JSON-contract.
        description: String => ("description", true),
        /// `required` in het bestaande JSON-contract.
        required: bool => ("required", false),
        /// `kind` in het bestaande JSON-contract.
        kind: String => ("kind", true),
    }
}

model! {
    /// Het veldcontract van `DeliverableBundle`.
    DeliverableBundle {
        /// `ref` in het bestaande JSON-contract.
        r#ref: String => ("ref", false),
        /// `digest` in het bestaande JSON-contract.
        digest: String => ("digest", false),
        /// `size` in het bestaande JSON-contract.
        size: i64 => ("size", false),
        /// `files` in het bestaande JSON-contract.
        files: i64 => ("files", false),
        /// `folder` in het bestaande JSON-contract.
        folder: bool => ("folder", false),
        /// `entry` in het bestaande JSON-contract.
        entry: String => ("entry", true),
        /// `content_type` in het bestaande JSON-contract.
        content_type: String => ("content_type", true),
    }
}

model! {
    /// Het veldcontract van `WorkflowPhase`.
    WorkflowPhase {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `instructions` in het bestaande JSON-contract.
        instructions: String => ("instructions", false),
        /// `executor` in het bestaande JSON-contract.
        executor: WorkflowExecutor => ("executor", true),
        /// `environment_selector` in het bestaande JSON-contract.
        environment_selector: String => ("environment_selector", true),
        /// `with_selectors` in het bestaande JSON-contract.
        with_selectors: List<String> => ("with_selectors", true),
        /// `action` in het bestaande JSON-contract.
        action: Option<WorkflowAction> => ("action", true),
        /// `inject` in het bestaande JSON-contract.
        inject: List<String> => ("inject", false),
        /// `deliverables` in het bestaande JSON-contract.
        deliverables: List<DeliverableDefinition> => ("deliverables", false),
        /// `allow_changes` in het bestaande JSON-contract.
        allow_changes: bool => ("allow_changes", false),
        /// `resolve_merge` in het bestaande JSON-contract.
        resolve_merge: bool => ("resolve_merge", true),
        /// `model` in het bestaande JSON-contract.
        model: String => ("model", true),
        /// `reasoning_effort` in het bestaande JSON-contract.
        reasoning_effort: String => ("reasoning_effort", true),
        /// `accept` in het bestaande JSON-contract.
        accept: WorkflowTransition => ("accept", false),
        /// `reject` in het bestaande JSON-contract.
        reject: WorkflowTransition => ("reject", false),
    }
}

model! {
    /// Het veldcontract van `WorkflowTemplate`.
    WorkflowTemplate {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `revision` in het bestaande JSON-contract.
        revision: i64 => ("revision", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `description` in het bestaande JSON-contract.
        description: String => ("description", true),
        /// `git_selector` in het bestaande JSON-contract.
        git_selector: String => ("git_selector", true),
        /// `created_by` in het bestaande JSON-contract.
        created_by: String => ("created_by", false),
        /// `phases` in het bestaande JSON-contract.
        phases: List<WorkflowPhase> => ("phases", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het veldcontract van `PhaseRun`.
    PhaseRun {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `job_id` in het bestaande JSON-contract.
        job_id: String => ("job_id", false),
        /// `template_id` in het bestaande JSON-contract.
        template_id: String => ("template_id", false),
        /// `phase_id` in het bestaande JSON-contract.
        phase_id: String => ("phase_id", false),
        /// `phase_name` in het bestaande JSON-contract.
        phase_name: String => ("phase_name", false),
        /// `attempt` in het bestaande JSON-contract.
        attempt: i64 => ("attempt", false),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", false),
        /// `status` in het bestaande JSON-contract.
        status: PhaseRunStatus => ("status", false),
        /// `pending_reason` in het bestaande JSON-contract.
        pending_reason: String => ("pending_reason", true),
        /// `pending_outcome` in het bestaande JSON-contract.
        pending_outcome: String => ("pending_outcome", true),
        /// `summary` in het bestaande JSON-contract.
        summary: String => ("summary", true),
        /// `reject_reason` in het bestaande JSON-contract.
        reject_reason: String => ("reject_reason", true),
        /// `agent_outcomes` in het bestaande JSON-contract.
        agent_outcomes: List<WorkflowAgentOutcome> => ("agent_outcomes", true),
        /// `action_result` in het bestaande JSON-contract.
        action_result: Option<WorkflowActionResult> => ("action_result", true),
        /// `restarts` in het bestaande JSON-contract.
        restarts: i64 => ("restarts", true),
        /// `restart_notes` in het bestaande JSON-contract.
        restart_notes: List<String> => ("restart_notes", true),
        /// `restart_transcript` in het bestaande JSON-contract.
        restart_transcript: List<ChatLine> => ("restart_transcript", true),
        /// `started_at` in het bestaande JSON-contract.
        started_at: Timestamp => ("started_at", false),
        /// `completed_at` in het bestaande JSON-contract.
        completed_at: Option<Timestamp> => ("completed_at", true),
    }
}

model! {
    /// Het veldcontract van `ChatLine`.
    ChatLine {
        /// `role` in het bestaande JSON-contract.
        role: String => ("role", false),
        /// `text` in het bestaande JSON-contract.
        text: String => ("text", false),
    }
}

model! {
    /// Het veldcontract van `WorkflowActionResult`.
    WorkflowActionResult {
        /// `type` in het bestaande JSON-contract.
        r#type: String => ("type", false),
        /// `external_id` in het bestaande JSON-contract.
        external_id: String => ("external_id", true),
        /// `url` in het bestaande JSON-contract.
        url: String => ("url", true),
        /// `detail` in het bestaande JSON-contract.
        detail: String => ("detail", true),
        /// `results` in het bestaande JSON-contract.
        results: WireMap<String> => ("results", true),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `WorkflowAgentOutcome`.
    WorkflowAgentOutcome {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `outcome` in het bestaande JSON-contract.
        outcome: String => ("outcome", false),
        /// `detail` in het bestaande JSON-contract.
        detail: String => ("detail", true),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `Deliverable`.
    Deliverable {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `job_id` in het bestaande JSON-contract.
        job_id: String => ("job_id", false),
        /// `phase_run_id` in het bestaande JSON-contract.
        phase_run_id: String => ("phase_run_id", false),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `description` in het bestaande JSON-contract.
        description: String => ("description", true),
        /// `content` in het bestaande JSON-contract.
        content: String => ("content", false),
        /// `revision` in het bestaande JSON-contract.
        revision: i64 => ("revision", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", true),
        /// `kind` in het bestaande JSON-contract.
        kind: String => ("kind", true),
        /// `bundle` in het bestaande JSON-contract.
        bundle: Option<DeliverableBundle> => ("bundle", true),
        /// `share_token` in het bestaande JSON-contract.
        share_token: String => ("share_token", true),
        /// `share_expires_at` in het bestaande JSON-contract.
        share_expires_at: Option<Timestamp> => ("share_expires_at", true),
        /// `-` in het bestaande JSON-contract.
        preview_token: String => ("-", false),
        /// `-` in het bestaande JSON-contract.
        preview_expires_at: Option<Timestamp> => ("-", false),
    }
}

model! {
    /// Het veldcontract van `DeliverableComment`.
    DeliverableComment {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `deliverable_id` in het bestaande JSON-contract.
        deliverable_id: String => ("deliverable_id", false),
        /// `selected_text` in het bestaande JSON-contract.
        selected_text: String => ("selected_text", false),
        /// `start_offset` in het bestaande JSON-contract.
        start_offset: i64 => ("start_offset", false),
        /// `end_offset` in het bestaande JSON-contract.
        end_offset: i64 => ("end_offset", false),
        /// `prefix` in het bestaande JSON-contract.
        prefix: String => ("prefix", true),
        /// `suffix` in het bestaande JSON-contract.
        suffix: String => ("suffix", true),
        /// `body` in het bestaande JSON-contract.
        body: String => ("body", false),
        /// `author` in het bestaande JSON-contract.
        author: String => ("author", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `CodeReviewRevision`.
    CodeReviewRevision {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `job_id` in het bestaande JSON-contract.
        job_id: String => ("job_id", false),
        /// `source_phase_run_id` in het bestaande JSON-contract.
        source_phase_run_id: String => ("source_phase_run_id", true),
        /// `context_phase_run_id` in het bestaande JSON-contract.
        context_phase_run_id: String => ("context_phase_run_id", true),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", true),
        /// `phase_id` in het bestaande JSON-contract.
        phase_id: String => ("phase_id", true),
        /// `phase_name` in het bestaande JSON-contract.
        phase_name: String => ("phase_name", true),
        /// `attempt` in het bestaande JSON-contract.
        attempt: i64 => ("attempt", true),
        /// `scope` in het bestaande JSON-contract.
        scope: String => ("scope", false),
        /// `scope_key` in het bestaande JSON-contract.
        scope_key: String => ("scope_key", false),
        /// `live` in het bestaande JSON-contract.
        live: bool => ("live", true),
        /// `branch` in het bestaande JSON-contract.
        branch: String => ("branch", true),
        /// `digest` in het bestaande JSON-contract.
        digest: String => ("digest", false),
        /// `added` in het bestaande JSON-contract.
        added: i64 => ("added", false),
        /// `deleted` in het bestaande JSON-contract.
        deleted: i64 => ("deleted", false),
        /// `files` in het bestaande JSON-contract.
        files: List<CodeReviewFile> => ("files", false),
        /// `created_by` in het bestaande JSON-contract.
        created_by: String => ("created_by", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `CodeReviewFile`.
    CodeReviewFile {
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", false),
        /// `status` in het bestaande JSON-contract.
        status: String => ("status", false),
        /// `repository` in het bestaande JSON-contract.
        repository: String => ("repository", true),
        /// `folder` in het bestaande JSON-contract.
        folder: String => ("folder", true),
        /// `head` in het bestaande JSON-contract.
        head: String => ("head", true),
        /// `added` in het bestaande JSON-contract.
        added: i64 => ("added", false),
        /// `deleted` in het bestaande JSON-contract.
        deleted: i64 => ("deleted", false),
        /// `patch` in het bestaande JSON-contract.
        patch: String => ("patch", true),
        /// `binary` in het bestaande JSON-contract.
        binary: bool => ("binary", true),
        /// `truncated` in het bestaande JSON-contract.
        truncated: bool => ("truncated", true),
    }
}

model! {
    /// Het veldcontract van `CodeReviewRevisionSummary`.
    CodeReviewRevisionSummary {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `job_id` in het bestaande JSON-contract.
        job_id: String => ("job_id", false),
        /// `source_phase_run_id` in het bestaande JSON-contract.
        source_phase_run_id: String => ("source_phase_run_id", true),
        /// `context_phase_run_id` in het bestaande JSON-contract.
        context_phase_run_id: String => ("context_phase_run_id", true),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", true),
        /// `phase_id` in het bestaande JSON-contract.
        phase_id: String => ("phase_id", true),
        /// `phase_name` in het bestaande JSON-contract.
        phase_name: String => ("phase_name", true),
        /// `attempt` in het bestaande JSON-contract.
        attempt: i64 => ("attempt", true),
        /// `scope` in het bestaande JSON-contract.
        scope: String => ("scope", false),
        /// `scope_key` in het bestaande JSON-contract.
        scope_key: String => ("scope_key", false),
        /// `branch` in het bestaande JSON-contract.
        branch: String => ("branch", true),
        /// `added` in het bestaande JSON-contract.
        added: i64 => ("added", false),
        /// `deleted` in het bestaande JSON-contract.
        deleted: i64 => ("deleted", false),
        /// `file_count` in het bestaande JSON-contract.
        file_count: i64 => ("file_count", false),
        /// `created_by` in het bestaande JSON-contract.
        created_by: String => ("created_by", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `CodeReviewComment`.
    CodeReviewComment {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `revision_id` in het bestaande JSON-contract.
        revision_id: String => ("revision_id", false),
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", false),
        /// `side` in het bestaande JSON-contract.
        side: String => ("side", false),
        /// `start_line` in het bestaande JSON-contract.
        start_line: i64 => ("start_line", false),
        /// `end_line` in het bestaande JSON-contract.
        end_line: i64 => ("end_line", false),
        /// `selected_text` in het bestaande JSON-contract.
        selected: String => ("selected_text", false),
        /// `body` in het bestaande JSON-contract.
        body: String => ("body", false),
        /// `author` in het bestaande JSON-contract.
        author: String => ("author", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `WorkflowQuestionItem`.
    WorkflowQuestionItem {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `question` in het bestaande JSON-contract.
        question: String => ("question", false),
        /// `options` in het bestaande JSON-contract.
        options: List<String> => ("options", true),
        /// `answer` in het bestaande JSON-contract.
        answer: String => ("answer", true),
        /// `other` in het bestaande JSON-contract.
        other: bool => ("other", true),
    }
}

model! {
    /// Het veldcontract van `WorkflowQuestionAnswer`.
    WorkflowQuestionAnswer {
        /// `item_id` in het bestaande JSON-contract.
        item_id: String => ("item_id", false),
        /// `answer` in het bestaande JSON-contract.
        answer: String => ("answer", false),
    }
}

model! {
    /// Het veldcontract van `WorkflowQuestion`.
    WorkflowQuestion {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `job_id` in het bestaande JSON-contract.
        job_id: String => ("job_id", false),
        /// `phase_run_id` in het bestaande JSON-contract.
        phase_run_id: String => ("phase_run_id", false),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", false),
        /// `kind` in het bestaande JSON-contract.
        kind: String => ("kind", false),
        /// `question` in het bestaande JSON-contract.
        question: String => ("question", false),
        /// `items` in het bestaande JSON-contract.
        items: List<WorkflowQuestionItem> => ("items", true),
        /// `outcome` in het bestaande JSON-contract.
        outcome: String => ("outcome", true),
        /// `agent_detail` in het bestaande JSON-contract.
        agent_detail: String => ("agent_detail", true),
        /// `agent_outcome_id` in het bestaande JSON-contract.
        agent_outcome_id: String => ("agent_outcome_id", true),
        /// `accept_target` in het bestaande JSON-contract.
        accept_target: String => ("accept_target", true),
        /// `reject_target` in het bestaande JSON-contract.
        reject_target: String => ("reject_target", true),
        /// `answer` in het bestaande JSON-contract.
        answer: String => ("answer", true),
        /// `reason` in het bestaande JSON-contract.
        reason: String => ("reason", true),
        /// `answered_by` in het bestaande JSON-contract.
        answered_by: String => ("answered_by", true),
        /// `status` in het bestaande JSON-contract.
        status: String => ("status", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `answered_at` in het bestaande JSON-contract.
        answered_at: Option<Timestamp> => ("answered_at", true),
    }
}

model! {
    /// Het veldcontract van `LayerContents`.
    LayerContents {
        /// `files` in het bestaande JSON-contract.
        files: i64 => ("files", false),
        /// `bytes` in het bestaande JSON-contract.
        bytes: i64 => ("bytes", false),
        /// `dropped_identical` in het bestaande JSON-contract.
        dropped_identical: ContentTotal => ("dropped_identical", true),
        /// `entries` in het bestaande JSON-contract.
        entries: List<ContentEntry> => ("entries", true),
    }
}

model! {
    /// Het veldcontract van `ContentEntry`.
    ContentEntry {
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", false),
        /// `bytes` in het bestaande JSON-contract.
        bytes: i64 => ("bytes", false),
        /// `logins` in het bestaande JSON-contract.
        logins: List<i64> => ("logins", true),
        /// `source` in het bestaande JSON-contract.
        source: String => ("source", true),
    }
}

model! {
    /// Het veldcontract van `ContentTotal`.
    ContentTotal {
        /// `files` in het bestaande JSON-contract.
        files: i64 => ("files", true),
        /// `bytes` in het bestaande JSON-contract.
        bytes: i64 => ("bytes", true),
    }
}

model! {
    /// Het veldcontract van `CapsuleSnapshot`.
    CapsuleSnapshot {
        /// `driver` in het bestaande JSON-contract.
        driver: String => ("driver", false),
        /// `client_id` in het bestaande JSON-contract.
        client_id: String => ("client_id", true),
        /// `replica_client_ids` in het bestaande JSON-contract.
        replica_client_ids: List<String> => ("replica_client_ids", true),
        /// `ref` in het bestaande JSON-contract.
        r#ref: String => ("ref", true),
        /// `digest` in het bestaande JSON-contract.
        digest: String => ("digest", false),
        /// `rootfs` in het bestaande JSON-contract.
        root_fs: String => ("rootfs", true),
        /// `restorable` in het bestaande JSON-contract.
        restorable: bool => ("restorable", false),
        /// `includes_process_state` in het bestaande JSON-contract.
        includes_process_state: bool => ("includes_process_state", false),
        /// `contents` in het bestaande JSON-contract.
        contents: Option<LayerContents> => ("contents", true),
        /// `parent_ref` in het bestaande JSON-contract.
        parent_ref: String => ("parent_ref", true),
        /// `delta` in het bestaande JSON-contract.
        delta: bool => ("delta", true),
        /// `content` in het bestaande JSON-contract.
        content: String => ("content", true),
    }
}

model! {
    /// Het veldcontract van `CapsuleRuntime`.
    CapsuleRuntime {
        /// `driver` in het bestaande JSON-contract.
        driver: String => ("driver", false),
        /// `client_id` in het bestaande JSON-contract.
        client_id: String => ("client_id", true),
        /// `container_id` in het bestaande JSON-contract.
        container_id: String => ("container_id", true),
        /// `container_name` in het bestaande JSON-contract.
        container_name: String => ("container_name", true),
        /// `base_ref` in het bestaande JSON-contract.
        base_ref: String => ("base_ref", true),
        /// `parent_ref` in het bestaande JSON-contract.
        parent_ref: String => ("parent_ref", true),
        /// `workspace_ref` in het bestaande JSON-contract.
        workspace_ref: String => ("workspace_ref", true),
        /// `attach_command` in het bestaande JSON-contract.
        attach_command: String => ("attach_command", true),
        /// `status` in het bestaande JSON-contract.
        status: String => ("status", false),
        /// `stop_pending` in het bestaande JSON-contract.
        stop_pending: bool => ("stop_pending", true),
    }
}

model! {
    /// Het veldcontract van `CapsuleEngineInfo`.
    CapsuleEngineInfo {
        /// `driver` in het bestaande JSON-contract.
        driver: String => ("driver", false),
        /// `available` in het bestaande JSON-contract.
        available: bool => ("available", false),
        /// `base_image` in het bestaande JSON-contract.
        base_image: String => ("base_image", true),
        /// `filesystem_snapshots` in het bestaande JSON-contract.
        filesystem_snapshots: bool => ("filesystem_snapshots", false),
        /// `process_checkpoints` in het bestaande JSON-contract.
        process_checkpoints: bool => ("process_checkpoints", false),
        /// `interactive_attach_command` in het bestaande JSON-contract.
        interactive_attach_command: bool => ("interactive_attach_command", false),
        /// `detail` in het bestaande JSON-contract.
        detail: String => ("detail", true),
    }
}

model! {
    /// Het veldcontract van `Enablement`.
    Enablement {
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `command` in het bestaande JSON-contract.
        command: String => ("command", true),
        /// `transport` in het bestaande JSON-contract.
        transport: String => ("transport", true),
        /// `protocol_version` in het bestaande JSON-contract.
        protocol_version: i64 => ("protocol_version", true),
    }
}

model! {
    /// Het veldcontract van `AgentOption`.
    AgentOption {
        /// `value` in het bestaande JSON-contract.
        value: String => ("value", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", true),
        /// `description` in het bestaande JSON-contract.
        description: String => ("description", true),
    }
}

model! {
    /// Het veldcontract van `Login`.
    Login {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `key` in het bestaande JSON-contract.
        key: String => ("key", false),
        /// `number` in het bestaande JSON-contract.
        number: i64 => ("number", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", true),
        /// `disabled` in het bestaande JSON-contract.
        disabled: bool => ("disabled", true),
        /// `owner` in het bestaande JSON-contract.
        owner: String => ("owner", true),
        /// `files` in het bestaande JSON-contract.
        files: WireMap<Bytes> => ("files", false),
        /// `last_used_at` in het bestaande JSON-contract.
        last_used_at: Option<Timestamp> => ("last_used_at", true),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het veldcontract van `LoginFile`.
    LoginFile {
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", false),
        /// `size` in het bestaande JSON-contract.
        size: i64 => ("size", false),
    }
}

model! {
    /// Het veldcontract van `LoginSummary`.
    LoginSummary {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `key` in het bestaande JSON-contract.
        key: String => ("key", false),
        /// `number` in het bestaande JSON-contract.
        number: i64 => ("number", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", true),
        /// `disabled` in het bestaande JSON-contract.
        disabled: bool => ("disabled", true),
        /// `files` in het bestaande JSON-contract.
        files: i64 => ("files", false),
        /// `bytes` in het bestaande JSON-contract.
        bytes: i64 => ("bytes", false),
        /// `owner` in het bestaande JSON-contract.
        owner: String => ("owner", true),
        /// `last_used_at` in het bestaande JSON-contract.
        last_used_at: Option<Timestamp> => ("last_used_at", true),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
        /// `composition_id` in het bestaande JSON-contract.
        composition_id: String => ("composition_id", true),
    }
}

model! {
    /// Het veldcontract van `AgentSettings`.
    AgentSettings {
        /// `mode` in het bestaande JSON-contract.
        mode: String => ("mode", true),
        /// `model` in het bestaande JSON-contract.
        model: String => ("model", true),
        /// `reasoning_effort` in het bestaande JSON-contract.
        reasoning_effort: String => ("reasoning_effort", true),
        /// `auto_accept` in het bestaande JSON-contract.
        auto_accept: Option<bool> => ("auto_accept", true),
    }
}

model! {
    /// Het veldcontract van `AgentOptions`.
    AgentOptions {
        /// `agent_name` in het bestaande JSON-contract.
        agent_name: String => ("agent_name", true),
        /// `models` in het bestaande JSON-contract.
        models: List<AgentOption> => ("models", true),
        /// `reasoning_efforts` in het bestaande JSON-contract.
        reasoning_efforts: List<AgentOption> => ("reasoning_efforts", true),
        /// `modes` in het bestaande JSON-contract.
        modes: List<AgentOption> => ("modes", true),
        /// `fetched_at` in het bestaande JSON-contract.
        fetched_at: Timestamp => ("fetched_at", false),
        /// `error` in het bestaande JSON-contract.
        error: String => ("error", true),
        /// `fetching` in het bestaande JSON-contract.
        fetching: bool => ("fetching", true),
    }
}

model! {
    /// Het veldcontract van `Artifact`.
    Artifact {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `kind` in het bestaande JSON-contract.
        kind: ArtifactKind => ("kind", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `scope` in het bestaande JSON-contract.
        scope: ArtifactScope => ("scope", false),
        /// `subject` in het bestaande JSON-contract.
        subject: String => ("subject", true),
        /// `profile` in het bestaande JSON-contract.
        profile: String => ("profile", false),
        /// `provides` in het bestaande JSON-contract.
        provides: List<String> => ("provides", false),
        /// `requires` in het bestaande JSON-contract.
        requires: List<String> => ("requires", false),
        /// `enables` in het bestaande JSON-contract.
        enables: List<Enablement> => ("enables", true),
        /// `slot` in het bestaande JSON-contract.
        slot: String => ("slot", true),
        /// `parent_artifact_ids` in het bestaande JSON-contract.
        parent_artifact_ids: List<String> => ("parent_artifact_ids", false),
        /// `snapshot_digest` in het bestaande JSON-contract.
        snapshot_digest: String => ("snapshot_digest", false),
        /// `snapshot` in het bestaande JSON-contract.
        snapshot: CapsuleSnapshot => ("snapshot", false),
        /// `compatibility_fingerprint` in het bestaande JSON-contract.
        compatibility_fingerprint: String => ("compatibility_fingerprint", true),
        /// `sensitivity` in het bestaande JSON-contract.
        sensitivity: ArtifactSensitivity => ("sensitivity", false),
        /// `created_by` in het bestaande JSON-contract.
        created_by: String => ("created_by", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `agent_options` in het bestaande JSON-contract.
        agent_options: Option<AgentOptions> => ("agent_options", true),
        /// `tracked_paths` in het bestaande JSON-contract.
        tracked_paths: List<String> => ("tracked_paths", true),
        /// `tracked_excludes` in het bestaande JSON-contract.
        tracked_excludes: List<String> => ("tracked_excludes", true),
        /// `agent_settings` in het bestaande JSON-contract.
        agent_settings: Option<AgentSettings> => ("agent_settings", true),
        /// `snapshot_pruned_at` in het bestaande JSON-contract.
        snapshot_pruned_at: Option<Timestamp> => ("snapshot_pruned_at", true),
        /// `superseded_by` in het bestaande JSON-contract.
        superseded_by: String => ("superseded_by", true),
    }
}

model! {
    /// Het veldcontract van `RecordingCommand`.
    RecordingCommand {
        /// `sequence` in het bestaande JSON-contract.
        sequence: i64 => ("sequence", false),
        /// `exit_code` in het bestaande JSON-contract.
        exit_code: Option<i64> => ("exit_code", true),
        /// `at` in het bestaande JSON-contract.
        at: Timestamp => ("at", false),
    }
}

model! {
    /// Het veldcontract van `Recording`.
    Recording {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `actor` in het bestaande JSON-contract.
        actor: String => ("actor", false),
        /// `kind` in het bestaande JSON-contract.
        kind: ArtifactKind => ("kind", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `scope` in het bestaande JSON-contract.
        scope: ArtifactScope => ("scope", false),
        /// `subject` in het bestaande JSON-contract.
        subject: String => ("subject", true),
        /// `profile` in het bestaande JSON-contract.
        profile: String => ("profile", false),
        /// `provides` in het bestaande JSON-contract.
        provides: List<String> => ("provides", false),
        /// `requires` in het bestaande JSON-contract.
        requires: List<String> => ("requires", false),
        /// `enables` in het bestaande JSON-contract.
        enables: List<Enablement> => ("enables", true),
        /// `slot` in het bestaande JSON-contract.
        slot: String => ("slot", true),
        /// `parent_artifact_ids` in het bestaande JSON-contract.
        parent_artifact_ids: List<String> => ("parent_artifact_ids", false),
        /// `runtime` in het bestaande JSON-contract.
        runtime: Option<CapsuleRuntime> => ("runtime", true),
        /// `compatibility_fingerprint` in het bestaande JSON-contract.
        compatibility_fingerprint: String => ("compatibility_fingerprint", true),
        /// `sensitivity` in het bestaande JSON-contract.
        sensitivity: ArtifactSensitivity => ("sensitivity", false),
        /// `status` in het bestaande JSON-contract.
        status: RecordingStatus => ("status", false),
        /// `commands` in het bestaande JSON-contract.
        commands: List<RecordingCommand> => ("commands", false),
        /// `artifact_id` in het bestaande JSON-contract.
        artifact_id: String => ("artifact_id", true),
        /// `replaces_artifact_id` in het bestaande JSON-contract.
        replaces_artifact_id: String => ("replaces_artifact_id", true),
        /// `started_at` in het bestaande JSON-contract.
        started_at: Timestamp => ("started_at", false),
        /// `ended_at` in het bestaande JSON-contract.
        ended_at: Option<Timestamp> => ("ended_at", true),
    }
}

model! {
    /// Het veldcontract van `ResolvedArtifact`.
    ResolvedArtifact {
        /// `artifact_id` in het bestaande JSON-contract.
        artifact_id: String => ("artifact_id", false),
        /// `kind` in het bestaande JSON-contract.
        kind: String => ("kind", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `slot` in het bestaande JSON-contract.
        slot: String => ("slot", true),
        /// `scope` in het bestaande JSON-contract.
        scope: String => ("scope", false),
        /// `subject` in het bestaande JSON-contract.
        subject: String => ("subject", true),
        /// `profile` in het bestaande JSON-contract.
        profile: String => ("profile", true),
        /// `enables` in het bestaande JSON-contract.
        enables: List<Enablement> => ("enables", true),
        /// `reason` in het bestaande JSON-contract.
        reason: String => ("reason", false),
    }
}

model! {
    /// Het veldcontract van `JobRepository`.
    JobRepository {
        /// `repository_id` in het bestaande JSON-contract.
        repository_id: String => ("repository_id", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `remote_url` in het bestaande JSON-contract.
        remote_url: String => ("remote_url", false),
        /// `provider` in het bestaande JSON-contract.
        provider: String => ("provider", true),
        /// `credential_scope` in het bestaande JSON-contract.
        credential_scope: CredentialScope => ("credential_scope", true),
        /// `base_ref` in het bestaande JSON-contract.
        base_ref: String => ("base_ref", false),
        /// `mode` in het bestaande JSON-contract.
        mode: RepositoryMode => ("mode", false),
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", true),
    }
}

model! {
    /// Het veldcontract van `JobRepositoryRequest`.
    JobRepositoryRequest {
        /// `repository_id` in het bestaande JSON-contract.
        repository_id: String => ("repository_id", false),
        /// `mode` in het bestaande JSON-contract.
        mode: RepositoryMode => ("mode", true),
        /// `base_ref` in het bestaande JSON-contract.
        base_ref: String => ("base_ref", true),
    }
}

model! {
    /// Het veldcontract van `GitWorkspace`.
    GitWorkspace {
        /// `repository_id` in het bestaande JSON-contract.
        repository_id: String => ("repository_id", false),
        /// `repository_name` in het bestaande JSON-contract.
        repository_name: String => ("repository_name", false),
        /// `remote_url` in het bestaande JSON-contract.
        remote_url: String => ("remote_url", false),
        /// `base_ref` in het bestaande JSON-contract.
        base_ref: String => ("base_ref", false),
        /// `bootstrap_ref` in het bestaande JSON-contract.
        bootstrap_ref: String => ("bootstrap_ref", false),
        /// `head_ref` in het bestaande JSON-contract.
        head_ref: String => ("head_ref", false),
        /// `target_ref` in het bestaande JSON-contract.
        target_ref: String => ("target_ref", false),
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", true),
        /// `mode` in het bestaande JSON-contract.
        mode: RepositoryMode => ("mode", true),
        /// `context_refs` in het bestaande JSON-contract.
        context_refs: List<String> => ("context_refs", true),
        /// `merge_ref` in het bestaande JSON-contract.
        merge_ref: String => ("merge_ref", true),
        /// `credential_scope` in het bestaande JSON-contract.
        credential_scope: CredentialScope => ("credential_scope", false),
        /// `account_id` in het bestaande JSON-contract.
        account_id: String => ("account_id", true),
        /// `provider` in het bestaande JSON-contract.
        provider: String => ("provider", true),
        /// `login` in het bestaande JSON-contract.
        login: String => ("login", true),
        /// `author_name` in het bestaande JSON-contract.
        author_name: String => ("author_name", true),
        /// `author_email` in het bestaande JSON-contract.
        author_email: String => ("author_email", true),
    }
}

model! {
    /// Het veldcontract van `Composition`.
    Composition {
        /// Marks a temporary capability probe for cleanup after a server restart.
        probe_artifact_id: String => ("probe_artifact_id", true),
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
        /// `selector` in het bestaande JSON-contract.
        selector: String => ("selector", false),
        /// `entry_artifact_id` in het bestaande JSON-contract.
        entry_artifact_id: String => ("entry_artifact_id", false),
        /// `tool` in het bestaande JSON-contract.
        tool: String => ("tool", true),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", true),
        /// `profile` in het bestaande JSON-contract.
        profile: String => ("profile", false),
        /// `with_selectors` in het bestaande JSON-contract.
        with_selectors: List<String> => ("with_selectors", true),
        /// `requested_artifact_ids` in het bestaande JSON-contract.
        requested_artifact_ids: List<String> => ("requested_artifact_ids", true),
        /// `layers` in het bestaande JSON-contract.
        layers: List<String> => ("layers", true),
        /// `resolved_artifacts` in het bestaande JSON-contract.
        resolved_artifacts: List<ResolvedArtifact> => ("resolved_artifacts", false),
        /// `slot_bindings` in het bestaande JSON-contract.
        slot_bindings: WireMap<String> => ("slot_bindings", false),
        /// `enabled` in het bestaande JSON-contract.
        enabled: List<Enablement> => ("enabled", true),
        /// `mcp_server_ids` in het bestaande JSON-contract.
        mcp_server_ids: List<String> => ("mcp_server_ids", true),
        /// `git` in het bestaande JSON-contract.
        git: Option<GitWorkspace> => ("git", true),
        /// `workspaces` in het bestaande JSON-contract.
        workspaces: List<GitWorkspace> => ("workspaces", true),
        /// `warnings` in het bestaande JSON-contract.
        warnings: List<String> => ("warnings", true),
        /// `runtime` in het bestaande JSON-contract.
        runtime: Option<CapsuleRuntime> => ("runtime", true),
        /// `capsule_changes` in het bestaande JSON-contract.
        capsule_changes: Option<LayerContents> => ("capsule_changes", true),
        /// `logins` in het bestaande JSON-contract.
        logins: WireMap<String> => ("logins", true),
        /// `for_login` in het bestaande JSON-contract.
        for_login: bool => ("for_login", true),
        /// `for_login_private` in het bestaande JSON-contract.
        for_login_private: bool => ("for_login_private", true),
        /// `agent` in het bestaande JSON-contract.
        agent: Option<AgentProcess> => ("agent", true),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `AgentProcess`.
    AgentProcess {
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", false),
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
        /// `stream_id` in het bestaande JSON-contract.
        stream_id: String => ("stream_id", false),
        /// `agent_session_id` in het bestaande JSON-contract.
        agent_session_id: String => ("agent_session_id", false),
        /// `agent_name` in het bestaande JSON-contract.
        agent_name: String => ("agent_name", true),
        /// `protocol_version` in het bestaande JSON-contract.
        protocol_version: i64 => ("protocol_version", true),
        /// `steering` in het bestaande JSON-contract.
        steering: bool => ("steering", true),
        /// `prompt_caps` in het bestaande JSON-contract.
        prompt_caps: RawJson => ("prompt_caps", true),
        /// `settings` in het bestaande JSON-contract.
        settings: RawJson => ("settings", true),
        /// `auto_accept` in het bestaande JSON-contract.
        auto_accept: bool => ("auto_accept", false),
        /// `primed` in het bestaande JSON-contract.
        primed: bool => ("primed", true),
        /// `prompt_id` in het bestaande JSON-contract.
        prompt_id: String => ("prompt_id", true),
        /// `sent_attachments` in het bestaande JSON-contract.
        sent_attachments: List<String> => ("sent_attachments", true),
        /// `pending_permissions` in het bestaande JSON-contract.
        pending_permissions: WireMap<RawJson> => ("pending_permissions", true),
    }
}

model! {
    /// Het veldcontract van `Job`.
    Job {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `forked_from_job_id` in het bestaande JSON-contract.
        forked_from_job_id: String => ("forked_from_job_id", true),
        /// `title` in het bestaande JSON-contract.
        title: String => ("title", false),
        /// `reference` in het bestaande JSON-contract.
        reference: String => ("reference", true),
        /// `objective` in het bestaande JSON-contract.
        objective: String => ("objective", false),
        /// `acceptance_criteria` in het bestaande JSON-contract.
        acceptance_criteria: List<String> => ("acceptance_criteria", true),
        /// `owner` in het bestaande JSON-contract.
        owner: String => ("owner", true),
        /// `assignee` in het bestaande JSON-contract.
        assignee: String => ("assignee", true),
        /// `git_repository_id` in het bestaande JSON-contract.
        git_repository_id: String => ("git_repository_id", false),
        /// `git_repository_name` in het bestaande JSON-contract.
        git_repository_name: String => ("git_repository_name", true),
        /// `git_remote_url` in het bestaande JSON-contract.
        git_remote_url: String => ("git_remote_url", true),
        /// `git_provider` in het bestaande JSON-contract.
        git_provider: String => ("git_provider", true),
        /// `git_credential_scope` in het bestaande JSON-contract.
        git_credential_scope: CredentialScope => ("git_credential_scope", true),
        /// `base_ref` in het bestaande JSON-contract.
        base_ref: String => ("base_ref", true),
        /// `branch` in het bestaande JSON-contract.
        branch: String => ("branch", false),
        /// `repositories` in het bestaande JSON-contract.
        repositories: List<JobRepository> => ("repositories", true),
        /// `with_selectors` in het bestaande JSON-contract.
        with_selectors: List<String> => ("with_selectors", true),
        /// `mcp_server_ids` in het bestaande JSON-contract.
        mcp_server_ids: List<String> => ("mcp_server_ids", true),
        /// `attachment_ids` in het bestaande JSON-contract.
        attachment_ids: List<String> => ("attachment_ids", true),
        /// `template_id` in het bestaande JSON-contract.
        template_id: String => ("template_id", true),
        /// `template_snapshot` in het bestaande JSON-contract.
        template_snapshot: Option<WorkflowTemplate> => ("template_snapshot", true),
        /// `environment_selector` in het bestaande JSON-contract.
        environment_selector: String => ("environment_selector", true),
        /// `model` in het bestaande JSON-contract.
        model: String => ("model", true),
        /// `phase_run_ids` in het bestaande JSON-contract.
        phase_run_ids: List<String> => ("phase_run_ids", true),
        /// `current_phase_run_id` in het bestaande JSON-contract.
        current_phase_run_id: String => ("current_phase_run_id", true),
        /// `workflow_status` in het bestaande JSON-contract.
        workflow_status: WorkflowStatus => ("workflow_status", true),
        /// `pending_reason` in het bestaande JSON-contract.
        pending_reason: String => ("pending_reason", true),
        /// `status` in het bestaande JSON-contract.
        status: JobStatus => ("status", false),
        /// `session_ids` in het bestaande JSON-contract.
        session_ids: List<String> => ("session_ids", false),
        /// `candidate_result_ids` in het bestaande JSON-contract.
        candidate_result_ids: List<String> => ("candidate_result_ids", false),
        /// `final_result_id` in het bestaande JSON-contract.
        final_result_id: String => ("final_result_id", true),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het veldcontract van `JobAttachment`.
    JobAttachment {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `job_id` in het bestaande JSON-contract.
        job_id: String => ("job_id", true),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `media_type` in het bestaande JSON-contract.
        media_type: String => ("media_type", false),
        /// `size` in het bestaande JSON-contract.
        size: i64 => ("size", false),
        /// `sha256` in het bestaande JSON-contract.
        sha256: String => ("sha256", false),
        /// `capsule_path` in het bestaande JSON-contract.
        capsule_path: String => ("capsule_path", false),
        /// `created_by` in het bestaande JSON-contract.
        created_by: String => ("created_by", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `Session`.
    Session {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `job_id` in het bestaande JSON-contract.
        job_id: String => ("job_id", false),
        /// `phase_run_id` in het bestaande JSON-contract.
        phase_run_id: String => ("phase_run_id", true),
        /// `parent_session_id` in het bestaande JSON-contract.
        parent_session_id: String => ("parent_session_id", true),
        /// `spawned_by_session_id` in het bestaande JSON-contract.
        spawned_by_session_id: String => ("spawned_by_session_id", true),
        /// `parent_checkpoint_id` in het bestaande JSON-contract.
        parent_checkpoint_id: String => ("parent_checkpoint_id", true),
        /// `input_result_ids` in het bestaande JSON-contract.
        input_result_ids: List<String> => ("input_result_ids", true),
        /// `fork_mode` in het bestaande JSON-contract.
        fork_mode: ForkMode => ("fork_mode", false),
        /// `tool` in het bestaande JSON-contract.
        tool: String => ("tool", false),
        /// `executor` in het bestaande JSON-contract.
        executor: WorkflowExecutor => ("executor", true),
        /// `environment_selector` in het bestaande JSON-contract.
        environment_selector: String => ("environment_selector", true),
        /// `with_selectors` in het bestaande JSON-contract.
        with_selectors: List<String> => ("with_selectors", true),
        /// `mcp_server_ids` in het bestaande JSON-contract.
        mcp_server_ids: List<String> => ("mcp_server_ids", true),
        /// `role` in het bestaande JSON-contract.
        role: String => ("role", true),
        /// `model` in het bestaande JSON-contract.
        model: String => ("model", true),
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", true),
        /// `prepared_composition_id` in het bestaande JSON-contract.
        prepared_composition_id: String => ("prepared_composition_id", true),
        /// `objective_delta` in het bestaande JSON-contract.
        objective_delta: String => ("objective_delta", true),
        /// `git_repository_id` in het bestaande JSON-contract.
        git_repository_id: String => ("git_repository_id", false),
        /// `base_ref` in het bestaande JSON-contract.
        base_ref: String => ("base_ref", true),
        /// `git_ref` in het bestaande JSON-contract.
        git_ref: String => ("git_ref", false),
        /// `target_branch` in het bestaande JSON-contract.
        target_branch: String => ("target_branch", false),
        /// `synced_head` in het bestaande JSON-contract.
        synced_head: String => ("synced_head", true),
        /// `synced_at` in het bestaande JSON-contract.
        synced_at: Option<Timestamp> => ("synced_at", true),
        /// `status` in het bestaande JSON-contract.
        status: SessionStatus => ("status", false),
        /// `client_id` in het bestaande JSON-contract.
        client_id: String => ("client_id", true),
        /// `activation_id` in het bestaande JSON-contract.
        activation_id: String => ("activation_id", true),
        /// `activation_epoch` in het bestaande JSON-contract.
        activation_epoch: i64 => ("activation_epoch", false),
        /// `lease_expires_at` in het bestaande JSON-contract.
        lease_expires_at: Option<Timestamp> => ("lease_expires_at", true),
        /// `current_checkpoint_id` in het bestaande JSON-contract.
        current_checkpoint_id: String => ("current_checkpoint_id", true),
        /// `turn_ids` in het bestaande JSON-contract.
        turn_ids: List<String> => ("turn_ids", false),
        /// `checkpoint_ids` in het bestaande JSON-contract.
        checkpoint_ids: List<String> => ("checkpoint_ids", false),
        /// `final_result_id` in het bestaande JSON-contract.
        final_result_id: String => ("final_result_id", true),
        /// `continuity_level` in het bestaande JSON-contract.
        continuity_level: String => ("continuity_level", false),
        /// `continuity_score` in het bestaande JSON-contract.
        continuity_score: i64 => ("continuity_score", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het veldcontract van `Turn`.
    Turn {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", false),
        /// `activation_id` in het bestaande JSON-contract.
        activation_id: String => ("activation_id", false),
        /// `activation_epoch` in het bestaande JSON-contract.
        activation_epoch: i64 => ("activation_epoch", false),
        /// `sequence` in het bestaande JSON-contract.
        sequence: i64 => ("sequence", false),
        /// `input` in het bestaande JSON-contract.
        input: String => ("input", false),
        /// `actor` in het bestaande JSON-contract.
        actor: String => ("actor", true),
        /// `credential_bindings` in het bestaande JSON-contract.
        credential_bindings: WireMap<String> => ("credential_bindings", true),
        /// `status` in het bestaande JSON-contract.
        status: TurnStatus => ("status", false),
        /// `checkpoint_id` in het bestaande JSON-contract.
        checkpoint_id: String => ("checkpoint_id", true),
        /// `started_at` in het bestaande JSON-contract.
        started_at: Timestamp => ("started_at", false),
        /// `ended_at` in het bestaande JSON-contract.
        ended_at: Option<Timestamp> => ("ended_at", true),
    }
}

model! {
    /// Het veldcontract van `Activation`.
    Activation {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", false),
        /// `client_id` in het bestaande JSON-contract.
        client_id: String => ("client_id", false),
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", true),
        /// `composition_id` in het bestaande JSON-contract.
        composition_id: String => ("composition_id", true),
        /// `credential_bindings` in het bestaande JSON-contract.
        credential_bindings: WireMap<String> => ("credential_bindings", true),
        /// `epoch` in het bestaande JSON-contract.
        epoch: i64 => ("epoch", false),
        /// `status` in het bestaande JSON-contract.
        status: ActivationStatus => ("status", false),
        /// `reason` in het bestaande JSON-contract.
        reason: String => ("reason", true),
        /// `started_at` in het bestaande JSON-contract.
        started_at: Timestamp => ("started_at", false),
        /// `ended_at` in het bestaande JSON-contract.
        ended_at: Option<Timestamp> => ("ended_at", true),
    }
}

model! {
    /// Het veldcontract van `CapsuleManifest`.
    CapsuleManifest {
        /// `image_digest` in het bestaande JSON-contract.
        image_digest: String => ("image_digest", true),
        /// `filesystem_snapshot_digest` in het bestaande JSON-contract.
        filesystem_snapshot_digest: String => ("filesystem_snapshot_digest", true),
        /// `process_checkpoint_digest` in het bestaande JSON-contract.
        process_checkpoint_digest: String => ("process_checkpoint_digest", true),
        /// `git_head` in het bestaande JSON-contract.
        git_head: String => ("git_head", true),
        /// `git_dirty` in het bestaande JSON-contract.
        git_dirty: bool => ("git_dirty", false),
        /// `agent_session_id` in het bestaande JSON-contract.
        agent_session_id: String => ("agent_session_id", true),
        /// `event_sequence` in het bestaande JSON-contract.
        event_sequence: i64 => ("event_sequence", false),
        /// `compatibility_fingerprint` in het bestaande JSON-contract.
        compatibility_fingerprint: String => ("compatibility_fingerprint", true),
        /// `external_effects_watermark` in het bestaande JSON-contract.
        external_effects_watermark: i64 => ("external_effects_watermark", false),
        /// `restorable` in het bestaande JSON-contract.
        restorable: bool => ("restorable", false),
        /// `unrestorable_reason` in het bestaande JSON-contract.
        unrestorable_reason: String => ("unrestorable_reason", true),
    }
}

model! {
    /// Het veldcontract van `Checkpoint`.
    Checkpoint {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", false),
        /// `activation_id` in het bestaande JSON-contract.
        activation_id: String => ("activation_id", false),
        /// `activation_epoch` in het bestaande JSON-contract.
        activation_epoch: i64 => ("activation_epoch", false),
        /// `turn_id` in het bestaande JSON-contract.
        turn_id: String => ("turn_id", true),
        /// `parent_checkpoint_id` in het bestaande JSON-contract.
        parent_checkpoint_id: String => ("parent_checkpoint_id", true),
        /// `sequence` in het bestaande JSON-contract.
        sequence: i64 => ("sequence", false),
        /// `kind` in het bestaande JSON-contract.
        kind: CheckpointKind => ("kind", false),
        /// `summary` in het bestaande JSON-contract.
        summary: String => ("summary", true),
        /// `capsule` in het bestaande JSON-contract.
        capsule: CapsuleManifest => ("capsule", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `TestEvidence`.
    TestEvidence {
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `passed` in het bestaande JSON-contract.
        passed: bool => ("passed", false),
        /// `details` in het bestaande JSON-contract.
        details: String => ("details", true),
    }
}

model! {
    /// Het veldcontract van `CriterionEvidence`.
    CriterionEvidence {
        /// `criterion` in het bestaande JSON-contract.
        criterion: String => ("criterion", false),
        /// `met` in het bestaande JSON-contract.
        met: bool => ("met", false),
        /// `evidence` in het bestaande JSON-contract.
        evidence: String => ("evidence", true),
    }
}

model! {
    /// Het veldcontract van `Usage`.
    Usage {
        /// `wall_time_ms` in het bestaande JSON-contract.
        wall_time_ms: i64 => ("wall_time_ms", true),
        /// `input_tokens` in het bestaande JSON-contract.
        input_tokens: i64 => ("input_tokens", true),
        /// `output_tokens` in het bestaande JSON-contract.
        output_tokens: i64 => ("output_tokens", true),
        /// `cache_read_tokens` in het bestaande JSON-contract.
        cache_read_tokens: i64 => ("cache_read_tokens", true),
        /// `cache_write_tokens` in het bestaande JSON-contract.
        cache_write_tokens: i64 => ("cache_write_tokens", true),
    }
}

model! {
    /// Het veldcontract van `Result`.
    Result {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `job_id` in het bestaande JSON-contract.
        job_id: String => ("job_id", false),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", false),
        /// `checkpoint_id` in het bestaande JSON-contract.
        checkpoint_id: String => ("checkpoint_id", false),
        /// `status` in het bestaande JSON-contract.
        status: ResultStatus => ("status", false),
        /// `summary` in het bestaande JSON-contract.
        summary: String => ("summary", false),
        /// `git_head` in het bestaande JSON-contract.
        git_head: String => ("git_head", true),
        /// `tests` in het bestaande JSON-contract.
        tests: List<TestEvidence> => ("tests", true),
        /// `acceptance_evidence` in het bestaande JSON-contract.
        acceptance_evidence: List<CriterionEvidence> => ("acceptance_evidence", true),
        /// `open_issues` in het bestaande JSON-contract.
        open_issues: List<String> => ("open_issues", true),
        /// `usage` in het bestaande JSON-contract.
        usage: Usage => ("usage", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `ClientCapabilities`.
    ClientCapabilities {
        /// `os` in het bestaande JSON-contract.
        os: String => ("os", true),
        /// `arch` in het bestaande JSON-contract.
        arch: String => ("arch", true),
        /// `tools` in het bestaande JSON-contract.
        tools: List<String> => ("tools", false),
        /// `snapshot_modes` in het bestaande JSON-contract.
        snapshot_modes: List<String> => ("snapshot_modes", true),
        /// `engine` in het bestaande JSON-contract.
        engine: CapsuleEngineInfo => ("engine", false),
        /// `max_workloads` in het bestaande JSON-contract.
        max_workloads: i64 => ("max_workloads", true),
    }
}

model! {
    /// Het veldcontract van `Client`.
    Client {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `instance_id` in het bestaande JSON-contract.
        instance_id: String => ("instance_id", true),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `capabilities` in het bestaande JSON-contract.
        capabilities: ClientCapabilities => ("capabilities", false),
        /// `status` in het bestaande JSON-contract.
        status: String => ("status", false),
        /// `draining` in het bestaande JSON-contract.
        draining: bool => ("draining", true),
        /// `last_seen_at` in het bestaande JSON-contract.
        last_seen_at: Timestamp => ("last_seen_at", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `MCPSecret`.
    MCPSecret {
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `value` in het bestaande JSON-contract.
        value: String => ("value", true),
    }
}

model! {
    /// Het veldcontract van `MCPServer`.
    MCPServer {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `transport` in het bestaande JSON-contract.
        transport: MCPTransport => ("transport", false),
        /// `command` in het bestaande JSON-contract.
        command: String => ("command", true),
        /// `args` in het bestaande JSON-contract.
        args: List<String> => ("args", true),
        /// `url` in het bestaande JSON-contract.
        url: String => ("url", true),
        /// `env` in het bestaande JSON-contract.
        env: List<MCPSecret> => ("env", true),
        /// `headers` in het bestaande JSON-contract.
        headers: List<MCPSecret> => ("headers", true),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `GitRepository`.
    GitRepository {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `remote_url` in het bestaande JSON-contract.
        remote_url: String => ("remote_url", false),
        /// `default_ref` in het bestaande JSON-contract.
        default_ref: String => ("default_ref", false),
        /// `provider` in het bestaande JSON-contract.
        provider: String => ("provider", false),
        /// `credential_scope` in het bestaande JSON-contract.
        credential_scope: CredentialScope => ("credential_scope", false),
        /// `layer_selectors` in het bestaande JSON-contract.
        layer_selectors: List<String> => ("layer_selectors", true),
        /// `services` in het bestaande JSON-contract.
        services: List<AppService> => ("services", true),
        /// `service_hosts` in het bestaande JSON-contract.
        service_hosts: List<String> => ("service_hosts", true),
        /// `created_by` in het bestaande JSON-contract.
        created_by: String => ("created_by", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het veldcontract van `AppService`.
    AppService {
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `image` in het bestaande JSON-contract.
        image: String => ("image", true),
        /// `prepare` in het bestaande JSON-contract.
        prepare: List<String> => ("prepare", true),
        /// `run` in het bestaande JSON-contract.
        run: String => ("run", true),
        /// `ports` in het bestaande JSON-contract.
        ports: List<i64> => ("ports", true),
        /// `env` in het bestaande JSON-contract.
        env: String => ("env", true),
    }
}

model! {
    /// Het veldcontract van `AppServiceRuntime`.
    AppServiceRuntime {
        /// `service` in het bestaande JSON-contract.
        service: String => ("service", false),
        /// `container_id` in het bestaande JSON-contract.
        container_id: String => ("container_id", true),
        /// `status` in het bestaande JSON-contract.
        status: String => ("status", false),
        /// `host` in het bestaande JSON-contract.
        host: String => ("host", true),
        /// `ports` in het bestaande JSON-contract.
        ports: WireMap<i64> => ("ports", true),
        /// `reachable` in het bestaande JSON-contract.
        reachable: bool => ("reachable", false),
        /// `error` in het bestaande JSON-contract.
        error: String => ("error", true),
        /// `started_at` in het bestaande JSON-contract.
        started_at: Option<Timestamp> => ("started_at", true),
    }
}

model! {
    /// Het veldcontract van `GitAccount`.
    GitAccount {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
        /// `provider` in het bestaande JSON-contract.
        provider: String => ("provider", false),
        /// `host` in het bestaande JSON-contract.
        host: String => ("host", false),
        /// `provider_id` in het bestaande JSON-contract.
        provider_id: String => ("provider_id", true),
        /// `login` in het bestaande JSON-contract.
        login: String => ("login", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", true),
        /// `email` in het bestaande JSON-contract.
        email: String => ("email", true),
        /// `access_token` in het bestaande JSON-contract.
        access_token: String => ("access_token", true),
        /// `refresh_token` in het bestaande JSON-contract.
        refresh_token: String => ("refresh_token", true),
        /// `token_type` in het bestaande JSON-contract.
        token_type: String => ("token_type", true),
        /// `scope` in het bestaande JSON-contract.
        scope: String => ("scope", true),
        /// `credential_scope` in het bestaande JSON-contract.
        credential_scope: CredentialScope => ("credential_scope", false),
        /// `expires_at` in het bestaande JSON-contract.
        expires_at: Option<Timestamp> => ("expires_at", true),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het veldcontract van `User`.
    User {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `username` in het bestaande JSON-contract.
        username: String => ("username", false),
        /// `display_name` in het bestaande JSON-contract.
        display_name: String => ("display_name", false),
        /// `role` in het bestaande JSON-contract.
        role: UserRole => ("role", false),
        /// `password_hash` in het bestaande JSON-contract.
        password_hash: String => ("password_hash", false),
        /// `archived_at` in het bestaande JSON-contract.
        archived_at: Option<Timestamp> => ("archived_at", true),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het veldcontract van `PublicUser`.
    PublicUser {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `username` in het bestaande JSON-contract.
        username: String => ("username", false),
        /// `display_name` in het bestaande JSON-contract.
        display_name: String => ("display_name", false),
        /// `role` in het bestaande JSON-contract.
        role: UserRole => ("role", false),
        /// `archived_at` in het bestaande JSON-contract.
        archived_at: Option<Timestamp> => ("archived_at", true),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
    }
}

model! {
    /// Het veldcontract van `AuthSession`.
    AuthSession {
        /// `id` in het bestaande JSON-contract.
        id: String => ("id", false),
        /// `user_id` in het bestaande JSON-contract.
        user_id: String => ("user_id", false),
        /// `token_hash` in het bestaande JSON-contract.
        token_hash: String => ("token_hash", false),
        /// `csrf_hash` in het bestaande JSON-contract.
        csrf_hash: String => ("csrf_hash", false),
        /// `expires_at` in het bestaande JSON-contract.
        expires_at: Timestamp => ("expires_at", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `last_seen_at` in het bestaande JSON-contract.
        last_seen_at: Timestamp => ("last_seen_at", false),
    }
}

model! {
    /// Het veldcontract van `GitOAuthConfiguration`.
    GitOAuthConfiguration {
        /// `provider` in het bestaande JSON-contract.
        provider: String => ("provider", false),
        /// `client_id` in het bestaande JSON-contract.
        client_id: String => ("client_id", false),
        /// `client_secret` in het bestaande JSON-contract.
        client_secret: String => ("client_secret", true),
        /// `created_by` in het bestaande JSON-contract.
        created_by: String => ("created_by", false),
        /// `created_at` in het bestaande JSON-contract.
        created_at: Timestamp => ("created_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het veldcontract van `Snapshot`.
    Snapshot {
        /// `artifacts` in het bestaande JSON-contract.
        artifacts: List<Artifact> => ("artifacts", false),
        /// `recordings` in het bestaande JSON-contract.
        recordings: List<Recording> => ("recordings", false),
        /// `compositions` in het bestaande JSON-contract.
        compositions: List<Composition> => ("compositions", false),
        /// `jobs` in het bestaande JSON-contract.
        jobs: List<Job> => ("jobs", false),
        /// `job_attachments` in het bestaande JSON-contract.
        job_attachments: List<JobAttachment> => ("job_attachments", false),
        /// `workflow_templates` in het bestaande JSON-contract.
        workflow_templates: List<WorkflowTemplate> => ("workflow_templates", false),
        /// `phase_runs` in het bestaande JSON-contract.
        phase_runs: List<PhaseRun> => ("phase_runs", false),
        /// `deliverables` in het bestaande JSON-contract.
        deliverables: List<Deliverable> => ("deliverables", false),
        /// `deliverable_comments` in het bestaande JSON-contract.
        deliverable_comments: List<DeliverableComment> => ("deliverable_comments", false),
        /// `code_review_revisions` in het bestaande JSON-contract.
        code_review_revisions: List<CodeReviewRevisionSummary> => ("code_review_revisions", false),
        /// `code_review_comments` in het bestaande JSON-contract.
        code_review_comments: List<CodeReviewComment> => ("code_review_comments", false),
        /// `workflow_questions` in het bestaande JSON-contract.
        workflow_questions: List<WorkflowQuestion> => ("workflow_questions", false),
        /// `sessions` in het bestaande JSON-contract.
        sessions: List<Session> => ("sessions", false),
        /// `activations` in het bestaande JSON-contract.
        activations: List<Activation> => ("activations", false),
        /// `turns` in het bestaande JSON-contract.
        turns: List<Turn> => ("turns", false),
        /// `checkpoints` in het bestaande JSON-contract.
        checkpoints: List<Checkpoint> => ("checkpoints", false),
        /// `results` in het bestaande JSON-contract.
        results: List<Result> => ("results", false),
        /// `clients` in het bestaande JSON-contract.
        clients: List<Client> => ("clients", false),
        /// `mcp_servers` in het bestaande JSON-contract.
        mcp_servers: List<MCPServer> => ("mcp_servers", false),
        /// `git_repositories` in het bestaande JSON-contract.
        git_repositories: List<GitRepository> => ("git_repositories", false),
        /// `git_accounts` in het bestaande JSON-contract.
        git_accounts: List<GitAccount> => ("git_accounts", false),
        /// `users` in het bestaande JSON-contract.
        users: List<PublicUser> => ("users", false),
        /// `logins` in het bestaande JSON-contract.
        logins: List<LoginSummary> => ("logins", false),
    }
}

model! {
    /// Het veldcontract van `Recommendation`.
    Recommendation {
        /// `job_id` in het bestaande JSON-contract.
        job_id: String => ("job_id", false),
        /// `action` in het bestaande JSON-contract.
        action: String => ("action", false),
        /// `reason` in het bestaande JSON-contract.
        reason: String => ("reason", false),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", true),
        /// `checkpoint_id` in het bestaande JSON-contract.
        checkpoint_id: String => ("checkpoint_id", true),
        /// `result_ids` in het bestaande JSON-contract.
        result_ids: List<String> => ("result_ids", true),
        /// `priority` in het bestaande JSON-contract.
        priority: i64 => ("priority", false),
    }
}

model! {
    /// Het veldcontract van `CreateRecordingRequest`.
    CreateRecordingRequest {
        /// `actor` in het bestaande JSON-contract.
        actor: String => ("actor", false),
        /// `kind` in het bestaande JSON-contract.
        kind: ArtifactKind => ("kind", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `scope` in het bestaande JSON-contract.
        scope: ArtifactScope => ("scope", true),
        /// `subject` in het bestaande JSON-contract.
        subject: String => ("subject", true),
        /// `profile` in het bestaande JSON-contract.
        profile: String => ("profile", true),
        /// `provides` in het bestaande JSON-contract.
        provides: List<String> => ("provides", true),
        /// `requires` in het bestaande JSON-contract.
        requires: List<String> => ("requires", true),
        /// `enables` in het bestaande JSON-contract.
        enables: List<Enablement> => ("enables", true),
        /// `slot` in het bestaande JSON-contract.
        slot: String => ("slot", true),
        /// `parent_artifact_ids` in het bestaande JSON-contract.
        parent_artifact_ids: List<String> => ("parent_artifact_ids", true),
        /// `compatibility_fingerprint` in het bestaande JSON-contract.
        compatibility_fingerprint: String => ("compatibility_fingerprint", true),
        /// `sensitivity` in het bestaande JSON-contract.
        sensitivity: ArtifactSensitivity => ("sensitivity", true),
        /// `replaces_artifact_id` in het bestaande JSON-contract.
        replaces_artifact_id: String => ("replaces_artifact_id", true),
    }
}

model! {
    /// Het veldcontract van `ExecuteRecordingCommandRequest`.
    ExecuteRecordingCommandRequest {
        /// `actor` in het bestaande JSON-contract.
        actor: String => ("actor", false),
        /// `input` in het bestaande JSON-contract.
        input: String => ("input", false),
    }
}

model! {
    /// Het veldcontract van `AttachRecordingParentRequest`.
    AttachRecordingParentRequest {
        /// `actor` in het bestaande JSON-contract.
        actor: String => ("actor", false),
        /// `kind` in het bestaande JSON-contract.
        kind: ArtifactKind => ("kind", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
    }
}

model! {
    /// Het veldcontract van `EndRecordingRequest`.
    EndRecordingRequest {
        /// `actor` in het bestaande JSON-contract.
        actor: String => ("actor", false),
        /// `snapshot_digest` in het bestaande JSON-contract.
        snapshot_digest: String => ("snapshot_digest", true),
        /// `snapshot` in het bestaande JSON-contract.
        snapshot: CapsuleSnapshot => ("snapshot", true),
    }
}

model! {
    /// Het veldcontract van `CancelRecordingRequest`.
    CancelRecordingRequest {
        /// `actor` in het bestaande JSON-contract.
        actor: String => ("actor", false),
    }
}

model! {
    /// Het veldcontract van `DeleteArtifactRequest`.
    DeleteArtifactRequest {
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
    }
}

model! {
    /// Het veldcontract van `UseRequest`.
    UseRequest {
        /// `selector` in het bestaande JSON-contract.
        selector: String => ("selector", true),
        /// `with_selectors` in het bestaande JSON-contract.
        with_selectors: List<String> => ("with_selectors", true),
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
        /// `profile` in het bestaande JSON-contract.
        profile: String => ("profile", true),
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", true),
        /// `tool` in het bestaande JSON-contract.
        tool: String => ("tool", true),
        /// `merge_ref` in het bestaande JSON-contract.
        merge_ref: String => ("merge_ref", true),
        /// `for_login` in het bestaande JSON-contract.
        for_login: bool => ("for_login", true),
        /// `for_login_private` in het bestaande JSON-contract.
        for_login_private: bool => ("for_login_private", true),
    }
}

model! {
    /// Het veldcontract van `StopCompositionRequest`.
    StopCompositionRequest {
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
    }
}

model! {
    /// Het veldcontract van `StartStatus`.
    StartStatus {
        /// `recording_id` in het bestaande JSON-contract.
        recording_id: String => ("recording_id", false),
        /// `status` in het bestaande JSON-contract.
        status: String => ("status", false),
        /// `stage` in het bestaande JSON-contract.
        stage: String => ("stage", true),
        /// `message` in het bestaande JSON-contract.
        message: String => ("message", true),
        /// `current` in het bestaande JSON-contract.
        current: i64 => ("current", true),
        /// `total` in het bestaande JSON-contract.
        total: i64 => ("total", true),
        /// `error` in het bestaande JSON-contract.
        error: String => ("error", true),
        /// `recording` in het bestaande JSON-contract.
        recording: Option<Recording> => ("recording", true),
        /// `started_at` in het bestaande JSON-contract.
        started_at: Timestamp => ("started_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het veldcontract van `SealStatus`.
    SealStatus {
        /// `recording_id` in het bestaande JSON-contract.
        recording_id: String => ("recording_id", false),
        /// `status` in het bestaande JSON-contract.
        status: String => ("status", false),
        /// `stage` in het bestaande JSON-contract.
        stage: String => ("stage", true),
        /// `message` in het bestaande JSON-contract.
        message: String => ("message", true),
        /// `current` in het bestaande JSON-contract.
        current: i64 => ("current", true),
        /// `total` in het bestaande JSON-contract.
        total: i64 => ("total", true),
        /// `error` in het bestaande JSON-contract.
        error: String => ("error", true),
        /// `artifact` in het bestaande JSON-contract.
        artifact: Option<Artifact> => ("artifact", true),
        /// `started_at` in het bestaande JSON-contract.
        started_at: Timestamp => ("started_at", false),
        /// `updated_at` in het bestaande JSON-contract.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het veldcontract van `CreateJobRequest`.
    CreateJobRequest {
        /// `title` in het bestaande JSON-contract.
        title: String => ("title", false),
        /// `reference` in het bestaande JSON-contract.
        reference: String => ("reference", true),
        /// `objective` in het bestaande JSON-contract.
        objective: String => ("objective", false),
        /// `brainstorm` in het bestaande JSON-contract.
        brainstorm: bool => ("brainstorm", true),
        /// `forked_from_job_id` in het bestaande JSON-contract.
        forked_from_job_id: String => ("forked_from_job_id", true),
        /// `idempotency_key` in het bestaande JSON-contract.
        idempotency_key: String => ("idempotency_key", true),
        /// `acceptance_criteria` in het bestaande JSON-contract.
        acceptance_criteria: List<String> => ("acceptance_criteria", true),
        /// `owner` in het bestaande JSON-contract.
        owner: String => ("owner", true),
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", true),
        /// `git_repository_id` in het bestaande JSON-contract.
        git_repository_id: String => ("git_repository_id", false),
        /// `base_ref` in het bestaande JSON-contract.
        base_ref: String => ("base_ref", true),
        /// `repositories` in het bestaande JSON-contract.
        repositories: List<JobRepositoryRequest> => ("repositories", true),
        /// `tool` in het bestaande JSON-contract.
        tool: String => ("tool", true),
        /// `environment_selector` in het bestaande JSON-contract.
        environment_selector: String => ("environment_selector", true),
        /// `with_selectors` in het bestaande JSON-contract.
        with_selectors: List<String> => ("with_selectors", true),
        /// `mcp_server_ids` in het bestaande JSON-contract.
        mcp_server_ids: List<String> => ("mcp_server_ids", true),
        /// `attachment_ids` in het bestaande JSON-contract.
        attachment_ids: List<String> => ("attachment_ids", true),
        /// `model` in het bestaande JSON-contract.
        model: String => ("model", true),
        /// `run` in het bestaande JSON-contract.
        run: bool => ("run", true),
        /// `template_id` in het bestaande JSON-contract.
        template_id: String => ("template_id", true),
    }
}

model! {
    /// Het veldcontract van `CreateJobAttachmentRequest`.
    CreateJobAttachmentRequest {
        /// `ID` in het bestaande JSON-contract.
        id: String => ("ID", false),
        /// `JobID` in het bestaande JSON-contract.
        job_id: String => ("JobID", false),
        /// `Name` in het bestaande JSON-contract.
        name: String => ("Name", false),
        /// `MediaType` in het bestaande JSON-contract.
        media_type: String => ("MediaType", false),
        /// `Size` in het bestaande JSON-contract.
        size: i64 => ("Size", false),
        /// `SHA256` in het bestaande JSON-contract.
        sha256: String => ("SHA256", false),
        /// `CapsulePath` in het bestaande JSON-contract.
        capsule_path: String => ("CapsulePath", false),
        /// `Operator` in het bestaande JSON-contract.
        operator: String => ("Operator", false),
    }
}

model! {
    /// Het veldcontract van `CreateJobResponse`.
    CreateJobResponse {
        /// `job` in het bestaande JSON-contract.
        job: Job => ("job", false),
        /// `session` in het bestaande JSON-contract.
        session: Session => ("session", false),
        /// `composition` in het bestaande JSON-contract.
        composition: Option<Composition> => ("composition", true),
        /// `run_error` in het bestaande JSON-contract.
        run_error: String => ("run_error", true),
        /// `replayed` in het bestaande JSON-contract.
        replayed: bool => ("replayed", true),
    }
}

model! {
    /// Het veldcontract van `CreateJobSessionRequest`.
    CreateJobSessionRequest {
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
        /// `environment_selector` in het bestaande JSON-contract.
        environment_selector: String => ("environment_selector", false),
        /// `with_selectors` in het bestaande JSON-contract.
        with_selectors: List<String> => ("with_selectors", true),
        /// `mcp_server_ids` in het bestaande JSON-contract.
        mcp_server_ids: List<String> => ("mcp_server_ids", true),
        /// `objective_delta` in het bestaande JSON-contract.
        objective_delta: String => ("objective_delta", true),
        /// `role` in het bestaande JSON-contract.
        role: String => ("role", true),
        /// `model` in het bestaande JSON-contract.
        model: String => ("model", true),
        /// `spawned_by_session_id` in het bestaande JSON-contract.
        spawned_by_session_id: String => ("spawned_by_session_id", true),
        /// `run` in het bestaande JSON-contract.
        run: bool => ("run", true),
    }
}

model! {
    /// Het veldcontract van `CreateJobSessionResponse`.
    CreateJobSessionResponse {
        /// `session` in het bestaande JSON-contract.
        session: Session => ("session", false),
        /// `composition` in het bestaande JSON-contract.
        composition: Option<Composition> => ("composition", true),
        /// `run_error` in het bestaande JSON-contract.
        run_error: String => ("run_error", true),
    }
}

model! {
    /// Het veldcontract van `CreateWorkflowTemplateRequest`.
    CreateWorkflowTemplateRequest {
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", true),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `description` in het bestaande JSON-contract.
        description: String => ("description", true),
        /// `git_selector` in het bestaande JSON-contract.
        git_selector: String => ("git_selector", true),
        /// `phases` in het bestaande JSON-contract.
        phases: List<WorkflowPhase> => ("phases", false),
    }
}

model! {
    /// Het veldcontract van `AssignJobRequest`.
    AssignJobRequest {
        /// `assignee` in het bestaande JSON-contract.
        assignee: String => ("assignee", false),
    }
}

model! {
    /// Het veldcontract van `UpdateJobEnvironmentRequest`.
    UpdateJobEnvironmentRequest {
        /// `environment_selector` in het bestaande JSON-contract.
        environment_selector: String => ("environment_selector", false),
        /// `mcp_server_ids` in het bestaande JSON-contract.
        mcp_server_ids: List<String> => ("mcp_server_ids", false),
    }
}

model! {
    /// Het veldcontract van `CreateDeliverableCommentRequest`.
    CreateDeliverableCommentRequest {
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", true),
        /// `selected_text` in het bestaande JSON-contract.
        selected_text: String => ("selected_text", false),
        /// `start_offset` in het bestaande JSON-contract.
        start_offset: i64 => ("start_offset", false),
        /// `end_offset` in het bestaande JSON-contract.
        end_offset: i64 => ("end_offset", false),
        /// `prefix` in het bestaande JSON-contract.
        prefix: String => ("prefix", true),
        /// `suffix` in het bestaande JSON-contract.
        suffix: String => ("suffix", true),
        /// `body` in het bestaande JSON-contract.
        body: String => ("body", false),
    }
}

model! {
    /// Het veldcontract van `CreateCodeReviewRequest`.
    CreateCodeReviewRequest {
        /// `session_id` in het bestaande JSON-contract.
        session_id: String => ("session_id", true),
        /// `live` in het bestaande JSON-contract.
        live: bool => ("live", true),
    }
}

model! {
    /// Het veldcontract van `CreateCodeReviewCommentRequest`.
    CreateCodeReviewCommentRequest {
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", true),
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", false),
        /// `side` in het bestaande JSON-contract.
        side: String => ("side", false),
        /// `start_line` in het bestaande JSON-contract.
        start_line: i64 => ("start_line", false),
        /// `end_line` in het bestaande JSON-contract.
        end_line: i64 => ("end_line", false),
        /// `selected_text` in het bestaande JSON-contract.
        selected_text: String => ("selected_text", false),
        /// `body` in het bestaande JSON-contract.
        body: String => ("body", false),
    }
}

model! {
    /// Het veldcontract van `CodeReviewBundle`.
    CodeReviewBundle {
        /// `revision` in het bestaande JSON-contract.
        revision: CodeReviewRevision => ("revision", false),
        /// `history` in het bestaande JSON-contract.
        history: List<CodeReviewRevisionSummary> => ("history", false),
        /// `comments` in het bestaande JSON-contract.
        comments: List<CodeReviewComment> => ("comments", false),
        /// `latest_revision_id` in het bestaande JSON-contract.
        latest_revision_id: String => ("latest_revision_id", true),
        /// `annotatable` in het bestaande JSON-contract.
        annotatable: bool => ("annotatable", false),
    }
}

model! {
    /// Het veldcontract van `AnswerWorkflowQuestionRequest`.
    AnswerWorkflowQuestionRequest {
        /// `action` in het bestaande JSON-contract.
        action: String => ("action", false),
        /// `reason` in het bestaande JSON-contract.
        reason: String => ("reason", true),
        /// `answers` in het bestaande JSON-contract.
        answers: List<WorkflowQuestionAnswer> => ("answers", true),
    }
}

model! {
    /// Het veldcontract van `WorkflowAdvance`.
    WorkflowAdvance {
        /// `job` in het bestaande JSON-contract.
        job: Job => ("job", false),
        /// `phase_run` in het bestaande JSON-contract.
        phase_run: PhaseRun => ("phase_run", false),
        /// `question` in het bestaande JSON-contract.
        question: Option<WorkflowQuestion> => ("question", true),
        /// `next_session` in het bestaande JSON-contract.
        next_session: Option<Session> => ("next_session", true),
    }
}

model! {
    /// Het veldcontract van `CreateMCPServerRequest`.
    CreateMCPServerRequest {
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `transport` in het bestaande JSON-contract.
        transport: MCPTransport => ("transport", false),
        /// `command` in het bestaande JSON-contract.
        command: String => ("command", true),
        /// `args` in het bestaande JSON-contract.
        args: List<String> => ("args", true),
        /// `url` in het bestaande JSON-contract.
        url: String => ("url", true),
        /// `env` in het bestaande JSON-contract.
        env: List<MCPSecret> => ("env", true),
        /// `headers` in het bestaande JSON-contract.
        headers: List<MCPSecret> => ("headers", true),
    }
}

model! {
    /// Het veldcontract van `CreateGitRepositoryRequest`.
    CreateGitRepositoryRequest {
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `remote_url` in het bestaande JSON-contract.
        remote_url: String => ("remote_url", false),
        /// `default_ref` in het bestaande JSON-contract.
        default_ref: String => ("default_ref", true),
        /// `layer_selectors` in het bestaande JSON-contract.
        layer_selectors: List<String> => ("layer_selectors", true),
        /// `credential_scope` in het bestaande JSON-contract.
        credential_scope: CredentialScope => ("credential_scope", true),
        /// `services` in het bestaande JSON-contract.
        services: List<AppService> => ("services", true),
        /// `service_hosts` in het bestaande JSON-contract.
        service_hosts: List<String> => ("service_hosts", true),
    }
}

model! {
    /// Het veldcontract van `UpdateGitRepositoryRequest`.
    UpdateGitRepositoryRequest {
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", true),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `remote_url` in het bestaande JSON-contract.
        remote_url: String => ("remote_url", false),
        /// `default_ref` in het bestaande JSON-contract.
        default_ref: String => ("default_ref", false),
        /// `layer_selectors` in het bestaande JSON-contract.
        layer_selectors: List<String> => ("layer_selectors", false),
        /// `credential_scope` in het bestaande JSON-contract.
        credential_scope: CredentialScope => ("credential_scope", true),
        /// `services` in het bestaande JSON-contract.
        services: List<AppService> => ("services", false),
        /// `service_hosts` in het bestaande JSON-contract.
        service_hosts: List<String> => ("service_hosts", false),
    }
}

model! {
    /// Het veldcontract van `CreateGitRepositoryResponse`.
    CreateGitRepositoryResponse {
        /// `repository` in het bestaande JSON-contract.
        repository: GitRepository => ("repository", false),
    }
}

model! {
    /// Het veldcontract van `CreateGitAccountRequest`.
    CreateGitAccountRequest {
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", false),
        /// `provider` in het bestaande JSON-contract.
        provider: String => ("provider", false),
        /// `host` in het bestaande JSON-contract.
        host: String => ("host", true),
        /// `provider_id` in het bestaande JSON-contract.
        provider_id: String => ("provider_id", true),
        /// `login` in het bestaande JSON-contract.
        login: String => ("login", false),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", true),
        /// `email` in het bestaande JSON-contract.
        email: String => ("email", true),
        /// `access_token` in het bestaande JSON-contract.
        access_token: String => ("access_token", false),
        /// `credential_scope` in het bestaande JSON-contract.
        credential_scope: CredentialScope => ("credential_scope", true),
    }
}

model! {
    /// Het veldcontract van `SetupUserRequest`.
    SetupUserRequest {
        /// `username` in het bestaande JSON-contract.
        username: String => ("username", false),
        /// `display_name` in het bestaande JSON-contract.
        display_name: String => ("display_name", false),
        /// `password` in het bestaande JSON-contract.
        password: String => ("password", false),
    }
}

model! {
    /// Het veldcontract van `LoginRequest`.
    LoginRequest {
        /// `username` in het bestaande JSON-contract.
        username: String => ("username", false),
        /// `password` in het bestaande JSON-contract.
        password: String => ("password", false),
    }
}

model! {
    /// Het veldcontract van `CreateUserRequest`.
    CreateUserRequest {
        /// `username` in het bestaande JSON-contract.
        username: String => ("username", false),
        /// `display_name` in het bestaande JSON-contract.
        display_name: String => ("display_name", false),
        /// `password` in het bestaande JSON-contract.
        password: String => ("password", false),
        /// `role` in het bestaande JSON-contract.
        role: UserRole => ("role", true),
    }
}

model! {
    /// Het veldcontract van `SaveGitOAuthConfigurationRequest`.
    SaveGitOAuthConfigurationRequest {
        /// `provider` in het bestaande JSON-contract.
        provider: String => ("provider", false),
        /// `client_id` in het bestaande JSON-contract.
        client_id: String => ("client_id", false),
        /// `client_secret` in het bestaande JSON-contract.
        client_secret: String => ("client_secret", false),
    }
}

model! {
    /// Het veldcontract van `RegisterClientRequest`.
    RegisterClientRequest {
        /// `instance_id` in het bestaande JSON-contract.
        instance_id: String => ("instance_id", true),
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `capabilities` in het bestaande JSON-contract.
        capabilities: ClientCapabilities => ("capabilities", false),
    }
}

model! {
    /// Het veldcontract van `ClaimRequest`.
    ClaimRequest {
        /// `client_id` in het bestaande JSON-contract.
        client_id: String => ("client_id", false),
        /// `tools` in het bestaande JSON-contract.
        tools: List<String> => ("tools", false),
    }
}

model! {
    /// Het veldcontract van `Assignment`.
    Assignment {
        /// `job` in het bestaande JSON-contract.
        job: Job => ("job", false),
        /// `session` in het bestaande JSON-contract.
        session: Session => ("session", false),
        /// `activation` in het bestaande JSON-contract.
        activation: Activation => ("activation", false),
        /// `composition` in het bestaande JSON-contract.
        composition: Option<Composition> => ("composition", true),
    }
}

model! {
    /// Het veldcontract van `ActivationRequest`.
    ActivationRequest {
        /// `activation_id` in het bestaande JSON-contract.
        activation_id: String => ("activation_id", false),
        /// `epoch` in het bestaande JSON-contract.
        epoch: i64 => ("epoch", false),
    }
}

model! {
    /// Het veldcontract van `CreateTurnRequest`.
    CreateTurnRequest {
        /// `activation_id` in het bestaande JSON-contract.
        activation_id: String => ("activation_id", false),
        /// `epoch` in het bestaande JSON-contract.
        epoch: i64 => ("epoch", false),
        /// `input` in het bestaande JSON-contract.
        input: String => ("input", false),
        /// `actor` in het bestaande JSON-contract.
        actor: String => ("actor", true),
    }
}

model! {
    /// Het veldcontract van `CreateCheckpointRequest`.
    CreateCheckpointRequest {
        /// `activation_id` in het bestaande JSON-contract.
        activation_id: String => ("activation_id", false),
        /// `epoch` in het bestaande JSON-contract.
        epoch: i64 => ("epoch", false),
        /// `turn_id` in het bestaande JSON-contract.
        turn_id: String => ("turn_id", true),
        /// `kind` in het bestaande JSON-contract.
        kind: CheckpointKind => ("kind", false),
        /// `summary` in het bestaande JSON-contract.
        summary: String => ("summary", true),
        /// `capsule` in het bestaande JSON-contract.
        capsule: CapsuleManifest => ("capsule", false),
    }
}

model! {
    /// Het veldcontract van `CreateResultRequest`.
    CreateResultRequest {
        /// `activation_id` in het bestaande JSON-contract.
        activation_id: String => ("activation_id", false),
        /// `epoch` in het bestaande JSON-contract.
        epoch: i64 => ("epoch", false),
        /// `checkpoint_id` in het bestaande JSON-contract.
        checkpoint_id: String => ("checkpoint_id", false),
        /// `status` in het bestaande JSON-contract.
        status: ResultStatus => ("status", false),
        /// `summary` in het bestaande JSON-contract.
        summary: String => ("summary", false),
        /// `git_head` in het bestaande JSON-contract.
        git_head: String => ("git_head", true),
        /// `tests` in het bestaande JSON-contract.
        tests: List<TestEvidence> => ("tests", true),
        /// `acceptance_evidence` in het bestaande JSON-contract.
        acceptance_evidence: List<CriterionEvidence> => ("acceptance_evidence", true),
        /// `open_issues` in het bestaande JSON-contract.
        open_issues: List<String> => ("open_issues", true),
        /// `usage` in het bestaande JSON-contract.
        usage: Usage => ("usage", false),
    }
}

model! {
    /// Het veldcontract van `ForkSessionRequest`.
    ForkSessionRequest {
        /// `checkpoint_id` in het bestaande JSON-contract.
        checkpoint_id: String => ("checkpoint_id", true),
        /// `input_result_ids` in het bestaande JSON-contract.
        input_result_ids: List<String> => ("input_result_ids", true),
        /// `fork_mode` in het bestaande JSON-contract.
        fork_mode: ForkMode => ("fork_mode", false),
        /// `tool` in het bestaande JSON-contract.
        tool: String => ("tool", true),
        /// `model` in het bestaande JSON-contract.
        model: String => ("model", true),
        /// `operator` in het bestaande JSON-contract.
        operator: String => ("operator", true),
        /// `objective_delta` in het bestaande JSON-contract.
        objective_delta: String => ("objective_delta", true),
    }
}

model! {
    /// Het veldcontract van `SelectResultRequest`.
    SelectResultRequest {
        /// `result_id` in het bestaande JSON-contract.
        result_id: String => ("result_id", false),
    }
}
