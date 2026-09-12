package capsule

import (
	"context"
	"encoding/json"
	"io"
	"time"

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

// TrackedFiles reads and writes the files a layer tracks (a login, a
// config an agent rotates) in a running capsule, by absolute path. What
// Spin keeps between Sessions travels through this; nothing else the
// agent did in the capsule comes along.
type TrackedFiles interface {
	ReadTrackedFiles(ctx context.Context, runtime domain.CapsuleRuntime, paths []string) (map[string][]byte, error)
	WriteTrackedFiles(ctx context.Context, runtime domain.CapsuleRuntime, files map[string][]byte) error
}

// CapsuleInspector says what a running capsule changed outside its
// workspace, by kind.
// TrackedWatcher watches tracked files in a capsule and calls changed
// whenever one of them changes, until the capsule ends or ctx is done. The
// runner implements it.
type TrackedWatcher interface {
	WatchTrackedFiles(ctx context.Context, runtime domain.CapsuleRuntime, paths []string, changed func()) error
}

// TrackedSubscriber asks a runner to watch a capsule's tracked files and
// report changes as they happen. The server's remote engine implements it.
type TrackedSubscriber interface {
	SubscribeTrackedFiles(ctx context.Context, runtime domain.CapsuleRuntime, paths []string) error
}

// WorkspaceBundler streams a folder or file of a capsule as a tar: what a
// visual deliverable is made of. The runner implements it.
type WorkspaceBundler interface {
	BundleWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, path string, sink io.Writer) error
}

// DeliverableBundler has a runner bundle a folder or file of a capsule and
// deliver it to the server as a zip. The server's remote engine implements
// it.
type DeliverableBundler interface {
	BundleDeliverable(ctx context.Context, runtime domain.CapsuleRuntime, path string) (domain.DeliverableBundle, error)
}

// BundlePlacer unpacks a tar stream at a path in a capsule: a folder is
// replaced whole, a single file lands at the path itself. The runner
// implements it.
type BundlePlacer interface {
	PlaceBundle(ctx context.Context, runtime domain.CapsuleRuntime, target string, archive io.Reader) error
}

// DeliverablePlacer has a runner fetch a bundle from the server and put it
// at a path in a capsule. The server's remote engine implements it.
type DeliverablePlacer interface {
	PlaceDeliverable(ctx context.Context, runtime domain.CapsuleRuntime, target string, bundle domain.DeliverableBundle) error
}

type CapsuleInspector interface {
	CaptureCapsuleChanges(ctx context.Context, runtime domain.CapsuleRuntime) (domain.LayerContents, error)
}

// TrackedFileLimit bounds one tracked file; larger files are caches.
const TrackedFileLimit = 1 << 20

// WorkspaceAttachmentInjector copies immutable Job inputs outside /workspace,
// so agents can read them without making them part of the Git worktree.
type WorkspaceAttachmentInjector interface {
	InjectWorkspaceAttachments(context.Context, domain.CapsuleRuntime, []WorkspaceAttachment) error
}

type WorkspaceComparison struct {
	BaseRef            string
	HeadRef            string
	CommitMessageMatch string
	// MergeCommit, when set, is the merge that landed the Job on its base
	// branch: the comparison is then what that merge brought in, since
	// the base branch now contains the Job and a merge-base would find
	// nothing.
	MergeCommit    string
	Authentication *GitAuthentication
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

// WorkspaceMerge lands a Job: its branch merged into the base branch and
// pushed, from a workspace that holds the operator's Git identity for the
// moment of the push only.
type WorkspaceMerge struct {
	SourceRef      string // the Job branch
	TargetRef      string // the base branch the Job lands on
	CommitSubject  string
	CommitBody     string
	Authentication *GitAuthentication
}

// WorkspaceMergeResult is the merge commit that now heads the base branch.
type WorkspaceMergeResult struct {
	Head string
}

// WorkspaceSync pushes a Session's work in progress to its own branch on
// the remote, so a Session can continue on any runner and nothing lives
// only in one Docker volume. Dirty files become a WIP commit first; ACCEPT
// folds those commits away later.
type WorkspaceSync struct {
	SessionRef     string // the Session branch on the remote
	Authentication *GitAuthentication
}

type WorkspaceSyncResult struct {
	Head      string
	Committed bool // a WIP commit was made
	Pushed    bool // the remote branch moved
}

// WorkspaceSyncer pushes work in progress.
type WorkspaceSyncer interface {
	SyncWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, sync WorkspaceSync) (WorkspaceSyncResult, error)
}

