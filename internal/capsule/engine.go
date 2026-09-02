package capsule

import (
	"context"
	"encoding/json"
	"io"

	"easyacp/internal/domain"
)

type Execution struct {
	Output   string
	ExitCode int
}

// InteractiveProcess is one command attached to a real pseudo-terminal. It is
// deliberately command-oriented: the caller can stream output, forward stdin,
// resize the TTY and persist one audit entry when the process exits.
type InteractiveProcess interface {
	io.ReadWriteCloser
	Resize(rows, cols uint16) error
	Wait() (Execution, error)
}

// InteractiveEngine is optional so metadata-only engines remain small. A web
// client can negotiate this extension without teaching the graph about tools.
type InteractiveEngine interface {
	StartInteractive(context.Context, domain.Recording, string, uint16, uint16) (InteractiveProcess, error)
}

// Engine is the intentionally small boundary between Spin's typed graph and a
// concrete snapshot implementation. Engines never need to understand Codex,
// Claude or any other recorded tool.
type Engine interface {
	Info() domain.CapsuleEngineInfo
	StartRecording(context.Context, domain.Recording, []domain.Artifact) (domain.CapsuleRuntime, error)
	Execute(context.Context, domain.Recording, string) (Execution, error)
	Seal(context.Context, domain.Recording) (domain.CapsuleSnapshot, error)
	Cancel(context.Context, domain.Recording) error
	Materialize(context.Context, domain.Composition, []domain.Artifact) (domain.CapsuleRuntime, error)
	Stop(context.Context, domain.CapsuleRuntime) error
}

// GitAuthentication is resolved by the control plane immediately before a
// checkout. It is never part of a Composition, Artifact or CapsuleRuntime.
type GitAuthentication struct {
	Username    string
	Password    string
	AuthorName  string
	AuthorEmail string
}

// SecretMaterializer is an optional engine extension. The ordinary Engine
// contract remains secret-free; only engines that explicitly implement this
// boundary can receive short-lived checkout authentication.
type SecretMaterializer interface {
	MaterializeWithGitAuthentication(context.Context, domain.Composition, []domain.Artifact, *GitAuthentication) (domain.CapsuleRuntime, error)
}

// EnabledProber is an optional engine extension. The snapshot engine remains
// unaware of protocol semantics; feature hooks can exchange one framed message
// with an entrypoint inside a materialized capsule.
type EnabledProber interface {
	ProbeEnabled(context.Context, domain.CapsuleRuntime, domain.Enablement, json.RawMessage) (json.RawMessage, error)
}

// EnabledProcess is a long-lived, non-TTY stdio entrypoint. Protocol-specific
// clients own framing and semantics; engines only keep the opaque byte stream
// attached to the materialized capsule.
type EnabledProcess interface {
	io.ReadWriteCloser
	Wait() (Execution, error)
}

// EnabledEngine is the streaming counterpart of EnabledProber. It deliberately
// does not mention ACP so future ENABLED protocols can reuse the same boundary.
type EnabledEngine interface {
	StartEnabled(context.Context, domain.CapsuleRuntime, domain.Enablement) (EnabledProcess, error)
}

type WorkspaceFileChange struct {
	Path      string `json:"path"`
	Status    string `json:"status"`
	Added     int    `json:"added"`
	Deleted   int    `json:"deleted"`
	Patch     string `json:"patch,omitempty"`
	Binary    bool   `json:"binary,omitempty"`
	Truncated bool   `json:"truncated,omitempty"`
}

type WorkspaceChanges struct {
	Branch  string                `json:"branch,omitempty"`
	Added   int                   `json:"added"`
	Deleted int                   `json:"deleted"`
	Files   []WorkspaceFileChange `json:"files"`
}

// WorkspaceInspector exposes review metadata through a fixed, read-only
// operation. The web client never gets a generic shell endpoint for Sessions.
type WorkspaceInspector interface {
	InspectWorkspace(context.Context, domain.CapsuleRuntime) (WorkspaceChanges, error)
}

type WorkspaceAttachment struct {
	SourcePath string
	Data       []byte
	TargetPath string
}

