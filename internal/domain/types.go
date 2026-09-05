package domain

import "time"

type JobStatus string

const (
	JobActive    JobStatus = "active"
	JobComparing JobStatus = "comparing"
	JobReview    JobStatus = "review"
	JobDone      JobStatus = "done"
	JobCancelled JobStatus = "cancelled"
)

type WorkflowStatus string

const (
	WorkflowBusy    WorkflowStatus = "busy"
	WorkflowPending WorkflowStatus = "pending"
	WorkflowDone    WorkflowStatus = "done"
)

type PhaseRunStatus string

const (
	PhaseRunQueued   PhaseRunStatus = "queued"
	PhaseRunRunning  PhaseRunStatus = "running"
	PhaseRunPending  PhaseRunStatus = "pending"
	PhaseRunAccepted PhaseRunStatus = "accepted"
	PhaseRunRejected PhaseRunStatus = "rejected"
)

const (
	WorkflowTargetNext    = "NEXT"
	WorkflowTargetSelf    = "SELF"
	WorkflowTargetDone    = "DONE"
	WorkflowTargetAskUser = "ASK_USER"
)

type WorkflowExecutor string

const (
	WorkflowExecutorAgent  WorkflowExecutor = "agent"
	WorkflowExecutorAction WorkflowExecutor = "action"

	WorkflowActionGitPullRequest = "git.pull_request.create"
	WorkflowPullRequestPhaseID   = "spin-pull-request"
)

type WorkflowAction struct {
	Type string `json:"type"`
}

type WorkflowTransition struct {
	Target    string `json:"target"`
	AskUser   bool   `json:"ask_user,omitempty"`
	Max       int    `json:"max,omitempty"`
	Exhausted string `json:"exhausted,omitempty"`
}

type DeliverableDefinition struct {
	Name        string `json:"name"`
	Description string `json:"description,omitempty"`
	Required    bool   `json:"required"`
}

type WorkflowPhase struct {
	ID                  string                  `json:"id"`
	Name                string                  `json:"name"`
	Instructions        string                  `json:"instructions"`
	Executor            WorkflowExecutor        `json:"executor,omitempty"`
	EnvironmentSelector string                  `json:"environment_selector,omitempty"`
	WithSelectors       []string                `json:"with_selectors,omitempty"`
	Action              *WorkflowAction         `json:"action,omitempty"`
	Inject              []string                `json:"inject"`
	Deliverables        []DeliverableDefinition `json:"deliverables"`
	AllowChanges        bool                    `json:"allow_changes"`
	// AllowCommit is read only to migrate Templates written by older builds.
	// New state and API clients use AllowChanges; commit is a control-plane
	// action performed as part of ACCEPT, never an agent tool.
	AllowCommit bool `json:"allow_commit,omitempty"`
	// AskUser is read only to migrate Templates written by older builds.
	// New Templates configure the human gate separately on Accept and Reject.
	AskUser bool               `json:"ask_user,omitempty"`
	Accept  WorkflowTransition `json:"accept"`
	Reject  WorkflowTransition `json:"reject"`
}

// WorkflowTemplate is deliberately only data. Names such as Development or
// Bugfix have no server-side meaning; their phase table defines the flow.
type WorkflowTemplate struct {
	ID          string          `json:"id"`
	Revision    int             `json:"revision"`
	Name        string          `json:"name"`
	Description string          `json:"description,omitempty"`
	GitSelector string          `json:"git_selector,omitempty"`
	CreatedBy   string          `json:"created_by"`
	Phases      []WorkflowPhase `json:"phases"`
	CreatedAt   time.Time       `json:"created_at"`
	UpdatedAt   time.Time       `json:"updated_at"`
}

type PhaseRun struct {
	ID             string                 `json:"id"`
	JobID          string                 `json:"job_id"`
	TemplateID     string                 `json:"template_id"`
	PhaseID        string                 `json:"phase_id"`
	PhaseName      string                 `json:"phase_name"`
	Attempt        int                    `json:"attempt"`
	SessionID      string                 `json:"session_id"`
	Status         PhaseRunStatus         `json:"status"`
	PendingReason  string                 `json:"pending_reason,omitempty"`
	PendingOutcome string                 `json:"pending_outcome,omitempty"`
	Summary        string                 `json:"summary,omitempty"`
	RejectReason   string                 `json:"reject_reason,omitempty"`
	AgentOutcomes  []WorkflowAgentOutcome `json:"agent_outcomes,omitempty"`
	ActionResult   *WorkflowActionResult  `json:"action_result,omitempty"`
	StartedAt      time.Time              `json:"started_at"`
	CompletedAt    *time.Time             `json:"completed_at,omitempty"`
}

type WorkflowActionResult struct {
	Type       string    `json:"type"`
	ExternalID string    `json:"external_id,omitempty"`
	URL        string    `json:"url,omitempty"`
	Detail     string    `json:"detail,omitempty"`
	CreatedAt  time.Time `json:"created_at"`
}

// WorkflowAgentOutcome is an immutable audit event for every accept/reject
// emitted by the agent. A PhaseRun can contain more than one outcome when a
// human chooses CHAT and lets the same Session continue.
type WorkflowAgentOutcome struct {
	ID        string    `json:"id"`
	Outcome   string    `json:"outcome"`
	Detail    string    `json:"detail,omitempty"`
	CreatedAt time.Time `json:"created_at"`
}

type Deliverable struct {
	ID          string    `json:"id"`
	JobID       string    `json:"job_id"`
	PhaseRunID  string    `json:"phase_run_id"`
	SessionID   string    `json:"session_id"`
	Name        string    `json:"name"`
	Description string    `json:"description,omitempty"`
	Content     string    `json:"content"`
	Revision    int       `json:"revision"`
	CreatedAt   time.Time `json:"created_at"`
}

// DeliverableComment is immutable review history on one exact deliverable
// revision. A newer revision is a different Deliverable and therefore starts
// with an empty comment history automatically.
type DeliverableComment struct {
	ID            string    `json:"id"`
	DeliverableID string    `json:"deliverable_id"`
	SelectedText  string    `json:"selected_text"`
	StartOffset   int       `json:"start_offset"`
	EndOffset     int       `json:"end_offset"`
	Prefix        string    `json:"prefix,omitempty"`
	Suffix        string    `json:"suffix,omitempty"`
	Body          string    `json:"body"`
	Author        string    `json:"author"`
	CreatedAt     time.Time `json:"created_at"`
}

