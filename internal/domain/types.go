package domain

import (
	"path/filepath"
	"strings"
	"time"
)

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
	// WorkflowExecutorExpose starts the repository's app services on the
	// phase's workspace and waits for a person to test and decide.
	WorkflowExecutorExpose WorkflowExecutor = "expose"

	WorkflowActionGitPullRequest = "git.pull_request.create"
	// WorkflowActionGitMerge finalizes a Job by merging its branch into the
	// base branch itself: the review and the accept already happened in
	// Spin, so nothing is left for a pull request to add.
	WorkflowActionGitMerge = "git.merge"
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

// A deliverable has a kind, and the kind says what a put must be, so a
// delivery is checked rather than trusted: a Markdown document, a PDF, an
// image, a folder with at least one file (a page with its own CSS and JS,
// say), or one file of any kind. Everything but a document is kept as a
// bundle and served to the reviewer.
const (
	DeliverableKindMarkdown = "markdown"
	DeliverableKindPDF      = "pdf"
	DeliverableKindImage    = "image"
	DeliverableKindFolder   = "folder"
	DeliverableKindFile     = "file"
)

// DeliverableKinds lists the kinds a Template may ask for.
var DeliverableKinds = []string{DeliverableKindMarkdown, DeliverableKindPDF, DeliverableKindImage, DeliverableKindFolder, DeliverableKindFile}

// DeliverableIsBundle says whether a kind travels as a bundle.
func DeliverableIsBundle(kind string) bool {
	return kind != "" && kind != DeliverableKindMarkdown
}

type DeliverableDefinition struct {
	Name        string `json:"name"`
	Description string `json:"description,omitempty"`
	Required    bool   `json:"required"`
	Kind        string `json:"kind,omitempty"`
}

// DeliverableDirectory is where a Job's deliverables live in every capsule
// of the Job: each as a file or folder named after the deliverable, put
// there when a step starts and read back when the agent puts one.
const DeliverableDirectory = "/root/deliverables"

// PreviousJobDeliverableDirectory is where a fork finds the last documents
// of the Job it continues: files to read when needed, not prompt text.
const PreviousJobDeliverableDirectory = DeliverableDirectory + "/vorige-job"

// DeliverableSlug is the file name a deliverable goes by in the capsule.
func DeliverableSlug(name string) string {
	var out []rune
	dash := false
	for _, r := range strings.ToLower(strings.TrimSpace(name)) {
		switch {
		case r >= 'a' && r <= 'z', r >= '0' && r <= '9':
			out = append(out, r)
			dash = false
		case !dash && len(out) > 0:
			out = append(out, '-')
			dash = true
		}
	}
	slug := strings.TrimRight(string(out), "-")
	if slug == "" {
		return "deliverable"
	}
	return slug
}

// CapsulePath is where this revision sits in a capsule: a Markdown file, a
// folder, or a single file with the entry's extension.
func (d Deliverable) CapsulePath() string { return d.CapsulePathIn(DeliverableDirectory) }

// CapsulePathIn is the deliverable's path under another directory: the
// previous Job's documents live under PreviousJobDeliverableDirectory.
func (d Deliverable) CapsulePathIn(directory string) string {
	slug := DeliverableSlug(d.Name)
	if !DeliverableIsBundle(d.Kind) || d.Bundle == nil {
		return directory + "/" + slug + ".md"
	}
	if d.Bundle.Folder {
		return directory + "/" + slug
	}
	return directory + "/" + slug + strings.ToLower(filepath.Ext(d.Bundle.Entry))
}