// WorkspaceAttachmentInjector copies immutable Job inputs outside /workspace,
// so agents can read them without making them part of the Git worktree.
type WorkspaceAttachmentInjector interface {
	InjectWorkspaceAttachments(context.Context, domain.CapsuleRuntime, []WorkspaceAttachment) error
}

type WorkspaceComparison struct {
	BaseRef            string
	HeadRef            string
	CommitMessageMatch string
	Authentication     *GitAuthentication
}

// WorkspaceRangeInspector compares the current worktree with the merge-base
// of two remote branches. When CommitMessageMatch is set it instead returns
// only the matching commit. Authentication exists only for the transient fetch.
type WorkspaceRangeInspector interface {
	InspectWorkspaceRange(context.Context, domain.CapsuleRuntime, WorkspaceComparison) (WorkspaceChanges, error)
}

type WorkspaceAcceptance struct {
	AllowChanges   bool
	CommitSubject  string
	CommitBody     string
	RemoteRef      string
	Authentication *GitAuthentication
}

type WorkspaceAcceptanceResult struct {
	Head      string
	Committed bool
}

// WorkspaceAcceptor is the single control-plane write boundary for a workflow
// Session. The agent only calls ACCEPT. The engine checks policy, folds the
// final worktree into one commit when needed and publishes HEAD to the Job
// branch without exposing remote credentials to the agent.
type WorkspaceAcceptor interface {
	AcceptWorkspace(context.Context, domain.CapsuleRuntime, WorkspaceAcceptance) (WorkspaceAcceptanceResult, error)
}

type SnapshotRemover interface {
	RemoveSnapshot(context.Context, domain.CapsuleSnapshot) error
}

// SnapshotExporter and SnapshotImporter are optional opaque image-transfer
// hooks. A runner fleet can copy a snapshot to the runner selected for a new
// workload without understanding anything inside the layer.
type SnapshotExporter interface {
	ExportSnapshot(context.Context, domain.CapsuleSnapshot, io.Writer) error
}

type SnapshotImporter interface {
	ImportSnapshot(context.Context, domain.CapsuleSnapshot, io.Reader) error
}

// SnapshotArchive is the server-owned source of truth for immutable Capsule
// snapshots. Docker daemons are caches: a runner may disappear without taking
// an Artifact with it.
type SnapshotArchive interface {
	StoreSnapshot(context.Context, domain.CapsuleSnapshot, io.Reader) error
	RestoreSnapshot(context.Context, domain.CapsuleSnapshot, io.Writer) error
	HasSnapshot(context.Context, domain.CapsuleSnapshot) (bool, error)
	RemoveArchivedSnapshot(context.Context, domain.CapsuleSnapshot) error
}

// Journal keeps unit tests and metadata-only deployments useful without ever
// claiming the resulting digest can be restored.
type Journal struct{}

func (Journal) Info() domain.CapsuleEngineInfo {
	return domain.CapsuleEngineInfo{Driver: "journal", Available: true, Detail: "metadata only; no restorable capsule"}
}

func (Journal) StartRecording(_ context.Context, _ domain.Recording, _ []domain.Artifact) (domain.CapsuleRuntime, error) {
	return domain.CapsuleRuntime{Driver: "journal", Status: "recording"}, nil
}

func (Journal) Execute(_ context.Context, _ domain.Recording, _ string) (Execution, error) {
	return Execution{ExitCode: 0}, nil
}

func (Journal) Seal(_ context.Context, _ domain.Recording) (domain.CapsuleSnapshot, error) {
	return domain.CapsuleSnapshot{Driver: "journal", Restorable: false, IncludesProcessState: false}, nil
}

func (Journal) Cancel(_ context.Context, _ domain.Recording) error { return nil }

func (Journal) Materialize(_ context.Context, _ domain.Composition, _ []domain.Artifact) (domain.CapsuleRuntime, error) {
	return domain.CapsuleRuntime{Driver: "journal", Status: "planned"}, nil
}

func (Journal) Stop(_ context.Context, _ domain.CapsuleRuntime) error { return nil }

func (Journal) RemoveSnapshot(_ context.Context, _ domain.CapsuleSnapshot) error { return nil }
