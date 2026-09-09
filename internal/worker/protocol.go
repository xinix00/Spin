package worker

import (
	"encoding/json"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

const (
	ProtocolVersion = 1

	messageHello        = "hello"
	messageWelcome      = "welcome"
	messageRequest      = "request"
	messageResponse     = "response"
	messageCancel       = "cancel"
	messageStreamData   = "stream_data"
	messageStreamInput  = "stream_input"
	messageStreamResize = "stream_resize"
	messageStreamClose  = "stream_close"
	messageStreamExit   = "stream_exit"
	messageGoodbye      = "goodbye"
)

const (
	methodStartRecording    = "capsule.start_recording"
	methodExecute           = "capsule.execute"
	methodSeal              = "capsule.seal"
	methodCancelRecording   = "capsule.cancel_recording"
	methodMaterialize       = "capsule.materialize"
	methodStop              = "capsule.stop"
	methodProbeEnabled      = "capsule.probe_enabled"
	methodStartEnabled      = "capsule.start_enabled"
	methodStartInteractive  = "capsule.start_interactive"
	methodInspectWorkspace  = "workspace.inspect"
	methodInspectRange      = "workspace.inspect_range"
	methodInjectAttachments = "workspace.inject_attachments"
	methodCaptureLogin      = "home.capture"
	methodWriteHomeFiles    = "home.write"
	methodAcceptWorkspace   = "workspace.accept"
	methodMergeWorkspace    = "workspace.merge"
	methodSyncWorkspace     = "workspace.sync"
	methodBrowseRepository  = "repository.browse"
	methodCompareRepository = "repository.compare"
	methodPullSnapshot      = "snapshot.pull"
	methodRemoveSnapshot    = "snapshot.remove"
	methodExportSnapshot    = "snapshot.export"
	methodImportSnapshot    = "snapshot.import"
	methodArchiveSnapshot   = "snapshot.archive"
	methodHasSnapshot       = "snapshot.has"
	methodStartApp          = "app.start"
	methodStopApp           = "app.stop"
	methodAppStatus         = "app.status"
	methodAppLogs           = "app.logs"
)

// wireMessage is the single versioned control and stream envelope used in
// both directions. JSON keeps the protocol inspectable; []byte fields are
// encoded as base64 and therefore remain safe for arbitrary PTY/stdio data.
type wireMessage struct {
	Version      int                       `json:"version,omitempty"`
	Type         string                    `json:"type"`
	ID           string                    `json:"id,omitempty"`
	Method       string                    `json:"method,omitempty"`
	InstanceID   string                    `json:"instance_id,omitempty"`
	Name         string                    `json:"name,omitempty"`
	Capabilities domain.ClientCapabilities `json:"capabilities,omitempty"`
	Client       *domain.Client            `json:"client,omitempty"`
	Payload      json.RawMessage           `json:"payload,omitempty"`
	Data         []byte                    `json:"data,omitempty"`
	Rows         uint16                    `json:"rows,omitempty"`
	Cols         uint16                    `json:"cols,omitempty"`
	Execution    *capsule.Execution        `json:"execution,omitempty"`
	Error        string                    `json:"error,omitempty"`
	Idle         bool                      `json:"idle,omitempty"`
}

type startRecordingPayload struct {
	Recording domain.Recording  `json:"recording"`
	Parents   []domain.Artifact `json:"parents"`
}

type recordingPayload struct {
	Recording domain.Recording `json:"recording"`
}

type executePayload struct {
	Recording domain.Recording `json:"recording"`
	Input     string           `json:"input"`
}

type materializePayload struct {
	Composition    domain.Composition         `json:"composition"`
	Artifacts      []domain.Artifact          `json:"artifacts"`
	Authentication *capsule.GitAuthentication `json:"authentication,omitempty"`
}

type runtimePayload struct {
	Runtime domain.CapsuleRuntime `json:"runtime"`
}

type mergePayload struct {
	Runtime domain.CapsuleRuntime  `json:"runtime"`
	Merge   capsule.WorkspaceMerge `json:"merge"`
}

type syncPayload struct {
	Runtime domain.CapsuleRuntime `json:"runtime"`
	Sync    capsule.WorkspaceSync `json:"sync"`
}

type repositoryBrowsePayload struct {
	Browse capsule.RepositoryBrowse `json:"browse"`
}

type repositoryComparePayload struct {
	Comparison capsule.RepositoryComparison `json:"comparison"`
}

type appPayload struct {
	Runtime   domain.CapsuleRuntime `json:"runtime,omitempty"`
	SessionID string                `json:"session_id"`
	Services  []domain.AppService   `json:"services,omitempty"`
	Hosts     []string              `json:"hosts,omitempty"`
	Service   string                `json:"service,omitempty"`
	Tail      int                   `json:"tail,omitempty"`
}

type appStatusResult struct {
	Services []domain.AppServiceRuntime `json:"services"`
}

type appLogsResult struct {
	Output string `json:"output"`
}

type enabledPayload struct {
	Runtime    domain.CapsuleRuntime `json:"runtime"`
	Enablement domain.Enablement     `json:"enablement"`
	Request    json.RawMessage       `json:"request,omitempty"`
}

type interactivePayload struct {
	Recording domain.Recording `json:"recording"`
	Input     string           `json:"input"`
	Rows      uint16           `json:"rows"`
	Cols      uint16           `json:"cols"`
}

type inspectRangePayload struct {
	Runtime    domain.CapsuleRuntime       `json:"runtime"`
	Comparison capsule.WorkspaceComparison `json:"comparison"`
}

type attachmentPayload struct {
	TargetPath string `json:"target_path"`
	Data       []byte `json:"data"`
}

type homeFilesPayload struct {
	Runtime    domain.CapsuleRuntime  `json:"runtime"`
	Credential domain.CapsuleSnapshot `json:"credential,omitempty"`
	Files      map[string][]byte      `json:"files,omitempty"`
}

type injectAttachmentsPayload struct {
	Runtime     domain.CapsuleRuntime `json:"runtime"`
	Attachments []attachmentPayload   `json:"attachments"`
}

type acceptWorkspacePayload struct {
	Runtime    domain.CapsuleRuntime       `json:"runtime"`
	Acceptance capsule.WorkspaceAcceptance `json:"acceptance"`
}

type snapshotPayload struct {
	Snapshot domain.CapsuleSnapshot `json:"snapshot"`
}

// snapshotPullPayload asks a runner to fetch an archived snapshot itself,
// in resumable HTTP chunks; Size lets it report progress.
type snapshotPullPayload struct {
	Snapshot domain.CapsuleSnapshot `json:"snapshot"`
	Size     int64                  `json:"size"`
}

// snapshotModePull is the capability a runner advertises for that.
const snapshotModePull = "docker-image-pull"

// presenceResult answers snapshot.has.
type presenceResult struct {
	Present bool `json:"present"`
}

// archiveResult reports what the runner put in the central archive.
type archiveResult struct {
	Ref    string `json:"ref"`
	Digest string `json:"digest"`
	Size   int64  `json:"size"`
}

type streamResponse struct {
	StreamID string `json:"stream_id"`
}