// What a repository browse returns: the files of a ref, and one file.
type WorkspaceEntry struct {
	Path string `json:"path"`
	Size int64  `json:"size"`
}

type WorkspaceTree struct {
	Ref     string           `json:"ref"`
	Entries []WorkspaceEntry `json:"entries"`
}

type WorkspaceFile struct {
	Ref       string `json:"ref"`
	Path      string `json:"path"`
	Size      int64  `json:"size"`
	Content   string `json:"content"`
	Binary    bool   `json:"binary"`
	Truncated bool   `json:"truncated"`
}

// WorkspaceFileLimit is how much of a file the browser reads.
const WorkspaceFileLimit = 512 << 10

// A repository can be explored without any Job: the runner keeps a shallow
// clone per remote in a volume of its own and reads refs, trees and files
// from it on demand, fetching the asked ref first.
type RepositoryBrowse struct {
	RemoteURL      string
	CacheKey       string // names the runner's clone volume; stable per repository
	Mode           string // refs, tree or file
	Ref            string
	Path           string
	Authentication *GitAuthentication
}

// RepositoryRef is a branch with the time of its last commit; refs come
// newest first, which in practice is "least stale".
type RepositoryRef struct {
	Name        string    `json:"name"`
	CommittedAt time.Time `json:"committed_at"`
}

type RepositoryBrowseResult struct {
	Refs []RepositoryRef `json:"refs,omitempty"`
	Tree *WorkspaceTree  `json:"tree,omitempty"`
	File *WorkspaceFile  `json:"file,omitempty"`
}

// RepositoryComparison compares two branches of a repository on the runner's
// own clone: no composition, no images, only git. CacheKey names the clone
// volume, stable per repository.
type RepositoryComparison struct {
	RemoteURL  string
	CacheKey   string
	Comparison WorkspaceComparison
}

type RepositoryComparer interface {
	CompareRepository(ctx context.Context, comparison RepositoryComparison) (WorkspaceChanges, error)
}

type RepositoryBrowser interface {
	BrowseRepository(ctx context.Context, browse RepositoryBrowse) (RepositoryBrowseResult, error)
}

// WorkspaceMerger merges a Job branch into its base branch on the remote.
type WorkspaceMerger interface {
	MergeWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, merge WorkspaceMerge) (WorkspaceMergeResult, error)
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

// AppServiceHost runs a repository's app services next to a Session's
// capsule: same image, same workspace volume, an own network per Session,
// host env files and published ports, so a person can test the app.
type AppServiceHost interface {
	StartAppServices(ctx context.Context, runtime domain.CapsuleRuntime, sessionID string, services []domain.AppService, hosts []string) ([]domain.AppServiceRuntime, error)
	StopAppServices(ctx context.Context, sessionID string) error
	AppServiceStatus(ctx context.Context, sessionID string) ([]domain.AppServiceRuntime, error)
	AppServiceLogs(ctx context.Context, sessionID, service string, tail int) (string, error)
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

// SnapshotArchiver delivers a sealed snapshot to the central archive itself,
// as separate acknowledged chunks, instead of streaming it through the caller.
type SnapshotArchiver interface {
	ArchiveSnapshot(context.Context, domain.CapsuleSnapshot) error
}

type SnapshotImporter interface {
	ImportSnapshot(context.Context, domain.CapsuleSnapshot, io.Reader) error
}

// SnapshotChecker reports whether a runner already holds a snapshot locally.
// Durable placement records can go stale, after a restore for instance, and
// asking is far cheaper than shipping a gigabyte that is already there.
type SnapshotChecker interface {
	HasSnapshot(context.Context, domain.CapsuleSnapshot) (bool, error)
}

// Progress receives stage updates from slow engine work, so a caller that
// answered early can show what the runner is doing. Current and total are
// bytes when known and zero otherwise.
type Progress func(stage, message string, current, total int64)

type progressKey struct{}

// WithProgress attaches a progress reporter to ctx.
func WithProgress(ctx context.Context, report Progress) context.Context {
	return context.WithValue(ctx, progressKey{}, report)
}

// ReportProgress calls the reporter attached to ctx, if any.
func ReportProgress(ctx context.Context, stage, message string, current, total int64) {
	if report, ok := ctx.Value(progressKey{}).(Progress); ok && report != nil {
		report(stage, message, current, total)
	}
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