// DeliverableBundle is a visual deliverable as the runner delivered it: a
// zip in the database, its entry file and what that file is.
type DeliverableBundle struct {
	Ref    string `json:"ref"`
	Digest string `json:"digest"`
	Size   int64  `json:"size"`
	Files  int    `json:"files"`
	// Folder says the bundle came from a folder; Entry is index.html of a
	// folder when it has one, or the one file of a single-file bundle.
	Folder      bool   `json:"folder"`
	Entry       string `json:"entry,omitempty"`
	ContentType string `json:"content_type,omitempty"`
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
	// Model and ReasoningEffort are set on the agent's ACP session before the
	// phase's first prompt (session/set_config_option); empty keeps the
	// agent's default. The values an agent accepts are on its layer's
	// AgentOptions.
	Model           string             `json:"model,omitempty"`
	ReasoningEffort string             `json:"reasoning_effort,omitempty"`
	Accept          WorkflowTransition `json:"accept"`
	Reject          WorkflowTransition `json:"reject"`
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
	// Restarts counts how often a person started this attempt over with a
	// fresh agent (the workspace kept); RestartNotes are what they said
	// should go differently, one per restart that had a note.
	Restarts     int      `json:"restarts,omitempty"`
	RestartNotes []string `json:"restart_notes,omitempty"`
	// RestartTranscript is the conversation of the previous attempt up to
	// the message the person went back to, when they forked from a message
	// rather than starting the attempt over blank.
	RestartTranscript []ChatLine `json:"restart_transcript,omitempty"`
	StartedAt         time.Time  `json:"started_at"`
	CompletedAt       *time.Time `json:"completed_at,omitempty"`
}

// ChatLine is one message of a chat, as context for a fresh agent.
type ChatLine struct {
	Role string `json:"role"` // user or agent
	Text string `json:"text"`
}