// CodeReviewRevision is an immutable capture of one explicit Changes review.
// Files live in persisted state and are only returned by the focused review
// API; Snapshot exposes the lightweight summary below.
type CodeReviewRevision struct {
	ID                string           `json:"id"`
	JobID             string           `json:"job_id"`
	SourcePhaseRunID  string           `json:"source_phase_run_id,omitempty"`
	ContextPhaseRunID string           `json:"context_phase_run_id,omitempty"`
	SessionID         string           `json:"session_id,omitempty"`
	PhaseID           string           `json:"phase_id,omitempty"`
	PhaseName         string           `json:"phase_name,omitempty"`
	Attempt           int              `json:"attempt,omitempty"`
	Scope             string           `json:"scope"`
	ScopeKey          string           `json:"scope_key"`
	Live              bool             `json:"live,omitempty"`
	Branch            string           `json:"branch,omitempty"`
	Digest            string           `json:"digest"`
	Added             int              `json:"added"`
	Deleted           int              `json:"deleted"`
	Files             []CodeReviewFile `json:"files"`
	CreatedBy         string           `json:"created_by"`
	CreatedAt         time.Time        `json:"created_at"`
}

type CodeReviewFile struct {
	Path      string `json:"path"`
	Status    string `json:"status"`
	Added     int    `json:"added"`
	Deleted   int    `json:"deleted"`
	Patch     string `json:"patch,omitempty"`
	Binary    bool   `json:"binary,omitempty"`
	Truncated bool   `json:"truncated,omitempty"`
}

type CodeReviewRevisionSummary struct {
	ID                string    `json:"id"`
	JobID             string    `json:"job_id"`
	SourcePhaseRunID  string    `json:"source_phase_run_id,omitempty"`
	ContextPhaseRunID string    `json:"context_phase_run_id,omitempty"`
	SessionID         string    `json:"session_id,omitempty"`
	PhaseID           string    `json:"phase_id,omitempty"`
	PhaseName         string    `json:"phase_name,omitempty"`
	Attempt           int       `json:"attempt,omitempty"`
	Scope             string    `json:"scope"`
	ScopeKey          string    `json:"scope_key"`
	Branch            string    `json:"branch,omitempty"`
	Added             int       `json:"added"`
	Deleted           int       `json:"deleted"`
	FileCount         int       `json:"file_count"`
	CreatedBy         string    `json:"created_by"`
	CreatedAt         time.Time `json:"created_at"`
}

// CodeReviewComment never changes or resolves. It remains attached to the
// exact captured diff revision on which an author selected the code.
type CodeReviewComment struct {
	ID         string    `json:"id"`
	RevisionID string    `json:"revision_id"`
	Path       string    `json:"path"`
	Side       string    `json:"side"`
	StartLine  int       `json:"start_line"`
	EndLine    int       `json:"end_line"`
	Selected   string    `json:"selected_text"`
	Body       string    `json:"body"`
	Author     string    `json:"author"`
	CreatedAt  time.Time `json:"created_at"`
}

// WorkflowQuestionItem is one question inside an agent ask. Options are the
// answers the agent expects; the operator can always answer in their own words
// instead, which Other records.
type WorkflowQuestionItem struct {
	ID       string   `json:"id"`
	Question string   `json:"question"`
	Options  []string `json:"options,omitempty"`
	Answer   string   `json:"answer,omitempty"`
	Other    bool     `json:"other,omitempty"`
}

// WorkflowQuestionAnswer answers one item of an agent ask.
type WorkflowQuestionAnswer struct {
	ItemID string `json:"item_id"`
	Answer string `json:"answer"`
}

type WorkflowQuestion struct {
	ID             string                 `json:"id"`
	JobID          string                 `json:"job_id"`
	PhaseRunID     string                 `json:"phase_run_id"`
	SessionID      string                 `json:"session_id"`
	Kind           string                 `json:"kind"`
	Question       string                 `json:"question"`
	Items          []WorkflowQuestionItem `json:"items,omitempty"`
	Outcome        string                 `json:"outcome,omitempty"`
	AgentDetail    string                 `json:"agent_detail,omitempty"`
	AgentOutcomeID string                 `json:"agent_outcome_id,omitempty"`
	AcceptTarget   string                 `json:"accept_target,omitempty"`
	RejectTarget   string                 `json:"reject_target,omitempty"`
	Answer         string                 `json:"answer,omitempty"`
	Reason         string                 `json:"reason,omitempty"`
	AnsweredBy     string                 `json:"answered_by,omitempty"`
	Status         string                 `json:"status"`
	CreatedAt      time.Time              `json:"created_at"`
	AnsweredAt     *time.Time             `json:"answered_at,omitempty"`
}

type SessionStatus string

const (
	SessionQueued    SessionStatus = "queued"
	SessionClaimed   SessionStatus = "claimed"
	SessionRunning   SessionStatus = "running"
	SessionFrozen    SessionStatus = "frozen"
	SessionCompleted SessionStatus = "completed"
	SessionCancelled SessionStatus = "cancelled"
)

type ActivationStatus string

const (
	ActivationClaimed ActivationStatus = "claimed"
	ActivationRunning ActivationStatus = "running"
	ActivationEnded   ActivationStatus = "ended"
)

type TurnStatus string

const (
	TurnRunning   TurnStatus = "running"
	TurnCompleted TurnStatus = "completed"
)

type ResultStatus string

const (
	ResultSuccess ResultStatus = "success"
	ResultPartial ResultStatus = "partial"
	ResultFailed  ResultStatus = "failed"
)

type CheckpointKind string

const (
	CheckpointBaseline     CheckpointKind = "baseline"
	CheckpointSessionStart CheckpointKind = "session_start"
	CheckpointTurnEnd      CheckpointKind = "turn_end"
	CheckpointManual       CheckpointKind = "manual"
	CheckpointResult       CheckpointKind = "result"
	CheckpointCrash        CheckpointKind = "crash"
)

type ForkMode string

const (
	ForkFull       ForkMode = "full"
	ForkFilesystem ForkMode = "filesystem"
	ForkResult     ForkMode = "result"
	ForkRoot       ForkMode = "root"
	ForkCritic     ForkMode = "critic"
	ForkSynthesis  ForkMode = "synthesis"
)

// ArtifactKind is deliberately extensible. These constants are the first
// composition behaviours Spin understands; unknown kinds remain opaque.
type ArtifactKind string

const (
	ArtifactTool       ArtifactKind = "tool"
	ArtifactCredential ArtifactKind = "credential"
	ArtifactConfig     ArtifactKind = "config"
	ArtifactWorkspace  ArtifactKind = "workspace"
	ArtifactSession    ArtifactKind = "session"
	ArtifactResult     ArtifactKind = "result"
)

type ArtifactScope string

const (
	ScopeGlobal  ArtifactScope = "global"
	ScopeTeam    ArtifactScope = "team"
	ScopeProject ArtifactScope = "project"
	ScopeUser    ArtifactScope = "user"
)

// CredentialScope controls whose provider identity is resolved for a Git
// operation. User credentials follow the operator performing the Job; global
// credentials are shared service identities. Public repositories deliberately
// resolve no credential.
type CredentialScope string

const (
	CredentialScopeUser   CredentialScope = "user"
	CredentialScopeGlobal CredentialScope = "global"
	CredentialScopePublic CredentialScope = "public"
)

type ArtifactSensitivity string

const (
	SensitivityPublic  ArtifactSensitivity = "public"
	SensitivityPrivate ArtifactSensitivity = "private"
	SensitivitySecret  ArtifactSensitivity = "secret"
)

type RecordingStatus string

const (
	RecordingOpen      RecordingStatus = "recording"
	RecordingCompleted RecordingStatus = "completed"
	RecordingCancelled RecordingStatus = "cancelled"
)

// CapsuleSnapshot describes what the snapshot engine actually persisted. The
// process-state bit is explicit because a Docker image commit is restorable but
// does not contain RAM, open sockets or a provider-side KV cache.
type CapsuleSnapshot struct {
	Driver string `json:"driver"`
	// ClientID pins daemon-local images to the runner that created them. An
	// empty value is legacy state and may be resolved by any compatible runner.
	ClientID             string   `json:"client_id,omitempty"`
	ReplicaClientIDs     []string `json:"replica_client_ids,omitempty"`
	Ref                  string   `json:"ref,omitempty"`
	Digest               string   `json:"digest"`
	Restorable           bool     `json:"restorable"`
	IncludesProcessState bool     `json:"includes_process_state"`
}

type CapsuleRuntime struct {
	Driver string `json:"driver"`
	// ClientID is the durable affinity key for every operation on this runtime.
	// A disconnected runner does not clear it; Retry creates a new Session.
	ClientID      string `json:"client_id,omitempty"`
	ContainerID   string `json:"container_id,omitempty"`
	ContainerName string `json:"container_name,omitempty"`
	BaseRef       string `json:"base_ref,omitempty"`
	WorkspaceRef  string `json:"workspace_ref,omitempty"`
	AttachCommand string `json:"attach_command,omitempty"`
	Status        string `json:"status"`
}

type CapsuleEngineInfo struct {
	Driver                   string `json:"driver"`
	Available                bool   `json:"available"`
	BaseImage                string `json:"base_image,omitempty"`
	FilesystemSnapshots      bool   `json:"filesystem_snapshots"`
	ProcessCheckpoints       bool   `json:"process_checkpoints"`
	InteractiveAttachCommand bool   `json:"interactive_attach_command"`
	Detail                   string `json:"detail,omitempty"`
}

// Enablement is a capability published by a layer. Spin treats unknown names
// as opaque metadata; a matching hook can interpret the optional launch
// descriptor. ACP is the first such hook and uses a stdio command.
type Enablement struct {
	Name            string `json:"name"`
	Command         string `json:"command,omitempty"`
	Transport       string `json:"transport,omitempty"`
	ProtocolVersion int    `json:"protocol_version,omitempty"`
}

type Artifact struct {
	ID                       string              `json:"id"`
	Kind                     ArtifactKind        `json:"kind"`
	Name                     string              `json:"name"`
	Scope                    ArtifactScope       `json:"scope"`
	Subject                  string              `json:"subject,omitempty"`
	Profile                  string              `json:"profile"`
	Provides                 []string            `json:"provides"`
	Requires                 []string            `json:"requires"`
	Enables                  []Enablement        `json:"enables,omitempty"`
	Slot                     string              `json:"slot,omitempty"`
	ParentArtifactIDs        []string            `json:"parent_artifact_ids"`
	SnapshotDigest           string              `json:"snapshot_digest"`
	Snapshot                 CapsuleSnapshot     `json:"snapshot"`
	CompatibilityFingerprint string              `json:"compatibility_fingerprint,omitempty"`
	Sensitivity              ArtifactSensitivity `json:"sensitivity"`
	CreatedBy                string              `json:"created_by"`
	CreatedAt                time.Time           `json:"created_at"`
}

type RecordingCommand struct {
	Sequence int       `json:"sequence"`
	ExitCode *int      `json:"exit_code,omitempty"`
	At       time.Time `json:"at"`
}

type Recording struct {
	ID                       string              `json:"id"`
	Actor                    string              `json:"actor"`
	Kind                     ArtifactKind        `json:"kind"`
	Name                     string              `json:"name"`
	Scope                    ArtifactScope       `json:"scope"`
	Subject                  string              `json:"subject,omitempty"`
	Profile                  string              `json:"profile"`
	Provides                 []string            `json:"provides"`
	Requires                 []string            `json:"requires"`
	Enables                  []Enablement        `json:"enables,omitempty"`
	Slot                     string              `json:"slot,omitempty"`
	ParentArtifactIDs        []string            `json:"parent_artifact_ids"`
	Runtime                  *CapsuleRuntime     `json:"runtime,omitempty"`
	CompatibilityFingerprint string              `json:"compatibility_fingerprint,omitempty"`
	Sensitivity              ArtifactSensitivity `json:"sensitivity"`
	Status                   RecordingStatus     `json:"status"`
	Commands                 []RecordingCommand  `json:"commands"`
	ArtifactID               string              `json:"artifact_id,omitempty"`
	StartedAt                time.Time           `json:"started_at"`
	EndedAt                  *time.Time          `json:"ended_at,omitempty"`
}