type WorkflowActionResult struct {
	Type       string `json:"type"`
	ExternalID string `json:"external_id,omitempty"`
	URL        string `json:"url,omitempty"`
	Detail     string `json:"detail,omitempty"`
	// Results holds the outcome per repository of a Job with several: the
	// merge commit or pull request URL, by repository ID.
	Results   map[string]string `json:"results,omitempty"`
	CreatedAt time.Time         `json:"created_at"`
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
	// UpdatedAt is set when the same phase run rewrote or edited this
	// revision in place.
	UpdatedAt time.Time `json:"updated_at,omitempty"`
	// Kind is one of DeliverableKinds; every kind but markdown has a
	// Bundle and no Content.
	Kind   string             `json:"kind,omitempty"`
	Bundle *DeliverableBundle `json:"bundle,omitempty"`
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
	Path       string `json:"path"`
	Status     string `json:"status"`
	Repository string `json:"repository,omitempty"`
	Folder     string `json:"folder,omitempty"`
	Head       string `json:"head,omitempty"`
	Added      int    `json:"added"`
	Deleted    int    `json:"deleted"`
	Patch      string `json:"patch,omitempty"`
	Binary     bool   `json:"binary,omitempty"`
	Truncated  bool   `json:"truncated,omitempty"`
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
// LayerContents is what a layer wrote (its real difference from the layer
// under it) or what a Session changed outside its workspace.
type LayerContents struct {
	Files int   `json:"files"`
	Bytes int64 `json:"bytes"`
	// DroppedIdentical is what sealing left out: files identical to the
	// layer below.
	DroppedIdentical ContentTotal `json:"dropped_identical,omitempty"`
	// Entries is the full listing, largest first. The runner fills it; the
	// server keeps it as a file next to the state, not in the state.
	Entries []ContentEntry `json:"entries,omitempty"`
}

type ContentEntry struct {
	Path  string `json:"path"`
	Bytes int64  `json:"bytes"`
	// Logins are the numbers of the layer's logins that hold this file;
	// Source is "login" for a file that is only there, not in the layer.
	Logins []int  `json:"logins,omitempty"`
	Source string `json:"source,omitempty"`
}

type ContentTotal struct {
	Files int   `json:"files,omitempty"`
	Bytes int64 `json:"bytes,omitempty"`
}

type CapsuleSnapshot struct {
	Driver string `json:"driver"`
	// ClientID pins daemon-local images to the runner that created them.
	ClientID         string   `json:"client_id,omitempty"`
	ReplicaClientIDs []string `json:"replica_client_ids,omitempty"`
	Ref              string   `json:"ref,omitempty"`
	Digest           string   `json:"digest"`
	// RootFS identifies the image by its layer diff IDs, which every image
	// store reports the same; Digest is the image ID, which the classic
	// store and the containerd store compute differently.
	RootFS               string `json:"rootfs,omitempty"`
	Restorable           bool   `json:"restorable"`
	IncludesProcessState bool   `json:"includes_process_state"`
	// Contents is the layer's manifest: its real difference, by kind.
	Contents *LayerContents `json:"contents,omitempty"`
	// ParentRef is the Spin layer image this layer was recorded on; with
	// Delta set, the archive holds only this layer's own difference, and a
	// runner rebuilds the image from the parent plus that difference.
	ParentRef string `json:"parent_ref,omitempty"`
	Delta     bool   `json:"delta,omitempty"`
	// Content identifies the layer by what is in it: a chain of the
	// parent's identity and the hash of this layer's own difference, the
	// same on every runner however the image was put together there.
	Content string `json:"content,omitempty"`
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

// AgentOption is one value an ACP agent offers for a session config option.
type AgentOption struct {
	Value       string `json:"value"`
	Name        string `json:"name,omitempty"`
	Description string `json:"description,omitempty"`
}

// AgentSettings is the chosen way to start an ACP layer's agent. Empty
// fields mean automatic: full access when the agent offers it, and the
// agent's own default model and reasoning effort.
// Login is one logged-in instance of a layer's tracked files: the files a
// person chose in the layer's contents (a token the agent rotates, a
// config it keeps), as they were last read from a capsule. A credential
// layer hands its logins out: every running capsule holds one of them and
// no two capsules hold the same one, so a refresh in one capsule never
// invalidates another; a person logs in as often as there should be
// capsules at once. Any other layer has one login that every capsule
// shares. The layer's own files are its first login.
type Login struct {
	ID string `json:"id"`
	// Key names the layer across its versions: subject/kind:name.
	Key string `json:"key"`
	// Number is the login's place in the layer's list, for people: Login 1,
	// Login 2. Removing one leaves the others their numbers.
	Number int `json:"number"`
	// Owner is the operator this login is for; empty means everyone who
	// runs the layer. A shared layer holds both kinds.
	Owner     string            `json:"owner,omitempty"`
	Files     map[string][]byte `json:"files"`
	CreatedAt time.Time         `json:"created_at"`
	UpdatedAt time.Time         `json:"updated_at"`
}

// LoginFile is one file of a login, without its content.
type LoginFile struct {
	Path string `json:"path"`
	Size int64  `json:"size"`
}

// LoginSummary is a login without its files, as the browser sees it.
type LoginSummary struct {
	ID        string    `json:"id"`
	Key       string    `json:"key"`
	Number    int       `json:"number"`
	Files     int       `json:"files"`
	Bytes     int64     `json:"bytes"`
	Owner     string    `json:"owner,omitempty"`
	CreatedAt time.Time `json:"created_at"`
	UpdatedAt time.Time `json:"updated_at"`
	// CompositionID is the running capsule that holds the login, if any.
	CompositionID string `json:"composition_id,omitempty"`
}

type AgentSettings struct {
	Mode            string `json:"mode,omitempty"`
	Model           string `json:"model,omitempty"`
	ReasoningEffort string `json:"reasoning_effort,omitempty"`
	// AutoAccept is whether Spin answers this agent's permission requests
	// itself; nil means yes. Sessions on the layer start with it.
	AutoAccept *bool `json:"auto_accept,omitempty"`
}

// AgentOptions is what an ACP layer's agent reported it can be configured
// with (session/new configOptions), kept on the layer so Templates can pick
// a model and reasoning effort per phase without starting the agent.
type AgentOptions struct {
	AgentName        string        `json:"agent_name,omitempty"`
	Models           []AgentOption `json:"models,omitempty"`
	ReasoningEfforts []AgentOption `json:"reasoning_efforts,omitempty"`
	Modes            []AgentOption `json:"modes,omitempty"`
	FetchedAt        time.Time     `json:"fetched_at"`
	// Error is why the last fetch failed, when it did.
	Error string `json:"error,omitempty"`
	// Fetching is set while a fetch runs; the previous options stay
	// visible meanwhile.
	Fetching bool `json:"fetching,omitempty"`
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
	// AgentOptions is what this layer's ACP agent reported it accepts; nil
	// until fetched or until a Session on this layer ran.
	AgentOptions *AgentOptions `json:"agent_options,omitempty"`
	// TrackedPaths are the files of this layer, chosen by a person in its
	// contents, that Spin keeps between Sessions: read back from a capsule
	// after every turn and at stop, put in place before the next agent
	// starts. Absolute paths in the capsule. They move to a new version.
	// A path ending in "/" is a folder: everything in it, apart from
	// TrackedExcludes and lock files.
	TrackedPaths []string `json:"tracked_paths,omitempty"`
	// TrackedExcludes are files and folders (ending in "/") inside a tracked
	// folder that are not kept: caches, history, what a login does not need.
	TrackedExcludes []string `json:"tracked_excludes,omitempty"`
	// AgentSettings is how Sessions on this layer start the agent: chosen
	// from AgentOptions on the layer that ENABLES acp, kept as metadata (no
	// re-seal) and carried to the next version.
	AgentSettings *AgentSettings `json:"agent_settings,omitempty"`
	// SnapshotPrunedAt is set once the archived snapshot of a superseded
	// version was removed to free storage; the newer version carries its
	// content, and runners may still hold the image as a cache.
	SnapshotPrunedAt *time.Time `json:"snapshot_pruned_at,omitempty"`
	// SupersededBy points at the version that replaced this one.
	// The snapshot stays: layers recorded from it still resolve their parent by
	// ID, and a composition that meets it binds the newest version instead.
	SupersededBy string `json:"superseded_by,omitempty"`
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
	ReplacesArtifactID       string              `json:"replaces_artifact_id,omitempty"`
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
// RepositoryMode is what a Job does with one of its repositories: change
// it on the Job branch, or only read it.
type RepositoryMode string

const (
	RepositoryModeChange    RepositoryMode = "change"
	RepositoryModeReference RepositoryMode = "reference"
)

// WorkspaceRoot is where the repositories of a Job are checked out in a
// capsule: the one repository of a Job at the root, several each in a
// folder of their own below it.
const WorkspaceRoot = "/workspace"

// JobRepository is one repository of a Job. Every repository the Job
// changes gets the same Job branch and Session branches, and lands on its
// own base; a reference repository is checked out at its base, read-only.
type JobRepository struct {
	RepositoryID    string          `json:"repository_id"`
	Name            string          `json:"name"`
	RemoteURL       string          `json:"remote_url"`
	Provider        string          `json:"provider,omitempty"`
	CredentialScope CredentialScope `json:"credential_scope,omitempty"`
	BaseRef         string          `json:"base_ref"`
	Mode            RepositoryMode  `json:"mode"`
	// Path is the folder under the workspace root, empty for a Job with one
	// repository, which sits at the root.
	Path string `json:"path,omitempty"`
}

// Directory is where the repository is checked out in a capsule.
func (r JobRepository) Directory() string { return WorkspaceDirectory(r.Path) }

// WorkspaceDirectory is the checkout directory for a workspace path.
func WorkspaceDirectory(path string) string {
	if path == "" {
		return WorkspaceRoot
	}
	return WorkspaceRoot + "/" + path
}

// JobRepositoryRequest names a repository for a new Job.
type JobRepositoryRequest struct {
	RepositoryID string         `json:"repository_id"`
	Mode         RepositoryMode `json:"mode,omitempty"`
	BaseRef      string         `json:"base_ref,omitempty"`
}

type GitWorkspace struct {
	RepositoryID   string `json:"repository_id"`
	RepositoryName string `json:"repository_name"`
	RemoteURL      string `json:"remote_url"`
	BaseRef        string `json:"base_ref"`
	BootstrapRef   string `json:"bootstrap_ref"`
	HeadRef        string `json:"head_ref"`
	TargetRef      string `json:"target_ref"`
	// Path is the folder under the workspace root; empty is the root.
	Path string `json:"path,omitempty"`
	// Mode says whether the Session changes this repository or only reads
	// it; empty means change.
	Mode RepositoryMode `json:"mode,omitempty"`
	// ContextRefs are branches fetched read-only next to the Job's own, such
	// as the branch of the Job this one continues.
	ContextRefs     []string        `json:"context_refs,omitempty"`
	CredentialScope CredentialScope `json:"credential_scope"`
	// AccountID is retained only for compositions persisted before scope resolution.
	AccountID   string `json:"account_id,omitempty"`
	Provider    string `json:"provider,omitempty"`
	Login       string `json:"login,omitempty"`
	AuthorName  string `json:"author_name,omitempty"`
	AuthorEmail string `json:"author_email,omitempty"`
}

// Directory is where the workspace is checked out in a capsule.
func (w GitWorkspace) Directory() string { return WorkspaceDirectory(w.Path) }

// Changes says whether the Session may change this repository.
func (w GitWorkspace) Changes() bool { return w.Mode != RepositoryModeReference }

type Composition struct {
	ID                   string   `json:"id"`
	Operator             string   `json:"operator"`
	Selector             string   `json:"selector"`
	EntryArtifactID      string   `json:"entry_artifact_id"`
	Tool                 string   `json:"tool,omitempty"` // the agent's name, for routing and display
	SessionID            string   `json:"session_id,omitempty"`
	Profile              string   `json:"profile"`
	WithSelectors        []string `json:"with_selectors,omitempty"`
	RequestedArtifactIDs []string `json:"requested_artifact_ids,omitempty"`
	// Layers is the stack, bottom to top: every layer version that
	// contributes, parents before children, newer versions right above the
	// ones they replace, later selections above earlier ones. The filesystem
	// is each layer's own diff applied in this order; the agent is the
	// topmost layer that enables acp.
	Layers            []string           `json:"layers,omitempty"`
	ResolvedArtifacts []ResolvedArtifact `json:"resolved_artifacts"`
	SlotBindings      map[string]string  `json:"slot_bindings"`
	Enabled           []Enablement       `json:"enabled,omitempty"`
	MCPServerIDs      []string           `json:"mcp_server_ids,omitempty"`
	Git               *GitWorkspace      `json:"git,omitempty"`
	// Workspaces are all repositories of the Job in this capsule, Git
	// (the first one the Job changes) among them; empty for a capsule with
	// one repository, which is Git alone.
	Workspaces []GitWorkspace  `json:"workspaces,omitempty"`
	Warnings   []string        `json:"warnings,omitempty"`
	Runtime    *CapsuleRuntime `json:"runtime,omitempty"`
	// CapsuleChanges is what the capsule changed outside the workspace,
	// taken after a turn and at stop, by kind.
	CapsuleChanges *LayerContents `json:"capsule_changes,omitempty"`
	// Logins is which login of each layer this capsule holds, by layer
	// key, from its start until it stops.
	Logins map[string]string `json:"logins,omitempty"`
	// ForLogin marks a capsule started to log in once more: it holds no
	// login and keeps the layer's own files until a person saves what they
	// logged in as a new login.
	ForLogin        bool      `json:"for_login,omitempty"`
	ForLoginPrivate bool      `json:"for_login_private,omitempty"`
	CreatedAt       time.Time `json:"created_at"`
}

// GitWorkspaces are the repositories in the capsule: Workspaces when the
// Job has several, else Git alone.
func (c Composition) GitWorkspaces() []GitWorkspace {
	if len(c.Workspaces) > 0 {
		return c.Workspaces
	}
	if c.Git != nil {
		return []GitWorkspace{*c.Git}
	}
	return nil
}

// ChangedWorkspaces are the repositories the Session may change.
func (c Composition) ChangedWorkspaces() []GitWorkspace {
	var changed []GitWorkspace
	for _, workspace := range c.GitWorkspaces() {
		if workspace.Changes() {
			changed = append(changed, workspace)
		}
	}
	return changed
}

// JobRepositories are the Job's repositories: Repositories when set, else
// the one repository of the Git fields, changed, at the workspace root.
func (j Job) JobRepositories() []JobRepository {
	if len(j.Repositories) > 0 {
		return j.Repositories
	}
	if j.GitRepositoryID == "" {
		return nil
	}
	return []JobRepository{{RepositoryID: j.GitRepositoryID, Name: j.GitRepositoryName, RemoteURL: j.GitRemoteURL, Provider: j.GitProvider, CredentialScope: j.GitCredentialScope, BaseRef: j.BaseRef, Mode: RepositoryModeChange}}
}

// ChangedRepositories are the repositories the Job changes, in order.
func (j Job) ChangedRepositories() []JobRepository {
	var changed []JobRepository
	for _, repository := range j.JobRepositories() {
		if repository.Mode != RepositoryModeReference {
			changed = append(changed, repository)
		}
	}
	return changed
}

type Job struct {
	ID              string `json:"id"`
	ForkedFromJobID string `json:"forked_from_job_id,omitempty"`
	Title           string `json:"title"`
	// Reference is the ticket or issue number this Job belongs to. It
	// becomes the branch namespace (jobs/<reference>/…) so every Job on the
	// same ticket, forks included, is found together.
	Reference          string   `json:"reference,omitempty"`
	Objective          string   `json:"objective"`
	AcceptanceCriteria []string `json:"acceptance_criteria,omitempty"`
	Owner              string   `json:"owner,omitempty"`
	// Assignee is who the Job is with right now; the owner at creation,
	// handed to a colleague to look at it. Empty means the owner.
	Assignee           string          `json:"assignee,omitempty"`
	GitRepositoryID    string          `json:"git_repository_id"`
	GitRepositoryName  string          `json:"git_repository_name,omitempty"`
	GitRemoteURL       string          `json:"git_remote_url,omitempty"`
	GitProvider        string          `json:"git_provider,omitempty"`
	GitCredentialScope CredentialScope `json:"git_credential_scope,omitempty"`
	BaseRef            string          `json:"base_ref,omitempty"`
	Branch             string          `json:"branch"`
	// Repositories are all repositories of the Job; the Git fields above
	// describe the first one it changes. Empty for a Job made before
	// several repositories were possible: that one repository is meant.
	Repositories        []JobRepository   `json:"repositories,omitempty"`
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
	// SyncedHead is the last commit pushed to the Session branch on the
	// remote as work in progress, and SyncedAt when.
	SyncedHead          string        `json:"synced_head,omitempty"`
	SyncedAt            *time.Time    `json:"synced_at,omitempty"`
	Status              SessionStatus `json:"status"`
	ClientID            string        `json:"client_id,omitempty"`
	ActivationID        string        `json:"activation_id,omitempty"`
	ActivationEpoch     int64         `json:"activation_epoch"`
	LeaseExpiresAt      *time.Time    `json:"lease_expires_at,omitempty"`
	CurrentCheckpointID string        `json:"current_checkpoint_id,omitempty"`
	TurnIDs             []string      `json:"turn_ids"`
	CheckpointIDs       []string      `json:"checkpoint_ids"`
	FinalResultID       string        `json:"final_result_id,omitempty"`
	ContinuityLevel     string        `json:"continuity_level"`
	ContinuityScore     int           `json:"continuity_score"`
	CreatedAt           time.Time     `json:"created_at"`
	UpdatedAt           time.Time     `json:"updated_at"`
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
	// Services is the repository's own recipe for running the app so a
	// person can test it: one container per service on a Session's workspace.
	Services []AppService `json:"services,omitempty"`
	// ServiceHosts are extra name→address entries the app containers get
	// (--add-host), for a database the app reaches by a name the runner
	// host knows; set here on purpose rather than copied from the host.
	ServiceHosts []string  `json:"service_hosts,omitempty"`
	CreatedBy    string    `json:"created_by"`
	CreatedAt    time.Time `json:"created_at"`
	UpdatedAt    time.Time `json:"updated_at"`
}

// AppService is one runnable part of a repository's app. A service with Run
// starts in the Session's own image on the Session's workspace, after its
// Prepare commands; a service with Image is a ready-made dependency such as
// a database. Env names a file on the runner host (var/env/<name>.env) that
// the runner hands to the container; its contents never reach the control
// plane.
type AppService struct {
	Name    string   `json:"name"`
	Image   string   `json:"image,omitempty"`
	Prepare []string `json:"prepare,omitempty"`
	Run     string   `json:"run,omitempty"`
	Ports   []int    `json:"ports,omitempty"`
	Env     string   `json:"env,omitempty"`
}

// AppServiceRuntime is what the runner reports about one running service.
type AppServiceRuntime struct {
	Service     string         `json:"service"`
	ContainerID string         `json:"container_id,omitempty"`
	Status      string         `json:"status"` // starting, running, exited, error
	Host        string         `json:"host,omitempty"`
	Ports       map[string]int `json:"ports,omitempty"` // container port → host port
	Reachable   bool           `json:"reachable"`
	Error       string         `json:"error,omitempty"`
	StartedAt   *time.Time     `json:"started_at,omitempty"`
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
	Logins              []LoginSummary              `json:"logins"`
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
	// ReplacesArtifactID marks a new version: the finished recording becomes the new
	// version of that artifact, which is then superseded.
	ReplacesArtifactID string `json:"replaces_artifact_id,omitempty"`
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
	// ForLogin starts the capsule to log in once more; see Composition.
	// ForLoginPrivate makes the login for the operator alone.
	ForLogin        bool `json:"for_login,omitempty"`
	ForLoginPrivate bool `json:"for_login_private,omitempty"`
}

type StopCompositionRequest struct {
	Operator string `json:"operator"`
}

// StartStatus reports a recording or a new version while the capsule comes up. Providing the
// base image to a runner that lacks it can take minutes; the container itself
// starts in seconds.
type StartStatus struct {
	RecordingID string     `json:"recording_id"`
	Status      string     `json:"status"` // running, done or error
	Stage       string     `json:"stage,omitempty"`
	Message     string     `json:"message,omitempty"`
	Current     int64      `json:"current,omitempty"`
	Total       int64      `json:"total,omitempty"`
	Error       string     `json:"error,omitempty"`
	Recording   *Recording `json:"recording,omitempty"`
	StartedAt   time.Time  `json:"started_at"`
	UpdatedAt   time.Time  `json:"updated_at"`
}

// SealStatus reports End & save while it runs. Sealing commits the snapshot on
// the runner, uploads it to the archive in chunks and records the artifact; a
// large image takes minutes, longer than any single HTTP request may last.
type SealStatus struct {
	RecordingID string    `json:"recording_id"`
	Status      string    `json:"status"` // running, done or error
	Stage       string    `json:"stage,omitempty"`
	Message     string    `json:"message,omitempty"`
	Current     int64     `json:"current,omitempty"`
	Total       int64     `json:"total,omitempty"`
	Error       string    `json:"error,omitempty"`
	Artifact    *Artifact `json:"artifact,omitempty"`
	StartedAt   time.Time `json:"started_at"`
	UpdatedAt   time.Time `json:"updated_at"`
}

// Worker is who the Job is with: the assignee once it was handed over,
// the owner before that. New phases run as the worker; a running phase
// finishes as whoever started it.
func (j Job) Worker() string {
	if j.Assignee != "" {
		return j.Assignee
	}
	return j.Owner
}

// AllowsOperator reports whether an operator may work in the Job: its
// owner, or the person it was assigned to.
func (j Job) AllowsOperator(operator string) bool {
	return operator != "" && (j.Owner == operator || j.Assignee == operator)
}

// BrainstormPhaseID names the run a Job starts with when the person wants
// to talk first: a chat in the Job's environment whose only tool is
// start_process(goal). It is not a Template step; the Template's own
// first step follows once the goal is set.
const BrainstormPhaseID = "brainstorm"

// BrainstormPhase is that run as a phase: an agent, no changes, no
// deliverables, and instructions that say what the chat is for.
func BrainstormPhase() WorkflowPhase {
	return WorkflowPhase{
		ID: BrainstormPhaseID, Name: "Brainstorm", Executor: WorkflowExecutorAgent,
		Instructions: "Dit is een brainstorm, geen uitvoering. Alles van deze Job staat al vast (repository, omgeving, Template); alleen de goal nog niet. Verken de repository om te zien wat er al is, denk mee, leg mogelijkheden met voor- en nadelen naast elkaar en stel gerichte vragen in de chat. Bouw en wijzig niets. Werk toe naar één scherpe goal: wat er klaar moet zijn en waaraan je dat ziet. Zodra de gebruiker het eens is, leg je die goal vast met start_process; daarmee begint de gewone flow van de Template.",
		Accept:       WorkflowTransition{Target: WorkflowTargetNext}, Reject: WorkflowTransition{Target: WorkflowTargetSelf},
	}
}

type CreateJobRequest struct {
	Title     string `json:"title"`
	Reference string `json:"reference,omitempty"`
	Objective string `json:"objective"`
	// Brainstorm starts the Job with a chat that sets the goal instead of
	// the Template's first step; Objective may then be empty.
	Brainstorm         bool     `json:"brainstorm,omitempty"`
	ForkedFromJobID    string   `json:"forked_from_job_id,omitempty"`
	IdempotencyKey     string   `json:"idempotency_key,omitempty"`
	AcceptanceCriteria []string `json:"acceptance_criteria,omitempty"`
	Owner              string   `json:"owner,omitempty"`
	Operator           string   `json:"operator,omitempty"`
	GitRepositoryID    string   `json:"git_repository_id"`
	BaseRef            string   `json:"base_ref,omitempty"`
	// Repositories are the Job's repositories with what the Job does in
	// each; the first one to change is the Job's main repository. Empty
	// means GitRepositoryID and BaseRef alone.
	Repositories        []JobRepositoryRequest `json:"repositories,omitempty"`
	Tool                string                 `json:"tool,omitempty"`
	EnvironmentSelector string                 `json:"environment_selector,omitempty"`
	WithSelectors       []string               `json:"with_selectors,omitempty"`
	MCPServerIDs        []string               `json:"mcp_server_ids,omitempty"`
	AttachmentIDs       []string               `json:"attachment_ids,omitempty"`
	Model               string                 `json:"model,omitempty"`
	Run                 bool                   `json:"run,omitempty"` // legacy; Jobs always initialize asynchronously
	TemplateID          string                 `json:"template_id,omitempty"`
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

type AssignJobRequest struct {
	Assignee string `json:"assignee"`
}

// UpdateJobEnvironmentRequest changes what the next steps of a Job start
// with: the agent layer and the MCP connections. Steps already running
// keep what they were started with.
type UpdateJobEnvironmentRequest struct {
	EnvironmentSelector string   `json:"environment_selector"`
	MCPServerIDs        []string `json:"mcp_server_ids"`
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
	Services        []AppService    `json:"services,omitempty"`
	ServiceHosts    []string        `json:"service_hosts,omitempty"`
}

type UpdateGitRepositoryRequest struct {
	Operator        string          `json:"operator,omitempty"`
	Name            string          `json:"name"`
	RemoteURL       string          `json:"remote_url"`
	DefaultRef      string          `json:"default_ref"`
	LayerSelectors  []string        `json:"layer_selectors"`
	CredentialScope CredentialScope `json:"credential_scope,omitempty"`
	Services        []AppService    `json:"services"`
	ServiceHosts    []string        `json:"service_hosts"`
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

// TrackedFolder says whether a tracked path names a folder.
func TrackedFolder(path string) bool { return strings.HasSuffix(path, "/") }

// TrackedCovers says whether a file at path falls under the tracked paths
// and outside the excludes.
func TrackedCovers(path string, tracked, excludes []string) bool {
	covered := false
	for _, candidate := range tracked {
		if candidate == path || (TrackedFolder(candidate) && strings.HasPrefix(path, candidate)) {
			covered = true
			break
		}
	}
	if !covered {
		return false
	}
	for _, exclude := range excludes {
		if exclude == path || (TrackedFolder(exclude) && strings.HasPrefix(path, exclude)) {
			return false
		}
	}
	return true
}