type ResolvedArtifact struct {
	ArtifactID string       `json:"artifact_id"`
	Kind       string       `json:"kind"`
	Name       string       `json:"name"`
	Slot       string       `json:"slot,omitempty"`
	Scope      string       `json:"scope"`
	Subject    string       `json:"subject,omitempty"`
	Profile    string       `json:"profile,omitempty"`
	Enables    []Enablement `json:"enables,omitempty"`
	Reason     string       `json:"reason"`
}

// GitWorkspace is resolved when a Session is materialized. Git authentication
// is app-owned user state, not an Artifact/layer. Only non-secret identity
// metadata is copied into a Composition; the server resolves the token just in
// time for the short-lived checkout helper.
type GitWorkspace struct {
	RepositoryID    string          `json:"repository_id"`
	RepositoryName  string          `json:"repository_name"`
	RemoteURL       string          `json:"remote_url"`
	BaseRef         string          `json:"base_ref"`
	BootstrapRef    string          `json:"bootstrap_ref"`
	HeadRef         string          `json:"head_ref"`
	TargetRef       string          `json:"target_ref"`
	CredentialScope CredentialScope `json:"credential_scope"`
	// AccountID is retained only for compositions persisted before scope resolution.
	AccountID   string `json:"account_id,omitempty"`
	Provider    string `json:"provider,omitempty"`
	Login       string `json:"login,omitempty"`
	AuthorName  string `json:"author_name,omitempty"`
	AuthorEmail string `json:"author_email,omitempty"`
}

type Composition struct {
	ID                   string             `json:"id"`
	Operator             string             `json:"operator"`
	Selector             string             `json:"selector"`
	EntryArtifactID      string             `json:"entry_artifact_id"`
	Tool                 string             `json:"tool,omitempty"` // legacy worker routing hint
	SessionID            string             `json:"session_id,omitempty"`
	Profile              string             `json:"profile"`
	WithSelectors        []string           `json:"with_selectors,omitempty"`
	RequestedArtifactIDs []string           `json:"requested_artifact_ids,omitempty"`
	ResolvedArtifacts    []ResolvedArtifact `json:"resolved_artifacts"`
	SlotBindings         map[string]string  `json:"slot_bindings"`
	Enabled              []Enablement       `json:"enabled,omitempty"`
	MCPServerIDs         []string           `json:"mcp_server_ids,omitempty"`
	Git                  *GitWorkspace      `json:"git,omitempty"`
	Warnings             []string           `json:"warnings,omitempty"`
	Runtime              *CapsuleRuntime    `json:"runtime,omitempty"`
	CreatedAt            time.Time          `json:"created_at"`
}

type Job struct {
	ID                  string            `json:"id"`
	ForkedFromJobID     string            `json:"forked_from_job_id,omitempty"`
	Title               string            `json:"title"`
	Objective           string            `json:"objective"`
	AcceptanceCriteria  []string          `json:"acceptance_criteria,omitempty"`
	Owner               string            `json:"owner,omitempty"`
	GitRepositoryID     string            `json:"git_repository_id"`
	GitRepositoryName   string            `json:"git_repository_name,omitempty"`
	GitRemoteURL        string            `json:"git_remote_url,omitempty"`
	GitProvider         string            `json:"git_provider,omitempty"`
	GitCredentialScope  CredentialScope   `json:"git_credential_scope,omitempty"`
	BaseRef             string            `json:"base_ref,omitempty"`
	Branch              string            `json:"branch"`
	WithSelectors       []string          `json:"with_selectors,omitempty"`
	MCPServerIDs        []string          `json:"mcp_server_ids,omitempty"`
	AttachmentIDs       []string          `json:"attachment_ids,omitempty"`
	TemplateID          string            `json:"template_id,omitempty"`
	TemplateSnapshot    *WorkflowTemplate `json:"template_snapshot,omitempty"`
	EnvironmentSelector string            `json:"environment_selector,omitempty"`
	Model               string            `json:"model,omitempty"`
	PhaseRunIDs         []string          `json:"phase_run_ids,omitempty"`
	CurrentPhaseRunID   string            `json:"current_phase_run_id,omitempty"`
	WorkflowStatus      WorkflowStatus    `json:"workflow_status,omitempty"`
	PendingReason       string            `json:"pending_reason,omitempty"`
	Status              JobStatus         `json:"status"`
	SessionIDs          []string          `json:"session_ids"`
	CandidateResultIDs  []string          `json:"candidate_result_ids"`
	FinalResultID       string            `json:"final_result_id,omitempty"`
	CreatedAt           time.Time         `json:"created_at"`
	UpdatedAt           time.Time         `json:"updated_at"`
}

// JobAttachment is immutable input supplied by a person. The blob stays out
// of Git and the JSON state; CapsulePath is the stable read-only location made
// available to every Session belonging to the Job.
type JobAttachment struct {
	ID          string    `json:"id"`
	JobID       string    `json:"job_id,omitempty"`
	Name        string    `json:"name"`
	MediaType   string    `json:"media_type"`
	Size        int64     `json:"size"`
	SHA256      string    `json:"sha256"`
	CapsulePath string    `json:"capsule_path"`
	CreatedBy   string    `json:"created_by"`
	CreatedAt   time.Time `json:"created_at"`
}

type Session struct {
	ID                    string           `json:"id"`
	JobID                 string           `json:"job_id"`
	PhaseRunID            string           `json:"phase_run_id,omitempty"`
	ParentSessionID       string           `json:"parent_session_id,omitempty"`
	SpawnedBySessionID    string           `json:"spawned_by_session_id,omitempty"`
	ParentCheckpointID    string           `json:"parent_checkpoint_id,omitempty"`
	InputResultIDs        []string         `json:"input_result_ids,omitempty"`
	ForkMode              ForkMode         `json:"fork_mode"`
	Tool                  string           `json:"tool"`
	Executor              WorkflowExecutor `json:"executor,omitempty"`
	EnvironmentSelector   string           `json:"environment_selector,omitempty"`
	WithSelectors         []string         `json:"with_selectors,omitempty"`
	MCPServerIDs          []string         `json:"mcp_server_ids,omitempty"`
	Role                  string           `json:"role,omitempty"`
	Model                 string           `json:"model,omitempty"`
	Operator              string           `json:"operator,omitempty"`
	PreparedCompositionID string           `json:"prepared_composition_id,omitempty"`
	ObjectiveDelta        string           `json:"objective_delta,omitempty"`
	GitRepositoryID       string           `json:"git_repository_id"`
	BaseRef               string           `json:"base_ref,omitempty"`
	GitRef                string           `json:"git_ref"`
	TargetBranch          string           `json:"target_branch"`
	Status                SessionStatus    `json:"status"`
	ClientID              string           `json:"client_id,omitempty"`
	ActivationID          string           `json:"activation_id,omitempty"`
	ActivationEpoch       int64            `json:"activation_epoch"`
	LeaseExpiresAt        *time.Time       `json:"lease_expires_at,omitempty"`
	CurrentCheckpointID   string           `json:"current_checkpoint_id,omitempty"`
	TurnIDs               []string         `json:"turn_ids"`
	CheckpointIDs         []string         `json:"checkpoint_ids"`
	FinalResultID         string           `json:"final_result_id,omitempty"`
	ContinuityLevel       string           `json:"continuity_level"`
	ContinuityScore       int              `json:"continuity_score"`
	CreatedAt             time.Time        `json:"created_at"`
	UpdatedAt             time.Time        `json:"updated_at"`
}

type Turn struct {
	ID                 string            `json:"id"`
	SessionID          string            `json:"session_id"`
	ActivationID       string            `json:"activation_id"`
	ActivationEpoch    int64             `json:"activation_epoch"`
	Sequence           int               `json:"sequence"`
	Input              string            `json:"input"`
	Actor              string            `json:"actor,omitempty"`
	CredentialBindings map[string]string `json:"credential_bindings,omitempty"`
	Status             TurnStatus        `json:"status"`
	CheckpointID       string            `json:"checkpoint_id,omitempty"`
	StartedAt          time.Time         `json:"started_at"`
	EndedAt            *time.Time        `json:"ended_at,omitempty"`
}

type Activation struct {
	ID                 string            `json:"id"`
	SessionID          string            `json:"session_id"`
	ClientID           string            `json:"client_id"`
	Operator           string            `json:"operator,omitempty"`
	CompositionID      string            `json:"composition_id,omitempty"`
	CredentialBindings map[string]string `json:"credential_bindings,omitempty"`
	Epoch              int64             `json:"epoch"`
	Status             ActivationStatus  `json:"status"`
	Reason             string            `json:"reason,omitempty"`
	StartedAt          time.Time         `json:"started_at"`
	EndedAt            *time.Time        `json:"ended_at,omitempty"`
}

type CapsuleManifest struct {
	ImageDigest              string `json:"image_digest,omitempty"`
	FilesystemSnapshotDigest string `json:"filesystem_snapshot_digest,omitempty"`
	ProcessCheckpointDigest  string `json:"process_checkpoint_digest,omitempty"`
	GitHead                  string `json:"git_head,omitempty"`
	GitDirty                 bool   `json:"git_dirty"`
	AgentSessionID           string `json:"agent_session_id,omitempty"`
	EventSequence            int64  `json:"event_sequence"`
	CompatibilityFingerprint string `json:"compatibility_fingerprint,omitempty"`
	ExternalEffectsWatermark int64  `json:"external_effects_watermark"`
	Restorable               bool   `json:"restorable"`
	UnrestorableReason       string `json:"unrestorable_reason,omitempty"`
}

type Checkpoint struct {
	ID                 string          `json:"id"`
	SessionID          string          `json:"session_id"`
	ActivationID       string          `json:"activation_id"`
	ActivationEpoch    int64           `json:"activation_epoch"`
	TurnID             string          `json:"turn_id,omitempty"`
	ParentCheckpointID string          `json:"parent_checkpoint_id,omitempty"`
	Sequence           int             `json:"sequence"`
	Kind               CheckpointKind  `json:"kind"`
	Summary            string          `json:"summary,omitempty"`
	Capsule            CapsuleManifest `json:"capsule"`
	CreatedAt          time.Time       `json:"created_at"`
}

type TestEvidence struct {
	Name    string `json:"name"`
	Passed  bool   `json:"passed"`
	Details string `json:"details,omitempty"`
}

type CriterionEvidence struct {
	Criterion string `json:"criterion"`
	Met       bool   `json:"met"`
	Evidence  string `json:"evidence,omitempty"`
}

type Usage struct {
	WallTimeMS       int64 `json:"wall_time_ms,omitempty"`
	InputTokens      int64 `json:"input_tokens,omitempty"`
	OutputTokens     int64 `json:"output_tokens,omitempty"`
	CacheReadTokens  int64 `json:"cache_read_tokens,omitempty"`
	CacheWriteTokens int64 `json:"cache_write_tokens,omitempty"`
}

type Result struct {
	ID                 string              `json:"id"`
	JobID              string              `json:"job_id"`
	SessionID          string              `json:"session_id"`
	CheckpointID       string              `json:"checkpoint_id"`
	Status             ResultStatus        `json:"status"`
	Summary            string              `json:"summary"`
	GitHead            string              `json:"git_head,omitempty"`
	Tests              []TestEvidence      `json:"tests,omitempty"`
	AcceptanceEvidence []CriterionEvidence `json:"acceptance_evidence,omitempty"`
	OpenIssues         []string            `json:"open_issues,omitempty"`
	Usage              Usage               `json:"usage"`
	CreatedAt          time.Time           `json:"created_at"`
}

type ClientCapabilities struct {
	OS            string            `json:"os,omitempty"`
	Arch          string            `json:"arch,omitempty"`
	Tools         []string          `json:"tools"`
	SnapshotModes []string          `json:"snapshot_modes,omitempty"`
	Engine        CapsuleEngineInfo `json:"engine"`
	MaxWorkloads  int               `json:"max_workloads,omitempty"`
}

type Client struct {
	ID           string             `json:"id"`
	InstanceID   string             `json:"instance_id,omitempty"`
	Name         string             `json:"name"`
	Capabilities ClientCapabilities `json:"capabilities"`
	Status       string             `json:"status"`
	Draining     bool               `json:"draining,omitempty"`
	LastSeenAt   time.Time          `json:"last_seen_at"`
	CreatedAt    time.Time          `json:"created_at"`
}

type MCPTransport string

const (
	MCPTransportStdio MCPTransport = "stdio"
	MCPTransportHTTP  MCPTransport = "http"
)

type MCPSecret struct {
	Name  string `json:"name"`
	Value string `json:"value,omitempty"`
}

// MCPServer mirrors the MCP server shapes accepted by ACP session/new. Secret
// values are persisted for handoff but redacted from public Snapshots.
type MCPServer struct {
	ID        string       `json:"id"`
	Operator  string       `json:"operator"`
	Name      string       `json:"name"`
	Transport MCPTransport `json:"transport"`
	Command   string       `json:"command,omitempty"`
	Args      []string     `json:"args,omitempty"`
	URL       string       `json:"url,omitempty"`
	Env       []MCPSecret  `json:"env,omitempty"`
	Headers   []MCPSecret  `json:"headers,omitempty"`
	CreatedAt time.Time    `json:"created_at"`
}

// GitRepository is shared source metadata. It stores only the identity scope;
// the provider account is resolved from remote host and operator per action.
type GitRepository struct {
	ID              string          `json:"id"`
	Name            string          `json:"name"`
	RemoteURL       string          `json:"remote_url"`
	DefaultRef      string          `json:"default_ref"`
	Provider        string          `json:"provider"`
	CredentialScope CredentialScope `json:"credential_scope"`
	LayerSelectors  []string        `json:"layer_selectors,omitempty"`
	CreatedBy       string          `json:"created_by"`
	CreatedAt       time.Time       `json:"created_at"`
	UpdatedAt       time.Time       `json:"updated_at"`
}

// GitAccount is user- or global-scoped application state. Secret fields are
// persisted by the Store but are always cleared from its public Snapshot.
type GitAccount struct {
	ID              string          `json:"id"`
	Operator        string          `json:"operator"`
	Provider        string          `json:"provider"`
	Host            string          `json:"host"`
	ProviderID      string          `json:"provider_id,omitempty"`
	Login           string          `json:"login"`
	Name            string          `json:"name,omitempty"`
	Email           string          `json:"email,omitempty"`
	AccessToken     string          `json:"access_token,omitempty"`
	RefreshToken    string          `json:"refresh_token,omitempty"`
	TokenType       string          `json:"token_type,omitempty"`
	Scope           string          `json:"scope,omitempty"`
	CredentialScope CredentialScope `json:"credential_scope"`
	ExpiresAt       *time.Time      `json:"expires_at,omitempty"`
	CreatedAt       time.Time       `json:"created_at"`
	UpdatedAt       time.Time       `json:"updated_at"`
}

type UserRole string

const (
	UserAdmin  UserRole = "admin"
	UserMember UserRole = "member"
)

// User contains authentication material and is never returned directly by the
// public API. PublicUser is the deliberately redacted representation.
type User struct {
	ID           string     `json:"id"`
	Username     string     `json:"username"`
	DisplayName  string     `json:"display_name"`
	Role         UserRole   `json:"role"`
	PasswordHash string     `json:"password_hash"`
	ArchivedAt   *time.Time `json:"archived_at,omitempty"`
	CreatedAt    time.Time  `json:"created_at"`
	UpdatedAt    time.Time  `json:"updated_at"`
}

type PublicUser struct {
	ID          string     `json:"id"`
	Username    string     `json:"username"`
	DisplayName string     `json:"display_name"`
	Role        UserRole   `json:"role"`
	ArchivedAt  *time.Time `json:"archived_at,omitempty"`
	CreatedAt   time.Time  `json:"created_at"`
}

type AuthSession struct {
	ID         string    `json:"id"`
	UserID     string    `json:"user_id"`
	TokenHash  string    `json:"token_hash"`
	CSRFHash   string    `json:"csrf_hash"`
	ExpiresAt  time.Time `json:"expires_at"`
	CreatedAt  time.Time `json:"created_at"`
	LastSeenAt time.Time `json:"last_seen_at"`
}

// GitOAuthConfiguration is encrypted server configuration for a real
// provider application. ClientSecret is redacted from all public responses.
type GitOAuthConfiguration struct {
	Provider     string    `json:"provider"`
	ClientID     string    `json:"client_id"`
	ClientSecret string    `json:"client_secret,omitempty"`
	CreatedBy    string    `json:"created_by"`
	CreatedAt    time.Time `json:"created_at"`
	UpdatedAt    time.Time `json:"updated_at"`
}

type Snapshot struct {
	Artifacts           []Artifact                  `json:"artifacts"`
	Recordings          []Recording                 `json:"recordings"`
	Compositions        []Composition               `json:"compositions"`
	Jobs                []Job                       `json:"jobs"`
	JobAttachments      []JobAttachment             `json:"job_attachments"`
	WorkflowTemplates   []WorkflowTemplate          `json:"workflow_templates"`
	PhaseRuns           []PhaseRun                  `json:"phase_runs"`
	Deliverables        []Deliverable               `json:"deliverables"`
	DeliverableComments []DeliverableComment        `json:"deliverable_comments"`
	CodeReviewRevisions []CodeReviewRevisionSummary `json:"code_review_revisions"`
	CodeReviewComments  []CodeReviewComment         `json:"code_review_comments"`
	WorkflowQuestions   []WorkflowQuestion          `json:"workflow_questions"`
	Sessions            []Session                   `json:"sessions"`
	Activations         []Activation                `json:"activations"`
	Turns               []Turn                      `json:"turns"`
	Checkpoints         []Checkpoint                `json:"checkpoints"`
	Results             []Result                    `json:"results"`
	Clients             []Client                    `json:"clients"`
	MCPServers          []MCPServer                 `json:"mcp_servers"`
	GitRepositories     []GitRepository             `json:"git_repositories"`
	GitAccounts         []GitAccount                `json:"git_accounts"`
	Users               []PublicUser                `json:"users"`
}

type Recommendation struct {
	JobID        string   `json:"job_id"`
	Action       string   `json:"action"`
	Reason       string   `json:"reason"`
	SessionID    string   `json:"session_id,omitempty"`
	CheckpointID string   `json:"checkpoint_id,omitempty"`
	ResultIDs    []string `json:"result_ids,omitempty"`
	Priority     int      `json:"priority"`
}

type CreateRecordingRequest struct {
	Actor                    string              `json:"actor"`
	Kind                     ArtifactKind        `json:"kind"`
	Name                     string              `json:"name"`
	Scope                    ArtifactScope       `json:"scope,omitempty"`
	Subject                  string              `json:"subject,omitempty"`
	Profile                  string              `json:"profile,omitempty"`
	Provides                 []string            `json:"provides,omitempty"`
	Requires                 []string            `json:"requires,omitempty"`
	Enables                  []Enablement        `json:"enables,omitempty"`
	Slot                     string              `json:"slot,omitempty"`
	ParentArtifactIDs        []string            `json:"parent_artifact_ids,omitempty"`
	CompatibilityFingerprint string              `json:"compatibility_fingerprint,omitempty"`
	Sensitivity              ArtifactSensitivity `json:"sensitivity,omitempty"`
}

type ExecuteRecordingCommandRequest struct {
	Actor string `json:"actor"`
	Input string `json:"input"`
}

type AttachRecordingParentRequest struct {
	Actor string       `json:"actor"`
	Kind  ArtifactKind `json:"kind"`
	Name  string       `json:"name"`
}

type EndRecordingRequest struct {
	Actor          string          `json:"actor"`
	SnapshotDigest string          `json:"snapshot_digest,omitempty"`
	Snapshot       CapsuleSnapshot `json:"snapshot,omitempty"`
}

type CancelRecordingRequest struct {
	Actor string `json:"actor"`
}

type DeleteArtifactRequest struct {
	Operator string `json:"operator"`
}

type UseRequest struct {
	Selector      string   `json:"selector,omitempty"`
	WithSelectors []string `json:"with_selectors,omitempty"`
	Operator      string   `json:"operator"`
	Profile       string   `json:"profile,omitempty"`
	SessionID     string   `json:"session_id,omitempty"`
	Tool          string   `json:"tool,omitempty"` // accepted for old API clients
}

type StopCompositionRequest struct {
	Operator string `json:"operator"`
}

type CommandRequest struct {
	Operator string `json:"operator"`
	Line     string `json:"line"`
}

type CommandResponse struct {
	Message     string       `json:"message"`
	Output      string       `json:"output,omitempty"`
	ExitCode    *int         `json:"exit_code,omitempty"`
	Recording   *Recording   `json:"recording,omitempty"`
	Artifact    *Artifact    `json:"artifact,omitempty"`
	Composition *Composition `json:"composition,omitempty"`
	Artifacts   []Artifact   `json:"artifacts,omitempty"`
}

type CreateJobRequest struct {
	Title               string   `json:"title"`
	Objective           string   `json:"objective"`
	ForkedFromJobID     string   `json:"forked_from_job_id,omitempty"`
	IdempotencyKey      string   `json:"idempotency_key,omitempty"`
	AcceptanceCriteria  []string `json:"acceptance_criteria,omitempty"`
	Owner               string   `json:"owner,omitempty"`
	Operator            string   `json:"operator,omitempty"`
	GitRepositoryID     string   `json:"git_repository_id"`
	BaseRef             string   `json:"base_ref,omitempty"`
	Tool                string   `json:"tool,omitempty"`
	EnvironmentSelector string   `json:"environment_selector,omitempty"`
	WithSelectors       []string `json:"with_selectors,omitempty"`
	MCPServerIDs        []string `json:"mcp_server_ids,omitempty"`
	AttachmentIDs       []string `json:"attachment_ids,omitempty"`
	Model               string   `json:"model,omitempty"`
	Run                 bool     `json:"run,omitempty"` // legacy; Jobs always initialize asynchronously
	TemplateID          string   `json:"template_id,omitempty"`
}

type CreateJobAttachmentRequest struct {
	ID          string
	JobID       string
	Name        string
	MediaType   string
	Size        int64
	SHA256      string
	CapsulePath string
	Operator    string
}

type CreateJobResponse struct {
	Job         Job          `json:"job"`
	Session     Session      `json:"session"`
	Composition *Composition `json:"composition,omitempty"`
	RunError    string       `json:"run_error,omitempty"`
	Replayed    bool         `json:"replayed,omitempty"`
}

type CreateJobSessionRequest struct {
	Operator            string   `json:"operator"`
	EnvironmentSelector string   `json:"environment_selector"`
	WithSelectors       []string `json:"with_selectors,omitempty"`
	MCPServerIDs        []string `json:"mcp_server_ids,omitempty"`
	ObjectiveDelta      string   `json:"objective_delta,omitempty"`
	Role                string   `json:"role,omitempty"`
	Model               string   `json:"model,omitempty"`
	SpawnedBySessionID  string   `json:"spawned_by_session_id,omitempty"`
	Run                 bool     `json:"run,omitempty"`
}

type CreateJobSessionResponse struct {
	Session     Session      `json:"session"`
	Composition *Composition `json:"composition,omitempty"`
	RunError    string       `json:"run_error,omitempty"`
}

type CreateWorkflowTemplateRequest struct {
	Operator    string          `json:"operator,omitempty"`
	Name        string          `json:"name"`
	Description string          `json:"description,omitempty"`
	GitSelector string          `json:"git_selector,omitempty"`
	Phases      []WorkflowPhase `json:"phases"`
}

type CreateDeliverableCommentRequest struct {
	Operator     string `json:"operator,omitempty"`
	SelectedText string `json:"selected_text"`
	StartOffset  int    `json:"start_offset"`
	EndOffset    int    `json:"end_offset"`
	Prefix       string `json:"prefix,omitempty"`
	Suffix       string `json:"suffix,omitempty"`
	Body         string `json:"body"`
}

type CreateCodeReviewRequest struct {
	SessionID string `json:"session_id,omitempty"`
	Live      bool   `json:"live,omitempty"`
}

type CreateCodeReviewCommentRequest struct {
	Operator     string `json:"operator,omitempty"`
	Path         string `json:"path"`
	Side         string `json:"side"`
	StartLine    int    `json:"start_line"`
	EndLine      int    `json:"end_line"`
	SelectedText string `json:"selected_text"`
	Body         string `json:"body"`
}

type CodeReviewBundle struct {
	Revision         CodeReviewRevision          `json:"revision"`
	History          []CodeReviewRevisionSummary `json:"history"`
	Comments         []CodeReviewComment         `json:"comments"`
	LatestRevisionID string                      `json:"latest_revision_id,omitempty"`
	Annotatable      bool                        `json:"annotatable"`
}

type AnswerWorkflowQuestionRequest struct {
	Action  string                   `json:"action"`
	Reason  string                   `json:"reason,omitempty"`
	Answers []WorkflowQuestionAnswer `json:"answers,omitempty"`
}

type WorkflowAdvance struct {
	Job         Job               `json:"job"`
	PhaseRun    PhaseRun          `json:"phase_run"`
	Question    *WorkflowQuestion `json:"question,omitempty"`
	NextSession *Session          `json:"next_session,omitempty"`
}

type CreateMCPServerRequest struct {
	Operator  string       `json:"operator"`
	Name      string       `json:"name"`
	Transport MCPTransport `json:"transport"`
	Command   string       `json:"command,omitempty"`
	Args      []string     `json:"args,omitempty"`
	URL       string       `json:"url,omitempty"`
	Env       []MCPSecret  `json:"env,omitempty"`
	Headers   []MCPSecret  `json:"headers,omitempty"`
}

type CreateGitRepositoryRequest struct {
	Operator        string          `json:"operator"`
	Name            string          `json:"name"`
	RemoteURL       string          `json:"remote_url"`
	DefaultRef      string          `json:"default_ref,omitempty"`
	LayerSelectors  []string        `json:"layer_selectors,omitempty"`
	CredentialScope CredentialScope `json:"credential_scope,omitempty"`
}

type UpdateGitRepositoryRequest struct {
	Operator        string          `json:"operator,omitempty"`
	Name            string          `json:"name"`
	RemoteURL       string          `json:"remote_url"`
	DefaultRef      string          `json:"default_ref"`
	LayerSelectors  []string        `json:"layer_selectors"`
	CredentialScope CredentialScope `json:"credential_scope,omitempty"`
}

type CreateGitRepositoryResponse struct {
	Repository GitRepository `json:"repository"`
}

type CreateGitAccountRequest struct {
	Operator        string          `json:"operator"`
	Provider        string          `json:"provider"`
	Host            string          `json:"host,omitempty"`
	ProviderID      string          `json:"provider_id,omitempty"`
	Login           string          `json:"login"`
	Name            string          `json:"name,omitempty"`
	Email           string          `json:"email,omitempty"`
	AccessToken     string          `json:"access_token"`
	CredentialScope CredentialScope `json:"credential_scope,omitempty"`
}

type SetupUserRequest struct {
	Username    string `json:"username"`
	DisplayName string `json:"display_name"`
	Password    string `json:"password"`
}

type LoginRequest struct {
	Username string `json:"username"`
	Password string `json:"password"`
}

type CreateUserRequest struct {
	Username    string   `json:"username"`
	DisplayName string   `json:"display_name"`
	Password    string   `json:"password"`
	Role        UserRole `json:"role,omitempty"`
}

type SaveGitOAuthConfigurationRequest struct {
	Provider     string `json:"provider"`
	ClientID     string `json:"client_id"`
	ClientSecret string `json:"client_secret"`
}

type RegisterClientRequest struct {
	InstanceID   string             `json:"instance_id,omitempty"`
	Name         string             `json:"name"`
	Capabilities ClientCapabilities `json:"capabilities"`
}

type ClaimRequest struct {
	ClientID string   `json:"client_id"`
	Tools    []string `json:"tools"`
}

type Assignment struct {
	Job         Job          `json:"job"`
	Session     Session      `json:"session"`
	Activation  Activation   `json:"activation"`
	Composition *Composition `json:"composition,omitempty"`
}

type ActivationRequest struct {
	ActivationID string `json:"activation_id"`
	Epoch        int64  `json:"epoch"`
}

type CreateTurnRequest struct {
	ActivationID string `json:"activation_id"`
	Epoch        int64  `json:"epoch"`
	Input        string `json:"input"`
	Actor        string `json:"actor,omitempty"`
}

type CreateCheckpointRequest struct {
	ActivationID string          `json:"activation_id"`
	Epoch        int64           `json:"epoch"`
	TurnID       string          `json:"turn_id,omitempty"`
	Kind         CheckpointKind  `json:"kind"`
	Summary      string          `json:"summary,omitempty"`
	Capsule      CapsuleManifest `json:"capsule"`
}

type CreateResultRequest struct {
	ActivationID       string              `json:"activation_id"`
	Epoch              int64               `json:"epoch"`
	CheckpointID       string              `json:"checkpoint_id"`
	Status             ResultStatus        `json:"status"`
	Summary            string              `json:"summary"`
	GitHead            string              `json:"git_head,omitempty"`
	Tests              []TestEvidence      `json:"tests,omitempty"`
	AcceptanceEvidence []CriterionEvidence `json:"acceptance_evidence,omitempty"`
	OpenIssues         []string            `json:"open_issues,omitempty"`
	Usage              Usage               `json:"usage"`
}

type ForkSessionRequest struct {
	CheckpointID   string   `json:"checkpoint_id,omitempty"`
	InputResultIDs []string `json:"input_result_ids,omitempty"`
	ForkMode       ForkMode `json:"fork_mode"`
	Tool           string   `json:"tool,omitempty"`
	Model          string   `json:"model,omitempty"`
	Operator       string   `json:"operator,omitempty"`
	ObjectiveDelta string   `json:"objective_delta,omitempty"`
}

type SelectResultRequest struct {
	ResultID string `json:"result_id"`
}
