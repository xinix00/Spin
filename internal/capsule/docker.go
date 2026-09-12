//go:build !tamago

package capsule

import (
	"bufio"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"easyacp/internal/domain"
	"github.com/creack/pty"
)

type DockerConfig struct {
	Binary    string
	BaseImage string
	Network   string
	// EnvDir holds the env files app services may use (<name>.env).
	EnvDir string
	// AdvertiseHost is the address people use to reach published app ports.
	AdvertiseHost string
	// Logger receives what the engine decides; nil is quiet.
	Logger *slog.Logger
}

type Docker struct {
	logger        *slog.Logger
	binary        string
	baseImage     string
	network       string
	envDir        string
	advertiseHost string
	info          domain.CapsuleEngineInfo
}

func NewDocker(ctx context.Context, cfg DockerConfig) (*Docker, error) {
	if cfg.Binary == "" {
		cfg.Binary = findDockerBinary()
	}
	if cfg.BaseImage == "" {
		cfg.BaseImage = "alpine:3.24"
	}
	if cfg.Network == "" {
		cfg.Network = "bridge"
	}
	d := &Docker{logger: cfg.Logger, binary: cfg.Binary, baseImage: cfg.BaseImage, network: cfg.Network, envDir: cfg.EnvDir, advertiseHost: AdvertiseHost(cfg.AdvertiseHost)}
	version, code, err := d.run(ctx, "version", "--format", "{{.Server.Version}}")
	if err != nil || code != 0 || strings.TrimSpace(version) == "" {
		return nil, fmt.Errorf("Docker daemon is unavailable: %s: %w", strings.TrimSpace(version), err)
	}
	d.info = domain.CapsuleEngineInfo{
		Driver:                   "docker",
		Available:                true,
		BaseImage:                d.baseImage,
		FilesystemSnapshots:      true,
		ProcessCheckpoints:       false,
		InteractiveAttachCommand: true,
		Detail:                   "Docker image commit/clone; process memory and provider KV cache are not included",
	}
	return d, nil
}

func (d *Docker) Info() domain.CapsuleEngineInfo { return d.info }

func (d *Docker) StartRecording(ctx context.Context, recording domain.Recording, parents []domain.Artifact) (domain.CapsuleRuntime, error) {
	base := d.baseImage
	if len(parents) > 1 {
		return domain.CapsuleRuntime{}, fmt.Errorf("Docker recording requires one linear parent snapshot; got %d", len(parents))
	}
	if len(parents) == 1 {
		parent := parents[0]
		if parent.Snapshot.Driver != "docker" || !parent.Snapshot.Restorable || parent.Snapshot.Ref == "" {
			return domain.CapsuleRuntime{}, fmt.Errorf("parent artifact %s is not a restorable Docker snapshot", parent.ID)
		}
		base = parent.Snapshot.Ref
	}
	name := runtimeName("spin-rec", recording.ID)
	// The control plane may ask again for a capsule this runner already made:
	// its own request timed out, or it restarted before it could note the
	// runtime. The recording's capsule is the one named after it, so hand
	// that back rather than failing on the name.
	if id, err := d.containerID(ctx, name); err == nil && id != "" {
		if _, err := d.control(ctx, "start", id); err != nil {
			return domain.CapsuleRuntime{}, fmt.Errorf("resume capsule %s: %w", name, err)
		}
		return domain.CapsuleRuntime{
			Driver:        "docker",
			ContainerID:   id,
			ContainerName: name,
			BaseRef:       base,
			AttachCommand: "docker exec -it " + id + " sh",
			Status:        "recording",
		}, nil
	}
	_, err := d.control(ctx,
		"run", "-d", "--pull=missing", "--init", "--name", name,
		"--label", "spin.managed=true",
		"--label", "spin.kind=recording",
		"--label", "spin.recording_id="+recording.ID,
		"--network", d.network,
		"--env", "DISABLE_AUTOUPDATER=1",
		"--workdir", "/workspace",
		"--entrypoint", "sh",
		base, "-lc", "trap 'exit 0' TERM INT; while :; do sleep 3600; done",
	)
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	id, err := d.containerID(ctx, name)
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	return domain.CapsuleRuntime{
		Driver:        "docker",
		ContainerID:   id,
		ContainerName: name,
		BaseRef:       base,
		AttachCommand: "docker exec -it " + id + " sh",
		Status:        "recording",
	}, nil
}

func (d *Docker) Execute(ctx context.Context, recording domain.Recording, input string) (Execution, error) {
	if recording.Runtime == nil || recording.Runtime.Driver != "docker" || recording.Runtime.ContainerID == "" {
		return Execution{}, errors.New("recording has no live Docker capsule")
	}
	output, code, err := d.run(ctx, "exec", "-i", "-w", "/workspace", recording.Runtime.ContainerID, "sh", "-lc", input)
	if err != nil && code < 0 {
		return Execution{}, err
	}
	return Execution{Output: strings.TrimSpace(output), ExitCode: code}, nil
}

func (d *Docker) StartInteractive(ctx context.Context, recording domain.Recording, input string, rows, cols uint16) (InteractiveProcess, error) {
	if recording.Runtime == nil || recording.Runtime.Driver != "docker" || recording.Runtime.ContainerID == "" {
		return nil, errors.New("recording has no live Docker capsule")
	}
	if strings.TrimSpace(input) == "" {
		return nil, errors.New("interactive command is required")
	}
	if rows == 0 {
		rows = 30
	}
	if cols == 0 {
		cols = 120
	}
	cmd := exec.CommandContext(ctx, d.binary,
		"exec", "-it", "-w", "/workspace", "-e", "TERM=xterm-256color", recording.Runtime.ContainerID,
		"sh", "-lc", input,
	)
	// The Docker CLI prints a "What's next" hint after exec on Docker
	// Desktop; it would land in the person's terminal.
	cmd.Env = append(os.Environ(), "DOCKER_CLI_HINTS=false")
	terminal, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: rows, Cols: cols})
	if err != nil {
		return nil, err
	}
	return &dockerInteractiveProcess{cmd: cmd, terminal: terminal}, nil
}

type dockerInteractiveProcess struct {
	cmd        *exec.Cmd
	terminal   *os.File
	mu         sync.Mutex
	transcript bytes.Buffer
	waitOnce   sync.Once
	execution  Execution
	waitErr    error
}

func (p *dockerInteractiveProcess) Read(buffer []byte) (int, error) {
	n, err := p.terminal.Read(buffer)
	if n > 0 {
		p.mu.Lock()
		remaining := (1 << 20) - p.transcript.Len()
		if remaining > 0 {
			if n < remaining {
				remaining = n
			}
			_, _ = p.transcript.Write(buffer[:remaining])
		}
		p.mu.Unlock()
	}
	if errors.Is(err, syscall.EIO) {
		return n, io.EOF
	}
	return n, err
}

func (p *dockerInteractiveProcess) Write(buffer []byte) (int, error) {
	return p.terminal.Write(buffer)
}

func (p *dockerInteractiveProcess) Close() error {
	return p.terminal.Close()
}

func (p *dockerInteractiveProcess) Resize(rows, cols uint16) error {
	if rows == 0 || cols == 0 {
		return nil
	}
	return pty.Setsize(p.terminal, &pty.Winsize{Rows: rows, Cols: cols})
}

func (p *dockerInteractiveProcess) Wait() (Execution, error) {
	p.waitOnce.Do(func() {
		err := p.cmd.Wait()
		code := 0
		if err != nil {
			var exitErr *exec.ExitError
			if errors.As(err, &exitErr) {
				code = exitErr.ExitCode()
			} else {
				code = -1
				p.waitErr = err
			}
		}
		p.mu.Lock()
		output := strings.TrimSpace(p.transcript.String())
		p.mu.Unlock()
		p.execution = Execution{Output: output, ExitCode: code}
	})
	return p.execution, p.waitErr
}

func (d *Docker) Seal(ctx context.Context, recording domain.Recording) (domain.CapsuleSnapshot, error) {
	if recording.Runtime == nil || recording.Runtime.Driver != "docker" || recording.Runtime.ContainerID == "" {
		return domain.CapsuleSnapshot{}, errors.New("recording has no live Docker capsule")
	}
	tag := "spin/artifact:" + safeName(recording.ID)
	d.tidyCapsule(ctx, recording.Runtime.ContainerID)
	parentImage, _ := d.control(ctx, "inspect", "--format", "{{.Config.Image}}", recording.Runtime.ContainerID)
	parentImage = strings.TrimSpace(parentImage)
	// Every layer, a new version of one as well, commits over the image it
	// was recorded on; the archive later holds only its own difference.
	_, err := d.control(ctx,
		"commit", "--pause=true",
		"--change", "LABEL spin.managed=true",
		"--change", "LABEL spin.recording_id="+recording.ID,
		recording.Runtime.ContainerID, tag,
	)
	var contents *domain.LayerContents
	if err != nil {
		// A retried save: the container was already committed and removed
		// by an attempt whose answer never reached the server. The image tagged
		// for this recording is that snapshot.
		if _, inspectErr := d.control(ctx, "image", "inspect", "--format", "{{.Id}}", tag); inspectErr != nil {
			return domain.CapsuleSnapshot{}, err
		}
	} else {
		// The layer keeps its real difference: what is byte-for-byte the
		// same in the layer below, and every cache, goes.
		cleaned, cleanErr := d.cleanLayer(ctx, tag, parentImage, recording.ID)
		if cleanErr != nil && d.logger != nil {
			d.logger.Warn("seal: the layer keeps its full diff", "recording", recording.ID, "error", cleanErr)
		}
		contents = cleaned
	}
	digest, err := d.control(ctx, "image", "inspect", "--format", "{{.Id}}", tag)
	if err != nil {
		return domain.CapsuleSnapshot{}, err
	}
	rootFS, err := d.imageRootFS(ctx, tag)
	if err != nil {
		return domain.CapsuleSnapshot{}, err
	}
	content, _, err := d.layerIdentity(ctx, tag, parentImage)
	if err != nil {
		return domain.CapsuleSnapshot{}, fmt.Errorf("identify the layer: %w", err)
	}
	delta := strings.HasPrefix(parentImage, "spin/artifact:")
	// The immutable image is already safe when cleanup fails, so leave any
	// stubborn container discoverable through its spin.* labels.
	_, _, _ = d.run(ctx, "rm", "-f", recording.Runtime.ContainerID)
	result := domain.CapsuleSnapshot{
		Driver:               "docker",
		Ref:                  tag,
		Digest:               strings.TrimSpace(digest),
		RootFS:               rootFS,
		Restorable:           true,
		IncludesProcessState: false,
		Contents:             contents,
		Content:              content,
		Delta:                delta,
	}
	if delta {
		result.ParentRef = parentImage
	}
	return result, nil
}

// tidyCapsule drops caches that have no business in a layer before it is
// sealed: package-manager download caches and temp files. Failure is not
// fatal; the layer is then merely bigger.
func (d *Docker) tidyCapsule(ctx context.Context, containerID string) {
	_, _, _ = d.run(ctx, "exec", containerID, "sh", "-c", "rm -rf /root/.npm/_cacache /root/.cache/pip /root/.cache/go-build /tmp/* /var/cache/apk/* 2>/dev/null; true")
}

// Cancel removes the recording's capsule. Without a known container ID it
// removes the container named after the recording, so a capsule whose start
// the control plane abandoned does not linger.
func (d *Docker) Cancel(ctx context.Context, recording domain.Recording) error {
	if recording.Runtime != nil && recording.Runtime.ContainerID != "" {
		return d.removeContainer(ctx, recording.Runtime.ContainerID)
	}
	return d.removeContainer(ctx, runtimeName("spin-rec", recording.ID))
}

func (d *Docker) Materialize(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact) (domain.CapsuleRuntime, error) {
	return d.MaterializeWithGitAuthentication(ctx, composition, artifacts, nil)
}

func (d *Docker) MaterializeWithGitAuthentication(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact, authentication *GitAuthentication) (domain.CapsuleRuntime, error) {
	selected, ephemeral, err := d.materializationArtifact(ctx, composition, artifacts)
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	keepImage := false
	if ephemeral {
		defer func() {
			if !keepImage {
				_ = d.removeImage(context.Background(), selected.Snapshot.Ref)
			}
		}()
	}
	workspaceRef := ""
	if len(composition.GitWorkspaces()) > 0 {
		workspaceRef, err = d.prepareGitWorkspaces(ctx, composition, selected, authentication)
		if err != nil {
			return domain.CapsuleRuntime{}, fmt.Errorf("prepare Git workspace: %w", err)
		}
	}
	name := runtimeName("spin-use", composition.ID)
	args := []string{
		"run", "-d", "--init", "--name", name,
		"--label", "spin.managed=true",
		"--label", "spin.kind=composition",
		"--label", "spin.composition_id=" + composition.ID,
		"--label", "spin.operator=" + safeName(composition.Operator),
		"--network", d.network,
		// A tool that updates itself would write the update into whatever
		// layer is being recorded (a login layer, say); the tool belongs
		// in its own layer, updated there on purpose.
		"--env", "DISABLE_AUTOUPDATER=1",
	}
	if workspaceRef != "" {
		args = append(args, "--mount", "type=volume,src="+workspaceRef+",dst=/workspace")
	}
	args = append(args,
		"--workdir", "/workspace",
		"--entrypoint", "sh",
		selected.Snapshot.Ref, "-lc", "trap 'exit 0' TERM INT; while :; do sleep 3600; done",
	)
	_, err = d.control(ctx, args...)
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	id, err := d.containerID(ctx, name)
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	keepImage = true
	return domain.CapsuleRuntime{
		Driver:        "docker",
		ContainerID:   id,
		ContainerName: name,
		BaseRef:       selected.Snapshot.Ref,
		WorkspaceRef:  workspaceRef,
		AttachCommand: "docker exec -it " + id + " sh",
		Status:        "ready",
	}, nil
}

// workspaceDirectory is the checkout directory of a repository in the
// capsule: the workspace root, or a folder below it for a Job with several
// repositories. The folder name is checked like any path a person names.
func workspaceDirectory(path string) (string, error) {
	if path == "" {
		return domain.WorkspaceRoot, nil
	}
	if !validHomePath(path) || strings.Contains(path, "/") {
		return "", fmt.Errorf("invalid workspace folder %q", path)
	}
	return domain.WorkspaceDirectory(path), nil
}

// prepareGitWorkspaces makes the Session's workspace volume with every
// repository of the Job checked out in it: the ones the Session changes on
// their Session branch, the reference ones at their base, read-only.
func (d *Docker) prepareGitWorkspaces(ctx context.Context, composition domain.Composition, selected domain.Artifact, authentication *GitAuthentication) (string, error) {
	if composition.SessionID == "" {
		return "", errors.New("Git workspace requires a Session")
	}
	if selected.Snapshot.Driver != "docker" || !selected.Snapshot.Restorable || selected.Snapshot.Ref == "" {
		return "", fmt.Errorf("Git helper artifact %s is not a restorable Docker snapshot", selected.ID)
	}
	volume := runtimeName("spin-work", composition.SessionID)
	if _, err := d.control(ctx,
		"volume", "create",
		"--label", "spin.managed=true",
		"--label", "spin.kind=workspace",
		"--label", "spin.session_id="+composition.SessionID,
		volume,
	); err != nil {
		return "", err
	}
	for _, workspace := range composition.GitWorkspaces() {
		if err := d.prepareGitWorkspace(ctx, volume, workspace, selected, authentication); err != nil {
			return "", fmt.Errorf("%s: %w", workspace.RepositoryName, err)
		}
	}
	return volume, nil
}

func (d *Docker) prepareGitWorkspace(ctx context.Context, volume string, workspace domain.GitWorkspace, selected domain.Artifact, authentication *GitAuthentication) error {
	directory, err := workspaceDirectory(workspace.Path)
	if err != nil {
		return err
	}
	requiresAuthentication := workspace.AccountID != "" || workspace.CredentialScope == domain.CredentialScopeUser || workspace.CredentialScope == domain.CredentialScopeGlobal
	if requiresAuthentication && (authentication == nil || authentication.Password == "") {
		return errors.New("Git account is bound but no checkout authentication was supplied")
	}
	script := gitWorkspaceScript
	if !workspace.Changes() {
		script = gitReferenceScript
	}
	secretInput := []byte("\n\n\n\n")
	if authentication != nil {
		secretInput = []byte(strings.Join([]string{
			authentication.Username,
			authentication.Password,
			authentication.AuthorName,
			authentication.AuthorEmail,
		}, "\n") + "\n")
	}
	_, err = d.controlInput(ctx, secretInput,
		"run", "-i", "--rm", "--read-only", "--tmpfs", "/tmp:rw,nosuid,nodev,size=1m",
		"--label", "spin.managed=true",
		"--label", "spin.kind=git-checkout",
		"--network", d.network,
		"--mount", "type=volume,src="+volume+",dst=/workspace",
		"--workdir", "/workspace",
		"--env", "GIT_TERMINAL_PROMPT=0",
		"--env", "SPIN_GIT_DIR="+directory,
		"--env", "SPIN_GIT_REMOTE="+workspace.RemoteURL,
		"--env", "SPIN_GIT_BASE="+workspace.BaseRef,
		"--env", "SPIN_GIT_BOOTSTRAP="+workspace.BootstrapRef,
		"--env", "SPIN_GIT_HEAD="+workspace.HeadRef,
		"--env", "SPIN_GIT_CONTEXT="+strings.Join(workspace.ContextRefs, " "),
		"--env", "SPIN_GIT_TARGET="+workspace.TargetRef,
		"--entrypoint", "sh",
		selected.Snapshot.Ref, "-lc", script,
	)
	return err
}

const gitCredentialEnvironmentScript = `export SPIN_GIT_USERNAME SPIN_GIT_PASSWORD
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=credential.helper
export GIT_CONFIG_VALUE_0='!f() { printf "username=%s\npassword=%s\n" "$SPIN_GIT_USERNAME" "$SPIN_GIT_PASSWORD"; }; f'`

// gitReferenceScript checks a repository out at its base for reading only:
// shallow, on a local branch named after the base, with pushing disabled.
const gitReferenceScript = `set -eu
command -v git >/dev/null
mkdir -p "${SPIN_GIT_DIR:-.}" && cd "${SPIN_GIT_DIR:-.}"
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
IFS= read -r SPIN_GIT_AUTHOR_NAME || true
IFS= read -r SPIN_GIT_AUTHOR_EMAIL || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  ` + gitCredentialEnvironmentScript + `
fi
if [ ! -d .git ]; then
  git init -q
  git remote add origin "$SPIN_GIT_REMOTE"
fi
git fetch -q --depth=1 origin "+refs/heads/${SPIN_GIT_BASE}:refs/remotes/origin/${SPIN_GIT_BASE}"
git checkout -q -B "$SPIN_GIT_BASE" "refs/remotes/origin/${SPIN_GIT_BASE}"
git remote set-url --push origin no_push
git config spin.reference 1
unset SPIN_GIT_PASSWORD`

const gitWorkspaceScript = `set -eu
command -v git >/dev/null
mkdir -p "${SPIN_GIT_DIR:-.}" && cd "${SPIN_GIT_DIR:-.}"
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
IFS= read -r SPIN_GIT_AUTHOR_NAME || true
IFS= read -r SPIN_GIT_AUTHOR_EMAIL || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  ` + gitCredentialEnvironmentScript + `
fi
if [ ! -d .git ]; then
  git init -q
  git remote add origin "$SPIN_GIT_REMOTE"
else
  test "$(git config --get remote.origin.url)" = "$SPIN_GIT_REMOTE"
fi
if ! git ls-remote --exit-code origin "refs/heads/$SPIN_GIT_TARGET" >/dev/null 2>&1; then
  git fetch --depth=1 origin "$SPIN_GIT_BOOTSTRAP"
  SPIN_BOOTSTRAP_HEAD="$(git rev-parse FETCH_HEAD)"
  if ! git push origin "$SPIN_BOOTSTRAP_HEAD:refs/heads/$SPIN_GIT_TARGET"; then
    git ls-remote --exit-code origin "refs/heads/$SPIN_GIT_TARGET" >/dev/null
  fi
fi
# A workspace whose Session branch exists is complete: reuse it as it is.
# Shallow fetches must not be repeated into an existing shallow clone (git
# refuses with "shallow file has changed"), so a half-made workspace, from
# a launch that failed before the branch existed, starts over instead.
if git show-ref --verify --quiet "refs/heads/$SPIN_GIT_HEAD"; then
  git checkout -q "$SPIN_GIT_HEAD"
else
  if [ -e .git/shallow ] || [ -n "$(git for-each-ref refs/remotes 2>/dev/null)" ]; then
    # Nothing of value exists yet: the Session branch was never made.
    find . -mindepth 1 -maxdepth 1 -exec rm -rf {} +
    git init -q
    git remote add origin "$SPIN_GIT_REMOTE"
  fi
  # The agent must be able to see what the Job did before this phase and
  # where the Job will land: the base branch as a remote ref, and the Job
  # branch with its history since that base (falling back to a bounded
  # depth when the remote cannot exclude by ref). Both are shallow; nothing
  # older than the Job is pulled in.
  git fetch -q --depth=1 origin "+refs/heads/${SPIN_GIT_BOOTSTRAP}:refs/remotes/origin/${SPIN_GIT_BOOTSTRAP}" || true
  git fetch -q --shallow-exclude="$SPIN_GIT_BOOTSTRAP" origin "+refs/heads/${SPIN_GIT_BASE}:refs/remotes/origin/${SPIN_GIT_BASE}" 2>/dev/null \
    || git fetch -q --depth=100 origin "+refs/heads/${SPIN_GIT_BASE}:refs/remotes/origin/${SPIN_GIT_BASE}"
  # Branches given as context (the Job this one continues) come along as
  # remote refs, with their commits since the base, read-only.
  for SPIN_CONTEXT_REF in ${SPIN_GIT_CONTEXT:-}; do
    git fetch -q --shallow-exclude="$SPIN_GIT_BOOTSTRAP" origin "+refs/heads/${SPIN_CONTEXT_REF}:refs/remotes/origin/${SPIN_CONTEXT_REF}" 2>/dev/null \
      || git fetch -q --depth=100 origin "+refs/heads/${SPIN_CONTEXT_REF}:refs/remotes/origin/${SPIN_CONTEXT_REF}" || true
  done
  # A Session whose work in progress was pushed continues from that branch
  # (on any runner); its base stays the Job branch it started from.
  if git ls-remote --exit-code origin "refs/heads/$SPIN_GIT_HEAD" >/dev/null 2>&1; then
    git fetch -q --shallow-exclude="$SPIN_GIT_BOOTSTRAP" origin "+refs/heads/${SPIN_GIT_HEAD}:refs/remotes/origin/${SPIN_GIT_HEAD}" 2>/dev/null \
      || git fetch -q --depth=100 origin "+refs/heads/${SPIN_GIT_HEAD}:refs/remotes/origin/${SPIN_GIT_HEAD}"
    git checkout -q -B "$SPIN_GIT_HEAD" "refs/remotes/origin/${SPIN_GIT_HEAD}"
    git config spin.baseCommit "$(git merge-base "refs/remotes/origin/${SPIN_GIT_BASE}" HEAD 2>/dev/null || git rev-parse "refs/remotes/origin/${SPIN_GIT_BASE}")"
  else
    git checkout -q -B "$SPIN_GIT_HEAD" "refs/remotes/origin/${SPIN_GIT_BASE}"
  fi
fi
git config spin.targetRef "$SPIN_GIT_TARGET"
if ! git config --get spin.baseCommit >/dev/null 2>&1; then
  SPIN_INITIAL_HEAD="$(git reflog show --format=%H "$SPIN_GIT_HEAD" | tail -n 1)"
  git config spin.baseCommit "${SPIN_INITIAL_HEAD:-$(git rev-parse HEAD)}"
fi
if [ -n "$SPIN_GIT_AUTHOR_NAME" ]; then git config user.name "$SPIN_GIT_AUTHOR_NAME"; fi
if [ -n "$SPIN_GIT_AUTHOR_EMAIL" ]; then git config user.email "$SPIN_GIT_AUTHOR_EMAIL"; fi
unset SPIN_GIT_PASSWORD`

func (d *Docker) Stop(ctx context.Context, runtime domain.CapsuleRuntime) error {
	if runtime.ContainerID != "" {
		if err := d.removeContainer(ctx, runtime.ContainerID); err != nil {
			return err
		}
	}
	if strings.HasPrefix(runtime.BaseRef, "spin/composition:") {
		return d.removeImage(ctx, runtime.BaseRef)
	}
	return nil
}

func (d *Docker) RemoveSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot) error {
	if snapshot.Driver != "docker" || strings.TrimSpace(snapshot.Ref) == "" {
		return nil
	}
	return d.removeImage(ctx, snapshot.Ref)
}

// ExportSnapshot writes the image as a gzip-compressed docker save stream.
// Compressing at the source is what makes the archive, the upload and every
// later restore smaller, where compressing on the wire only would leave the
// archive at full size; docker load reads gzip natively, so the import side
// and older uncompressed archives need nothing.
func (d *Docker) ExportSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot, destination io.Writer) error {
	if snapshot.Driver != "docker" || strings.TrimSpace(snapshot.Ref) == "" {
		return errors.New("snapshot is not an exportable Docker image")
	}
	if snapshot.Delta && snapshot.ParentRef != "" {
		return d.exportDelta(ctx, snapshot, destination)
	}
	compressor, err := gzip.NewWriterLevel(destination, gzip.BestSpeed)
	if err != nil {
		return err
	}
	cmd := exec.CommandContext(ctx, d.binary, "image", "save", snapshot.Ref)
	cmd.Stdout = compressor
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		_ = compressor.Close()
		return fmt.Errorf("docker image save %s: %s: %w", snapshot.Ref, strings.TrimSpace(stderr.String()), err)
	}
	return compressor.Close()
}

// HasSnapshot reports whether this daemon holds the image the snapshot names,
// with the digest it was sealed with.
func (d *Docker) HasSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot) (bool, error) {
	if snapshot.Driver != "docker" || strings.TrimSpace(snapshot.Ref) == "" {
		return false, nil
	}
	id, _, err := d.run(ctx, "image", "inspect", "--format", "{{.Id}}", snapshot.Ref)
	if err != nil || strings.TrimSpace(id) == "" {
		return false, nil
	}
	// The same image on another image store has another ID; its layers
	// tell whether the cached image is the recorded one.
	if snapshot.Content != "" && d.imageLabel(ctx, snapshot.Ref, contentLabel) == snapshot.Content {
		return true, nil
	}
	if snapshot.RootFS == "" {
		return false, nil
	}
	rootFS, err := d.imageRootFS(ctx, snapshot.Ref)
	return err == nil && rootFS == snapshot.RootFS, nil
}

func (d *Docker) ImportSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot, source io.Reader) error {
	if snapshot.Driver != "docker" || strings.TrimSpace(snapshot.Ref) == "" {
		return errors.New("snapshot is not an importable Docker image")
	}
	seekable, ok := source.(io.ReadSeeker)
	if !ok {
		spool, err := os.CreateTemp("", "spin-import-*.tar.gz")
		if err != nil {
			return err
		}
		defer func() {
			_ = spool.Close()
			_ = os.Remove(spool.Name())
		}()
		if _, err := io.Copy(spool, source); err != nil {
			return err
		}
		seekable = spool
	}
	if _, err := seekable.Seek(0, io.SeekStart); err != nil {
		return err
	}
	if note, err := readDeltaNote(seekable); err != nil {
		return err
	} else if note != nil {
		return d.importDelta(ctx, snapshot, note, seekable)
	}
	cmd := exec.CommandContext(ctx, d.binary, "image", "load")
	cmd.Stdin = seekable
	var output bytes.Buffer
	cmd.Stdout = &output
	cmd.Stderr = &output
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("docker image load %s: %s: %w", snapshot.Ref, strings.TrimSpace(output.String()), err)
	}
	loadedDigest, err := d.control(ctx, "image", "inspect", "--format", "{{.Id}}", snapshot.Ref)
	if err != nil {
		return fmt.Errorf("verify imported image %s: %w", snapshot.Ref, err)
	}
	loadedRootFS, err := d.imageRootFS(ctx, snapshot.Ref)
	if err != nil {
		return fmt.Errorf("verify imported image %s: %w", snapshot.Ref, err)
	}
	if err := verifyImportedImage(snapshot, strings.TrimSpace(loadedDigest), loadedRootFS); err != nil {
		return err
	}
	return nil
}

// imageRootFS digests an image's layer diff IDs: what the image is made
// of, the same on every runner whatever image store its Docker uses.
func (d *Docker) imageRootFS(ctx context.Context, ref string) (string, error) {
	layers, err := d.control(ctx, "image", "inspect", "--format", "{{json .RootFS.Layers}}", ref)
	if err != nil {
		return "", err
	}
	return rootFSDigest(layers), nil
}

func rootFSDigest(layersJSON string) string {
	var layers []string
	if json.Unmarshal([]byte(strings.TrimSpace(layersJSON)), &layers) != nil || len(layers) == 0 {
		return ""
	}
	sum := sha256.Sum256([]byte(strings.Join(layers, "\n")))
	return "sha256:" + hex.EncodeToString(sum[:])
}

// verifyImportedImage accepts a loaded image when its layers are the
// recorded ones. The image ID alone is not enough: the classic store and
// the containerd store give the same image different IDs, so a snapshot
// recorded on one runner would never verify on the other. A snapshot from
// before layers were recorded is accepted by ID, or, when the ID differs,
// on the strength of the archive it came from.
func verifyImportedImage(snapshot domain.CapsuleSnapshot, loadedID, loadedRootFS string) error {
	if snapshot.RootFS == "" {
		return fmt.Errorf("snapshot %s records no layers to verify against", snapshot.Ref)
	}
	if loadedRootFS != snapshot.RootFS {
		return fmt.Errorf("imported image %s has layers %s, expected %s", snapshot.Ref, loadedRootFS, snapshot.RootFS)
	}
	return nil
}

func (d *Docker) removeImage(ctx context.Context, ref string) error {
	output, code, err := d.run(ctx, "image", "rm", ref)
	if code == 0 || strings.Contains(output, "No such image") {
		return nil
	}
	return fmt.Errorf("docker image rm failed (exit %d): %s: %w", code, strings.TrimSpace(output), err)
}

func (d *Docker) ProbeEnabled(ctx context.Context, runtime domain.CapsuleRuntime, enabled domain.Enablement, request json.RawMessage) (json.RawMessage, error) {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return nil, errors.New("composition has no live Docker capsule")
	}
	if enabled.Transport != "stdio" {
		return nil, fmt.Errorf("enabled capability %s uses unsupported probe transport %q", enabled.Name, enabled.Transport)
	}
	if strings.TrimSpace(enabled.Command) == "" {
		return nil, fmt.Errorf("enabled capability %s has no command entrypoint", enabled.Name)
	}
	pidFile := "/tmp/spin-probe-" + strconv.FormatInt(time.Now().UnixNano(), 36) + ".pid"
	wrappedCommand, err := enabledLaunchCommand(enabled, pidFile)
	if err != nil {
		return nil, err
	}
	cmd := exec.CommandContext(ctx, d.binary,
		"exec", "-i", "-w", "/workspace", runtime.ContainerID,
		"sh", "-lc", wrappedCommand,
	)
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	defer func() {
		_ = stdin.Close()
		cleanupContext, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		defer cancel()
		cleanupCommand := "if read probe_pid < " + pidFile + "; then kill \"$probe_pid\" 2>/dev/null || true; fi; rm -f " + pidFile
		_, _, _ = d.run(cleanupContext, "exec", runtime.ContainerID, "sh", "-lc", cleanupCommand)
		if cmd.Process != nil {
			_ = cmd.Process.Kill()
		}
		_ = cmd.Wait()
	}()
	if _, err := stdin.Write(append(append([]byte{}, request...), '\n')); err != nil {
		return nil, err
	}

	scanner := bufio.NewScanner(stdout)
	scanner.Buffer(make([]byte, 4096), 4<<20)
	for scanner.Scan() {
		line := append([]byte{}, scanner.Bytes()...)
		if !json.Valid(line) {
			return nil, fmt.Errorf("%s wrote non-JSON data to ACP stdout: %q", enabled.Command, string(line))
		}
		var envelope struct {
			ID json.RawMessage `json:"id"`
		}
		if err := json.Unmarshal(line, &envelope); err != nil {
			return nil, err
		}
		if string(envelope.ID) == "0" {
			return json.RawMessage(line), nil
		}
	}
	if err := scanner.Err(); err != nil {
		return nil, err
	}
	return nil, fmt.Errorf("%s closed before its ACP response: %s", enabled.Command, strings.TrimSpace(stderr.String()))
}

func (d *Docker) StartEnabled(ctx context.Context, runtime domain.CapsuleRuntime, enabled domain.Enablement) (EnabledProcess, error) {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return nil, errors.New("composition has no live Docker capsule")
	}
	if enabled.Transport != "stdio" {
		return nil, fmt.Errorf("enabled capability %s uses unsupported streaming transport %q", enabled.Name, enabled.Transport)
	}
	if strings.TrimSpace(enabled.Command) == "" {
		return nil, fmt.Errorf("enabled capability %s has no command entrypoint", enabled.Name)
	}
	pidFile := "/tmp/spin-enabled-" + strconv.FormatInt(time.Now().UnixNano(), 36) + ".pid"
	wrappedCommand, err := enabledLaunchCommand(enabled, pidFile)
	if err != nil {
		return nil, err
	}
	cmd := exec.CommandContext(ctx, d.binary,
		"exec", "-i", "-w", "/workspace", runtime.ContainerID,
		"sh", "-lc", wrappedCommand,
	)
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		_ = stdin.Close()
		return nil, err
	}
	process := &dockerEnabledProcess{
		docker: d, cmd: cmd, stdin: stdin, stdout: stdout,
		containerID: runtime.ContainerID, pidFile: pidFile,
	}
	cmd.Stderr = &process.stderr
	if err := cmd.Start(); err != nil {
		_ = stdin.Close()
		_ = stdout.Close()
		return nil, err
	}
	return process, nil
}

// enabledLaunchCommand lets ordinary filesystem layers configure an opaque
// capability without teaching the engine about ACP, Codex or any future tool.
// The file is shell syntax because the layer itself is already trusted to
// supply the executable and the rest of the container filesystem.
func enabledLaunchCommand(enabled domain.Enablement, pidFile string) (string, error) {
	name := strings.TrimSpace(enabled.Name)
	if !validEnabledName(name) {
		return "", fmt.Errorf("invalid enabled capability name %q", enabled.Name)
	}
	environmentFile := "/etc/spin/enabled/" + name + ".env"
	// Agents that spawn shells (Claude Code refuses to start without one)
	// read SHELL, which docker exec leaves unset: point it at the best
	// shell the layer has unless the environment file already did.
	shell := `if [ -z "${SHELL:-}" ]; then for candidate in /bin/bash /usr/bin/bash /bin/zsh /bin/sh; do if [ -x "$candidate" ]; then export SHELL="$candidate"; break; fi; done; fi; `
	return "set -a; if [ -f " + environmentFile + " ]; then . " + environmentFile + "; fi; set +a; " + shell + "echo $$ > " + pidFile + "; exec " + enabled.Command, nil
}

func validEnabledName(value string) bool {
	if value == "" {
		return false
	}
	for _, char := range value {
		if (char < 'a' || char > 'z') && (char < '0' || char > '9') && char != '-' && char != '_' {
			return false
		}
	}
	return true
}

type dockerEnabledProcess struct {
	docker      *Docker
	cmd         *exec.Cmd
	stdin       io.WriteCloser
	stdout      io.ReadCloser
	containerID string
	pidFile     string
	stderr      bytes.Buffer
	closeOnce   sync.Once
	waitOnce    sync.Once
	execution   Execution
	waitErr     error
}

func (p *dockerEnabledProcess) Read(buffer []byte) (int, error) {
	return p.stdout.Read(buffer)
}

func (p *dockerEnabledProcess) Write(buffer []byte) (int, error) {
	return p.stdin.Write(buffer)
}

func (p *dockerEnabledProcess) Close() error {
	p.closeOnce.Do(func() {
		_ = p.stdin.Close()
		cleanupContext, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		defer cancel()
		cleanupCommand := "if read enabled_pid < " + p.pidFile + "; then kill \"$enabled_pid\" 2>/dev/null || true; fi; rm -f " + p.pidFile
		_, _, _ = p.docker.run(cleanupContext, "exec", p.containerID, "sh", "-lc", cleanupCommand)
		if p.cmd.Process != nil {
			_ = p.cmd.Process.Kill()
		}
		_ = p.stdout.Close()
	})
	return nil
}

func (p *dockerEnabledProcess) Wait() (Execution, error) {
	p.waitOnce.Do(func() {
		err := p.cmd.Wait()
		code := 0
		if err != nil {
			var exitErr *exec.ExitError
			if errors.As(err, &exitErr) {
				code = exitErr.ExitCode()
			} else {
				code = -1
				p.waitErr = err
			}
		}
		p.execution = Execution{Output: strings.TrimSpace(p.stderr.String()), ExitCode: code}
	})
	return p.execution, p.waitErr
}

func (d *Docker) InspectWorkspace(ctx context.Context, runtime domain.CapsuleRuntime) (WorkspaceChanges, error) {
	return d.inspectWorkspace(ctx, runtime, "HEAD", "")
}

func (d *Docker) InjectWorkspaceAttachments(ctx context.Context, runtime domain.CapsuleRuntime, attachments []WorkspaceAttachment) error {
	if len(attachments) == 0 {
		return nil
	}
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return errors.New("composition has no live Docker capsule")
	}
	if output, code, err := d.run(ctx, "exec", runtime.ContainerID, "sh", "-c", "mkdir -p /spin/job-attachments && chmod 0755 /spin /spin/job-attachments"); err != nil || code != 0 {
		return fmt.Errorf("prepare Job attachment directory (exit %d): %s: %w", code, strings.TrimSpace(output), err)
	}
	for _, attachment := range attachments {
		if (strings.TrimSpace(attachment.SourcePath) == "" && attachment.Data == nil) || !strings.HasPrefix(attachment.TargetPath, "/spin/job-attachments/") || strings.Contains(strings.TrimPrefix(attachment.TargetPath, "/spin/job-attachments/"), "/") {
			return fmt.Errorf("invalid Job attachment target %q", attachment.TargetPath)
		}
		sourcePath := attachment.SourcePath
		if attachment.Data != nil {
			temporary, err := os.CreateTemp("", "spin-capsule-attachment-*")
			if err != nil {
				return err
			}
			sourcePath = temporary.Name()
			if _, err := temporary.Write(attachment.Data); err != nil {
				_ = temporary.Close()
				_ = os.Remove(sourcePath)
				return err
			}
			if err := temporary.Close(); err != nil {
				_ = os.Remove(sourcePath)
				return err
			}
			defer os.Remove(sourcePath)
		}
		if output, code, err := d.run(ctx, "cp", sourcePath, runtime.ContainerID+":"+attachment.TargetPath); err != nil || code != 0 {
			return fmt.Errorf("copy Job attachment %s (exit %d): %s: %w", attachment.TargetPath, code, strings.TrimSpace(output), err)
		}
		if output, code, err := d.run(ctx, "exec", runtime.ContainerID, "chmod", "0444", attachment.TargetPath); err != nil || code != 0 {
			return fmt.Errorf("protect Job attachment %s (exit %d): %s: %w", attachment.TargetPath, code, strings.TrimSpace(output), err)
		}
	}
	return nil
}

// validHomePath is a relative path without spaces or parent steps.
func validHomePath(path string) bool {
	if path == "" || len(path) > 200 || strings.HasPrefix(path, "/") || strings.ContainsAny(path, " \t\r\n'\"\\") {
		return false
	}
	for _, part := range strings.Split(path, "/") {
		if part == "" || part == "." || part == ".." {
			return false
		}
	}
	return true
}

// validTrackedPath is an absolute path without spaces, quotes or parent
// steps.
func validTrackedPath(path string) bool {
	if path == "" || len(path) > 300 || !strings.HasPrefix(path, "/") || strings.ContainsAny(path, " \t\r\n'\"\\") {
		return false
	}
	for _, part := range strings.Split(strings.TrimPrefix(path, "/"), "/") {
		if part == "" || part == "." || part == ".." {
			return false
		}
	}
	return true
}

// ReadTrackedFiles returns the tracked files that exist in the capsule,
// base64 over one exec so a handful of small files costs one round trip.
func (d *Docker) ReadTrackedFiles(ctx context.Context, runtime domain.CapsuleRuntime, paths []string) (map[string][]byte, error) {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return nil, errors.New("composition has no live Docker capsule")
	}
	var script strings.Builder
	for _, path := range paths {
		if !validTrackedPath(path) {
			return nil, fmt.Errorf("invalid tracked path %q", path)
		}
		fmt.Fprintf(&script, "if [ -f '%s' ] && [ \"$(wc -c < '%s')\" -le %d ]; then printf 'SPIN_FILE %s '; base64 < '%s' | tr -d '\\n'; printf '\\n'; fi\n", path, path, TrackedFileLimit, path, path)
	}
	output, code, err := d.run(ctx, "exec", runtime.ContainerID, "sh", "-c", script.String())
	if err != nil && code < 0 {
		return nil, err
	}
	return parseHomeFiles(output)
}

func parseHomeFiles(output string) (map[string][]byte, error) {
	files := map[string][]byte{}
	for _, line := range strings.Split(output, "\n") {
		fields := strings.Fields(line)
		if len(fields) < 2 || fields[0] != "SPIN_FILE" {
			continue
		}
		var data []byte
		if len(fields) == 3 {
			decoded, err := base64.StdEncoding.DecodeString(fields[2])
			if err != nil {
				return nil, fmt.Errorf("tracked file %s: %w", fields[1], err)
			}
			data = decoded
		}
		files[fields[1]] = data
	}
	return files, nil
}

// WriteTrackedFiles puts files in the capsule, readable by the owner only.
func (d *Docker) WriteTrackedFiles(ctx context.Context, runtime domain.CapsuleRuntime, files map[string][]byte) error {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return errors.New("composition has no live Docker capsule")
	}
	var input strings.Builder
	for path, data := range files {
		if !validTrackedPath(path) {
			return fmt.Errorf("invalid tracked path %q", path)
		}
		fmt.Fprintf(&input, "%s %s\n", path, base64.StdEncoding.EncodeToString(data))
	}
	script := `while IFS=' ' read -r path data; do
  [ -n "$path" ] || continue
  mkdir -p "$(dirname "$path")"
  printf '%s' "$data" | base64 -d > "$path.spin-tmp" && chmod 600 "$path.spin-tmp" && mv "$path.spin-tmp" "$path"
done`
	output, err := d.controlInput(ctx, []byte(input.String()), "exec", "-i", runtime.ContainerID, "sh", "-c", script)
	if err != nil {
		return fmt.Errorf("write tracked files: %s: %w", strings.TrimSpace(output), err)
	}
	return nil
}

func (d *Docker) InspectWorkspaceRange(ctx context.Context, runtime domain.CapsuleRuntime, comparison WorkspaceComparison) (WorkspaceChanges, error) {
	changes := WorkspaceChanges{Files: []WorkspaceFileChange{}}
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return changes, errors.New("composition has no live Docker capsule")
	}
	if !validGitRef(comparison.BaseRef) || !validGitRef(comparison.HeadRef) {
		return changes, fmt.Errorf("invalid Git comparison %q...%q", comparison.BaseRef, comparison.HeadRef)
	}
	comparison.CommitMessageMatch = strings.TrimSpace(comparison.CommitMessageMatch)
	if strings.ContainsAny(comparison.CommitMessageMatch, "\r\n\x00") || len(comparison.CommitMessageMatch) > 256 {
		return changes, errors.New("invalid Git commit match")
	}
	if comparison.MergeCommit != "" && !validCommitHash(comparison.MergeCommit) {
		return changes, errors.New("invalid Git merge commit")
	}
	authentication := comparison.Authentication
	if authentication == nil {
		authentication = &GitAuthentication{}
	}
	secretInput := []byte(strings.Join([]string{singleLine(authentication.Username), singleLine(authentication.Password)}, "\n") + "\n")
	directory, err := workspaceDirectory(comparison.Path)
	if err != nil {
		return changes, err
	}
	output, err := d.controlInput(ctx, secretInput,
		"exec", "-i", "-w", directory,
		"-e", "GIT_TERMINAL_PROMPT=0",
		"-e", "SPIN_COMPARE_BASE="+comparison.BaseRef,
		"-e", "SPIN_COMPARE_HEAD="+comparison.HeadRef,
		"-e", "SPIN_COMPARE_COMMIT_MATCH="+comparison.CommitMessageMatch,
		"-e", "SPIN_COMPARE_MERGE="+comparison.MergeCommit,
		runtime.ContainerID, "sh", "-lc", compareWorkspaceScript,
	)
	if err != nil {
		return changes, fmt.Errorf("prepare Job comparison: %w", err)
	}
	baseCommit, headCommit := parseCompareLine(output)
	if len(baseCommit) < 7 {
		return changes, errors.New("Git comparison did not resolve a base commit")
	}
	if comparison.CommitMessageMatch != "" && len(headCommit) < 7 {
		return changes, nil
	}
	return d.inspectWorkspaceAt(ctx, runtime, comparison.Path, baseCommit, headCommit)
}

// CompareRepository compares two branches on the runner's own clone of the
// repository: what the Job pushed against its base, or one Session's commit.
// Nothing of the composition is needed, only git and the remote.
func (d *Docker) CompareRepository(ctx context.Context, request RepositoryComparison) (WorkspaceChanges, error) {
	changes := WorkspaceChanges{Files: []WorkspaceFileChange{}}
	if strings.TrimSpace(request.RemoteURL) == "" || !validRemoteURL(request.RemoteURL) {
		return changes, errors.New("repository remote URL is required")
	}
	comparison := request.Comparison
	if !validGitRef(comparison.BaseRef) || !validGitRef(comparison.HeadRef) {
		return changes, fmt.Errorf("invalid Git comparison %q...%q", comparison.BaseRef, comparison.HeadRef)
	}
	comparison.CommitMessageMatch = strings.TrimSpace(comparison.CommitMessageMatch)
	if strings.ContainsAny(comparison.CommitMessageMatch, "\r\n\x00") || len(comparison.CommitMessageMatch) > 256 {
		return changes, errors.New("invalid Git commit match")
	}
	if comparison.MergeCommit != "" && !validCommitHash(comparison.MergeCommit) {
		return changes, errors.New("invalid Git merge commit")
	}
	key := strings.TrimSpace(request.CacheKey)
	if key == "" {
		sum := sha256.Sum256([]byte(request.RemoteURL))
		key = hex.EncodeToString(sum[:6])
	}
	authentication := comparison.Authentication
	if authentication == nil {
		authentication = &GitAuthentication{}
	}
	secretInput := []byte(strings.Join([]string{singleLine(authentication.Username), singleLine(authentication.Password)}, "\n") + "\n")
	// Its own volume, apart from Explore's blobless clone: a diff needs the
	// blobs, and git would not fetch again what it believes it has.
	volume := runtimeName("spin-compare", key)
	output, err := d.controlInput(ctx, secretInput,
		"run", "--rm", "-i",
		"--label", "spin.managed=true", "--label", "spin.kind=compare",
		"--mount", "type=volume,src="+volume+",dst=/repo",
		"-w", "/repo",
		"-e", "GIT_TERMINAL_PROMPT=0",
		"-e", "SPIN_GIT_REMOTE="+request.RemoteURL,
		"-e", "SPIN_COMPARE_BASE="+comparison.BaseRef,
		"-e", "SPIN_COMPARE_HEAD="+comparison.HeadRef,
		"-e", "SPIN_COMPARE_COMMIT_MATCH="+comparison.CommitMessageMatch,
		"-e", "SPIN_COMPARE_MERGE="+comparison.MergeCommit,
		"--entrypoint", "sh", browseImage, "-c", compareRepositoryScript,
	)
	if err != nil {
		return changes, fmt.Errorf("prepare Job comparison: %w", err)
	}
	baseCommit, headCommit := parseCompareLine(output)
	if len(baseCommit) < 7 {
		return changes, errors.New("Git comparison did not resolve a base commit")
	}
	if len(headCommit) < 7 {
		return changes, nil
	}
	return d.inspectChanges(ctx, d.volumeGit(volume), baseCommit, headCommit)
}

func parseCompareLine(output string) (base, head string) {
	for _, line := range strings.Split(output, "\n") {
		fields := strings.Fields(strings.TrimSpace(line))
		if len(fields) == 3 && fields[0] == "SPIN_COMPARE" {
			base = strings.TrimPrefix(fields[1], "base=")
			head = strings.TrimPrefix(fields[2], "head=")
		}
	}
	return base, head
}

const compareRepositoryScript = `set -e
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  ` + gitCredentialEnvironmentScript + `
fi
if [ ! -d .git ]; then
  git init -q
  git remote add origin "$SPIN_GIT_REMOTE"
else
  git remote set-url origin "$SPIN_GIT_REMOTE"
fi
git fetch -q --depth=256 origin "+refs/heads/$SPIN_COMPARE_BASE:refs/remotes/spin/base" "+refs/heads/$SPIN_COMPARE_HEAD:refs/remotes/spin/head"
# A merged Job: the base branch holds the Job now, so the merge itself is
# the comparison: its first parent against the merge commit.
if [ -n "$SPIN_COMPARE_MERGE" ]; then
  if ! git cat-file -e "$SPIN_COMPARE_MERGE^{commit}" 2>/dev/null; then
    git fetch -q --deepen=1024 origin "+refs/heads/$SPIN_COMPARE_BASE:refs/remotes/spin/base"
  fi
  SPIN_COMPARE_BASE_COMMIT="$(git rev-parse "$SPIN_COMPARE_MERGE^1")"
  SPIN_COMPARE_HEAD_COMMIT="$(git rev-parse "$SPIN_COMPARE_MERGE")"
  printf 'SPIN_COMPARE base=%s head=%s\n' "$SPIN_COMPARE_BASE_COMMIT" "$SPIN_COMPARE_HEAD_COMMIT"
  unset SPIN_GIT_PASSWORD
  exit 0
fi
SPIN_COMPARE_BASE_COMMIT="$(git merge-base refs/remotes/spin/base refs/remotes/spin/head || true)"
if [ -z "$SPIN_COMPARE_BASE_COMMIT" ]; then
  git fetch -q --deepen=1024 origin "+refs/heads/$SPIN_COMPARE_BASE:refs/remotes/spin/base" "+refs/heads/$SPIN_COMPARE_HEAD:refs/remotes/spin/head"
  SPIN_COMPARE_BASE_COMMIT="$(git merge-base refs/remotes/spin/base refs/remotes/spin/head || true)"
fi
test -n "$SPIN_COMPARE_BASE_COMMIT"
SPIN_COMPARE_HEAD_COMMIT="$(git rev-parse refs/remotes/spin/head)"
if [ -n "$SPIN_COMPARE_COMMIT_MATCH" ]; then
  SPIN_COMPARE_HEAD_COMMIT="$(git log refs/remotes/spin/head --fixed-strings --grep="$SPIN_COMPARE_COMMIT_MATCH" -1 --format=%H)"
  if [ -n "$SPIN_COMPARE_HEAD_COMMIT" ]; then
    SPIN_COMPARE_BASE_COMMIT="$(git rev-parse "$SPIN_COMPARE_HEAD_COMMIT^")"
  fi
fi
printf 'SPIN_COMPARE base=%s head=%s\n' "$SPIN_COMPARE_BASE_COMMIT" "$SPIN_COMPARE_HEAD_COMMIT"
unset SPIN_GIT_PASSWORD`

const compareWorkspaceScript = `set -eu
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  ` + gitCredentialEnvironmentScript + `
fi
git fetch -q --depth=256 origin "+refs/heads/$SPIN_COMPARE_BASE:refs/remotes/spin/base" "+refs/heads/$SPIN_COMPARE_HEAD:refs/remotes/spin/head"
# A merged Job: the base branch holds the Job now, so the merge itself is
# the comparison: its first parent against the merge commit.
if [ -n "$SPIN_COMPARE_MERGE" ]; then
  if ! git cat-file -e "$SPIN_COMPARE_MERGE^{commit}" 2>/dev/null; then
    git fetch -q --deepen=1024 origin "+refs/heads/$SPIN_COMPARE_BASE:refs/remotes/spin/base"
  fi
  SPIN_COMPARE_BASE_COMMIT="$(git rev-parse "$SPIN_COMPARE_MERGE^1")"
  SPIN_COMPARE_HEAD_COMMIT="$(git rev-parse "$SPIN_COMPARE_MERGE")"
  printf 'SPIN_COMPARE base=%s head=%s\n' "$SPIN_COMPARE_BASE_COMMIT" "$SPIN_COMPARE_HEAD_COMMIT"
  unset SPIN_GIT_PASSWORD
  exit 0
fi
SPIN_COMPARE_BASE_COMMIT="$(git merge-base refs/remotes/spin/base HEAD || true)"
if [ -z "$SPIN_COMPARE_BASE_COMMIT" ]; then
  git fetch -q --deepen=1024 origin "+refs/heads/$SPIN_COMPARE_BASE:refs/remotes/spin/base" "+refs/heads/$SPIN_COMPARE_HEAD:refs/remotes/spin/head"
  SPIN_COMPARE_BASE_COMMIT="$(git merge-base refs/remotes/spin/base HEAD || true)"
fi
test -n "$SPIN_COMPARE_BASE_COMMIT"
SPIN_COMPARE_HEAD_COMMIT=""
if [ -n "$SPIN_COMPARE_COMMIT_MATCH" ]; then
  SPIN_COMPARE_HEAD_COMMIT="$(git log refs/remotes/spin/head --fixed-strings --grep="$SPIN_COMPARE_COMMIT_MATCH" -1 --format=%H)"
  if [ -n "$SPIN_COMPARE_HEAD_COMMIT" ]; then
    SPIN_COMPARE_BASE_COMMIT="$(git rev-parse "$SPIN_COMPARE_HEAD_COMMIT^")"
  fi
fi
printf 'SPIN_COMPARE base=%s head=%s\n' "$SPIN_COMPARE_BASE_COMMIT" "$SPIN_COMPARE_HEAD_COMMIT"
unset SPIN_GIT_PASSWORD`

// gitRunner runs one command in a checkout: in a composition's capsule, or
// in a one-shot git container on a clone volume.
type gitRunner func(ctx context.Context, args ...string) (string, int, error)

func (d *Docker) containerGit(runtime domain.CapsuleRuntime, directory string) gitRunner {
	return func(ctx context.Context, args ...string) (string, int, error) {
		return d.run(ctx, append([]string{"exec", "-w", directory, runtime.ContainerID}, args...)...)
	}
}

func (d *Docker) volumeGit(volume string) gitRunner {
	return func(ctx context.Context, args ...string) (string, int, error) {
		return d.run(ctx, append([]string{
			"run", "--rm", "--label", "spin.managed=true", "--label", "spin.kind=compare",
			"--mount", "type=volume,src=" + volume + ",dst=/repo", "-w", "/repo",
			"--entrypoint", args[0], browseImage,
		}, args[1:]...)...)
	}
}

func (d *Docker) inspectWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, diffBase, diffHead string) (WorkspaceChanges, error) {
	return d.inspectWorkspaceAt(ctx, runtime, "", diffBase, diffHead)
}

func (d *Docker) inspectWorkspaceAt(ctx context.Context, runtime domain.CapsuleRuntime, path, diffBase, diffHead string) (WorkspaceChanges, error) {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return WorkspaceChanges{Files: []WorkspaceFileChange{}}, errors.New("composition has no live Docker capsule")
	}
	directory, err := workspaceDirectory(path)
	if err != nil {
		return WorkspaceChanges{Files: []WorkspaceFileChange{}}, err
	}
	return d.inspectChanges(ctx, d.containerGit(runtime, directory), diffBase, diffHead)
}

// InspectWorkspaceAt is InspectWorkspace for one repository of a capsule
// with several.
func (d *Docker) InspectWorkspaceAt(ctx context.Context, runtime domain.CapsuleRuntime, path string) (WorkspaceChanges, error) {
	return d.inspectWorkspaceAt(ctx, runtime, path, "HEAD", "")
}

func (d *Docker) inspectChanges(ctx context.Context, git gitRunner, diffBase, diffHead string) (WorkspaceChanges, error) {
	const maxPatchBytes = 512 << 10
	const maxTotalPatchBytes = 2 << 20
	changes := WorkspaceChanges{Files: []WorkspaceFileChange{}}
	branch, code, err := git(ctx, "git", "branch", "--show-current")
	if err != nil && code < 0 {
		return changes, err
	}
	changes.Branch = strings.TrimSpace(branch)
	statusOutput := ""
	if diffHead == "" {
		var statusCode int
		var statusErr error
		statusOutput, statusCode, statusErr = git(ctx, "git", "status", "--porcelain=v1", "--untracked-files=all", "-z")
		if statusErr != nil && statusCode != 0 {
			return changes, fmt.Errorf("git status failed (exit %d): %s", statusCode, strings.TrimSpace(statusOutput))
		}
	}
	byPath := map[string]int{}
	if diffBase != "HEAD" || diffHead != "" {
		// A range names its files with what happened to them: A, M, D, or a
		// rename with both names. That is what the list shows.
		nameArgs := []string{"git", "diff", "--name-status", "-z", diffBase}
		if diffHead != "" {
			nameArgs = append(nameArgs, diffHead)
		}
		nameOutput, nameCode, nameErr := git(ctx, nameArgs...)
		if nameErr != nil && nameCode != 0 {
			return changes, fmt.Errorf("git range names failed (exit %d): %s", nameCode, strings.TrimSpace(nameOutput))
		}
		fields := strings.Split(nameOutput, "\x00")
		for index := 0; index+1 < len(fields); index += 2 {
			status, path := fields[index], fields[index+1]
			if status == "" || path == "" {
				continue
			}
			if (strings.HasPrefix(status, "R") || strings.HasPrefix(status, "C")) && index+2 < len(fields) {
				index++
				path = fields[index+1]
			}
			byPath[path] = len(changes.Files)
			changes.Files = append(changes.Files, WorkspaceFileChange{Path: path, Status: status[:1] + " "})
		}
	}
	statusFields := strings.Split(statusOutput, "\x00")
	for index := 0; index < len(statusFields); index++ {
		field := statusFields[index]
		if len(field) < 4 {
			continue
		}
		status, path := field[:2], field[3:]
		if (strings.HasPrefix(status, "R") || strings.HasPrefix(status, "C")) && index+1 < len(statusFields) {
			index++
		}
		if position, exists := byPath[path]; exists {
			changes.Files[position].Status = status
			continue
		}
		byPath[path] = len(changes.Files)
		changes.Files = append(changes.Files, WorkspaceFileChange{Path: path, Status: status})
	}
	numstatArgs := []string{"git", "diff", "--numstat", diffBase}
	if diffHead != "" {
		numstatArgs = append(numstatArgs, diffHead)
	}
	diffOutput, _, _ := git(ctx, numstatArgs...)
	for _, line := range strings.Split(diffOutput, "\n") {
		parts := strings.SplitN(line, "\t", 3)
		if len(parts) != 3 {
			continue
		}
		added, _ := strconv.Atoi(parts[0])
		deleted, _ := strconv.Atoi(parts[1])
		path := parts[2]
		position, ok := byPath[path]
		if !ok {
			position = len(changes.Files)
			byPath[path] = position
			changes.Files = append(changes.Files, WorkspaceFileChange{Path: path, Status: "M "})
		}
		changes.Files[position].Added = added
		changes.Files[position].Deleted = deleted
		changes.Added += added
		changes.Deleted += deleted
	}
	for position := range changes.Files {
		file := &changes.Files[position]
		if file.Status != "??" || file.Added != 0 || file.Deleted != 0 {
			continue
		}
		lineOutput, lineCode, _ := git(ctx, "wc", "-l", "--", file.Path)
		if lineCode != 0 {
			continue
		}
		fields := strings.Fields(lineOutput)
		if len(fields) == 0 {
			continue
		}
		file.Added, _ = strconv.Atoi(fields[0])
		changes.Added += file.Added
	}
	remainingPatchBytes := maxTotalPatchBytes
	for position := range changes.Files {
		file := &changes.Files[position]
		if remainingPatchBytes == 0 {
			file.Truncated = true
			continue
		}
		var patch string
		if file.Status == "??" {
			output, exitCode, _ := git(ctx, "git", "diff", "--no-index", "--no-ext-diff", "--no-textconv", "--no-color", "--unified=3", "--", "/dev/null", file.Path)
			if exitCode == 0 || exitCode == 1 {
				patch = output
			}
		} else {
			patchArgs := []string{"git", "diff", "--no-ext-diff", "--no-textconv", "--no-color", "--unified=3", diffBase}
			if diffHead != "" {
				patchArgs = append(patchArgs, diffHead)
			}
			patchArgs = append(patchArgs, "--", file.Path)
			output, exitCode, _ := git(ctx, patchArgs...)
			if exitCode == 0 {
				patch = output
			}
		}
		file.Binary = strings.Contains(patch, "Binary files ") || strings.Contains(patch, "GIT binary patch")
		limit := maxPatchBytes
		if remainingPatchBytes < limit {
			limit = remainingPatchBytes
		}
		if len(patch) > limit {
			patch = patch[:limit]
			if lastLine := strings.LastIndexByte(patch, '\n'); lastLine >= 0 {
				patch = patch[:lastLine+1]
			}
			file.Truncated = true
		}
		file.Patch = patch
		remainingPatchBytes -= len(patch)
	}
	return changes, nil
}

func (d *Docker) AcceptWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, acceptance WorkspaceAcceptance) (WorkspaceAcceptanceResult, error) {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return WorkspaceAcceptanceResult{}, errors.New("composition has no live Docker capsule")
	}
	acceptance.CommitSubject = strings.TrimSpace(acceptance.CommitSubject)
	acceptance.CommitBody = strings.TrimSpace(acceptance.CommitBody)
	if acceptance.CommitSubject == "" || len(acceptance.CommitSubject) > 200 || len(acceptance.CommitBody) > 4000 {
		return WorkspaceAcceptanceResult{}, errors.New("accept commit subject must contain 1 to 200 characters and body at most 4000 characters")
	}
	if !validGitRef(acceptance.RemoteRef) {
		return WorkspaceAcceptanceResult{}, fmt.Errorf("invalid remote Git ref %q", acceptance.RemoteRef)
	}
	authentication := acceptance.Authentication
	if authentication == nil {
		authentication = &GitAuthentication{}
	}
	authorName := strings.TrimSpace(authentication.AuthorName)
	if authorName == "" {
		authorName = "Spin Agent"
	}
	authorEmail := strings.TrimSpace(authentication.AuthorEmail)
	if authorEmail == "" {
		authorEmail = "spin@local.invalid"
	}
	secretInput := []byte(strings.Join([]string{
		singleLine(authentication.Username),
		singleLine(authentication.Password),
		singleLine(authorName),
		singleLine(authorEmail),
	}, "\n") + "\n")
	allowChanges := "0"
	if acceptance.AllowChanges {
		allowChanges = "1"
	}
	directory, err := workspaceDirectory(acceptance.Path)
	if err != nil {
		return WorkspaceAcceptanceResult{}, err
	}
	output, err := d.controlInput(ctx, secretInput,
		"exec", "-i", "-w", directory,
		"-e", "GIT_TERMINAL_PROMPT=0",
		"-e", "SPIN_ALLOW_CHANGES="+allowChanges,
		"-e", "SPIN_GIT_REF="+acceptance.RemoteRef,
		"-e", "SPIN_COMMIT_SUBJECT="+acceptance.CommitSubject,
		"-e", "SPIN_COMMIT_BODY="+acceptance.CommitBody,
		runtime.ContainerID, "sh", "-lc", acceptWorkspaceScript,
	)
	if err != nil {
		return WorkspaceAcceptanceResult{}, err
	}
	for _, line := range strings.Split(output, "\n") {
		fields := strings.Fields(strings.TrimSpace(line))
		if len(fields) != 3 || fields[0] != "SPIN_ACCEPT" {
			continue
		}
		committed := strings.TrimPrefix(fields[1], "committed=") == "1"
		head := strings.TrimPrefix(fields[2], "head=")
		if head != fields[2] && len(head) >= 7 {
			return WorkspaceAcceptanceResult{Head: head, Committed: committed}, nil
		}
	}
	return WorkspaceAcceptanceResult{}, errors.New("workspace acceptance returned no result marker")
}

func singleLine(value string) string {
	value = strings.ReplaceAll(value, "\r", " ")
	return strings.ReplaceAll(value, "\n", " ")
}

const acceptWorkspaceScript = `set -eu
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
IFS= read -r SPIN_GIT_AUTHOR_NAME || true
IFS= read -r SPIN_GIT_AUTHOR_EMAIL || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  ` + gitCredentialEnvironmentScript + `
fi
SPIN_BASE_COMMIT="$(git config --get spin.baseCommit || true)"
if [ -z "$SPIN_BASE_COMMIT" ]; then
  SPIN_BASE_COMMIT="$(git reflog show --format=%H HEAD | tail -n 1)"
fi
if [ -z "$SPIN_BASE_COMMIT" ] || ! git cat-file -e "$SPIN_BASE_COMMIT^{commit}"; then
  echo 'Spin cannot determine the immutable Session base commit' >&2
  exit 41
fi
SPIN_HEAD="$(git rev-parse HEAD)"
SPIN_DIRTY="$(git status --porcelain=v1 --untracked-files=all)"
SPIN_CHANGED=0
if [ "$SPIN_HEAD" != "$SPIN_BASE_COMMIT" ] || [ -n "$SPIN_DIRTY" ]; then
  SPIN_CHANGED=1
fi
SPIN_COMMITTED=0
SPIN_PUBLISH="$SPIN_HEAD"
if [ "$SPIN_CHANGED" = 1 ] && [ "$SPIN_ALLOW_CHANGES" != 1 ]; then
  # A phase without write policy never integrates. The agent may restore,
  # build and experiment in this throwaway workspace; none of it goes along.
  # ACCEPT confirms the untouched base and leaves the workspace as it is.
  SPIN_PUBLISH="$SPIN_BASE_COMMIT"
elif [ "$SPIN_CHANGED" = 1 ]; then
  # Agent-created commits and dirty files are deliberately folded into one
  # control-plane commit so ACCEPT is the only integration boundary.
  git reset --soft "$SPIN_BASE_COMMIT"
  git add -A
  if ! git diff --cached --quiet; then
    git -c user.name="$SPIN_GIT_AUTHOR_NAME" -c user.email="$SPIN_GIT_AUTHOR_EMAIL" commit -m "$SPIN_COMMIT_SUBJECT" -m "$SPIN_COMMIT_BODY"
    SPIN_COMMITTED=1
  else
    git reset --mixed "$SPIN_BASE_COMMIT"
  fi
  SPIN_PUBLISH="$(git rev-parse HEAD)"
fi
if git ls-remote --exit-code origin "refs/heads/$SPIN_GIT_REF" >/dev/null 2>&1; then
  git fetch --depth=50 origin "$SPIN_GIT_REF"
  if ! git merge-base --is-ancestor FETCH_HEAD "$SPIN_PUBLISH"; then
    echo 'The Job branch advanced after this Session started; automatic ACCEPT cannot overwrite it' >&2
    exit 43
  fi
fi
git push origin "$SPIN_PUBLISH:refs/heads/$SPIN_GIT_REF"
SPIN_REMOTE_HEAD="$(git ls-remote --exit-code origin "refs/heads/$SPIN_GIT_REF" | cut -f1)"
if [ "$SPIN_REMOTE_HEAD" != "$SPIN_PUBLISH" ]; then
  echo 'Remote Job branch does not match the accepted Session HEAD after push' >&2
  exit 44
fi
# The Session's work-in-progress branch on the remote has served; the Job
# branch carries the result now.
SPIN_SESSION_REF="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
if [ -n "$SPIN_SESSION_REF" ] && [ "$SPIN_SESSION_REF" != "HEAD" ] && [ "$SPIN_SESSION_REF" != "$SPIN_GIT_REF" ]; then
  git push -q origin ":refs/heads/$SPIN_SESSION_REF" >/dev/null 2>&1 || true
fi
printf 'SPIN_ACCEPT committed=%s head=%s\n' "$SPIN_COMMITTED" "$SPIN_PUBLISH"
unset SPIN_GIT_PASSWORD`

// splitSizeHeader finds the "SPIN_SIZE n" line a read script prints before
// the content; git may have printed a warning before it.
func splitSizeHeader(output string) (header, content string, ok bool) {
	index := strings.Index(output, "SPIN_SIZE ")
	if index < 0 || (index > 0 && output[index-1] != '\n') {
		return "", "", false
	}
	header, content, ok = strings.Cut(output[index:], "\n")
	return header, content, ok
}

// validWorkspacePath keeps a browser path inside the workspace.
func validWorkspacePath(value string) bool {
	if value == "" || strings.HasPrefix(value, "/") || strings.HasPrefix(value, "-") || strings.ContainsAny(value, "\x00\r\n") {
		return false
	}
	for _, segment := range strings.Split(value, "/") {
		if segment == "" || segment == "." || segment == ".." {
			return false
		}
	}
	return true
}

// browseImage has git and nothing else; the runner pulls it once.
const browseImage = "alpine/git:latest"

// BrowseRepository reads a remote repository through a shallow clone kept
// in a runner volume named after the repository.
func (d *Docker) BrowseRepository(ctx context.Context, browse RepositoryBrowse) (RepositoryBrowseResult, error) {
	result := RepositoryBrowseResult{}
	if strings.TrimSpace(browse.RemoteURL) == "" || !validRemoteURL(browse.RemoteURL) {
		return result, errors.New("repository remote URL is required")
	}
	if browse.Mode != "refs" && browse.Mode != "tree" && browse.Mode != "file" {
		return result, fmt.Errorf("unknown browse mode %q", browse.Mode)
	}
	if browse.Mode != "refs" && !validGitRef(browse.Ref) {
		return result, fmt.Errorf("invalid Git ref %q", browse.Ref)
	}
	if browse.Mode == "file" && !validWorkspacePath(browse.Path) {
		return result, fmt.Errorf("invalid repository path %q", browse.Path)
	}
	authentication := browse.Authentication
	if authentication == nil {
		authentication = &GitAuthentication{}
	}
	secretInput := []byte(strings.Join([]string{singleLine(authentication.Username), singleLine(authentication.Password)}, "\n") + "\n")
	volume := runtimeName("spin-browse", browse.CacheKey)
	output, err := d.controlInput(ctx, secretInput,
		"run", "--rm", "-i",
		"--label", "spin.managed=true", "--label", "spin.kind=browse",
		"--mount", "type=volume,src="+volume+",dst=/repo",
		"-w", "/repo",
		"-e", "GIT_TERMINAL_PROMPT=0",
		"-e", "SPIN_GIT_REMOTE="+browse.RemoteURL,
		"-e", "SPIN_MODE="+browse.Mode,
		"-e", "SPIN_REF="+browse.Ref,
		"-e", "SPIN_PATH="+browse.Path,
		"-e", "SPIN_LIMIT="+strconv.Itoa(WorkspaceFileLimit),
		"--entrypoint", "sh", browseImage, "-c", browseRepositoryScript,
	)
	if err != nil {
		return result, err
	}
	switch browse.Mode {
	case "refs":
		for _, line := range strings.Split(output, "\n") {
			stamp, name, ok := strings.Cut(strings.TrimSpace(line), "\t")
			if !ok || name == "" || name == "HEAD" {
				continue
			}
			unix, _ := strconv.ParseInt(strings.TrimSpace(stamp), 10, 64)
			result.Refs = append(result.Refs, RepositoryRef{Name: name, CommittedAt: time.Unix(unix, 0).UTC()})
		}
	case "tree":
		tree := &WorkspaceTree{Ref: browse.Ref, Entries: []WorkspaceEntry{}}
		for _, line := range strings.Split(output, "\n") {
			size, path, ok := strings.Cut(line, "\t")
			if !ok || path == "" {
				continue
			}
			bytes, _ := strconv.ParseInt(strings.TrimSpace(size), 10, 64)
			tree.Entries = append(tree.Entries, WorkspaceEntry{Path: path, Size: bytes})
		}
		result.Tree = tree
	case "file":
		header, content, ok := splitSizeHeader(output)
		if !ok {
			return result, fmt.Errorf("repository file read did not report a size: %s", strings.TrimSpace(output))
		}
		file := &WorkspaceFile{Ref: browse.Ref, Path: browse.Path}
		file.Size, _ = strconv.ParseInt(strings.TrimSpace(strings.TrimPrefix(header, "SPIN_SIZE ")), 10, 64)
		file.Truncated = file.Size > int64(len(content))
		if strings.IndexByte(content, 0) >= 0 {
			file.Binary, content = true, ""
		}
		file.Content = content
		result.File = file
	}
	return result, nil
}

func validRemoteURL(value string) bool {
	return !strings.ContainsAny(value, "\x00\r\n ") && (strings.HasPrefix(value, "https://") || strings.HasPrefix(value, "http://") || strings.HasPrefix(value, "ssh://") || strings.Contains(value, "@"))
}

// browseRepositoryScript keeps one shallow clone per repository and reads
// refs, a tree or a file from it. Credentials arrive on stdin and live only
// in this process.
const browseRepositoryScript = `set -e
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  ` + gitCredentialEnvironmentScript + `
fi
if [ ! -d .git ]; then
  git init -q
  git remote add origin "$SPIN_GIT_REMOTE"
else
  git remote set-url origin "$SPIN_GIT_REMOTE"
fi
case "$SPIN_MODE" in
  refs)
    # Every branch tip, commits and trees only (no blobs), so the branches
    # can be ordered by their last commit; blobs come lazily when a file is
    # read. An old git without partial clone fetches the tips whole.
    git fetch -q --prune --depth=1 --filter=blob:none origin '+refs/heads/*:refs/remotes/origin/*' 2>/dev/null \
      || git fetch -q --prune --depth=1 origin '+refs/heads/*:refs/remotes/origin/*'
    git for-each-ref --sort=-committerdate --count=300 --format='%(committerdate:unix)%09%(refname:strip=3)' refs/remotes/origin
    ;;
  tree)
    git fetch -q --depth=1 origin "+refs/heads/${SPIN_REF}:refs/remotes/origin/${SPIN_REF}" 2>/dev/null
    git ls-tree -r -l "refs/remotes/origin/${SPIN_REF}" | while IFS= read -r line; do
      meta="${line%%	*}"; path="${line#*	}"; size="${meta##* }"
      printf '%s\t%s\n' "$size" "$path"
    done
    ;;
  file)
    git fetch -q --depth=1 origin "+refs/heads/${SPIN_REF}:refs/remotes/origin/${SPIN_REF}" 2>/dev/null
    printf 'SPIN_SIZE %s\n' "$(git cat-file -s "refs/remotes/origin/${SPIN_REF}:${SPIN_PATH}")"
    git show "refs/remotes/origin/${SPIN_REF}:${SPIN_PATH}" | head -c "$SPIN_LIMIT"
    ;;
esac
unset SPIN_GIT_PASSWORD`

// SyncWorkspace commits what is dirty as a WIP commit and pushes the
// Session branch to the remote, force: ACCEPT rewrites it into one commit.
func (d *Docker) SyncWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, sync WorkspaceSync) (WorkspaceSyncResult, error) {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return WorkspaceSyncResult{}, errors.New("composition has no live Docker capsule")
	}
	if !validGitRef(sync.SessionRef) {
		return WorkspaceSyncResult{}, fmt.Errorf("invalid Session ref %q", sync.SessionRef)
	}
	authentication := sync.Authentication
	if authentication == nil {
		authentication = &GitAuthentication{}
	}
	authorName := strings.TrimSpace(authentication.AuthorName)
	if authorName == "" {
		authorName = "Spin Agent"
	}
	authorEmail := strings.TrimSpace(authentication.AuthorEmail)
	if authorEmail == "" {
		authorEmail = "spin@local.invalid"
	}
	secretInput := []byte(strings.Join([]string{
		singleLine(authentication.Username),
		singleLine(authentication.Password),
		singleLine(authorName),
		singleLine(authorEmail),
	}, "\n") + "\n")
	directory, err := workspaceDirectory(sync.Path)
	if err != nil {
		return WorkspaceSyncResult{}, err
	}
	output, err := d.controlInput(ctx, secretInput,
		"exec", "-i", "-w", directory,
		"-e", "GIT_TERMINAL_PROMPT=0",
		"-e", "SPIN_SESSION_REF="+sync.SessionRef,
		runtime.ContainerID, "sh", "-lc", syncWorkspaceScript,
	)
	if err != nil {
		return WorkspaceSyncResult{}, err
	}
	for _, line := range strings.Split(output, "\n") {
		fields := strings.Fields(strings.TrimSpace(line))
		if len(fields) != 4 || fields[0] != "SPIN_SYNC" {
			continue
		}
		return WorkspaceSyncResult{
			Committed: strings.TrimPrefix(fields[1], "committed=") == "1",
			Pushed:    strings.TrimPrefix(fields[2], "pushed=") == "1",
			Head:      strings.TrimPrefix(fields[3], "head="),
		}, nil
	}
	return WorkspaceSyncResult{}, fmt.Errorf("workspace sync did not report a result: %s", strings.TrimSpace(output))
}

const syncWorkspaceScript = gitCredentialEnvironmentScript + `
set -e
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
IFS= read -r SPIN_GIT_AUTHOR_NAME || true
IFS= read -r SPIN_GIT_AUTHOR_EMAIL || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  export GIT_CONFIG_COUNT=1
fi
SPIN_COMMITTED=0
git add -A
if ! git diff --cached --quiet; then
  git -c user.name="$SPIN_GIT_AUTHOR_NAME" -c user.email="$SPIN_GIT_AUTHOR_EMAIL" commit -q -m "WIP $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  SPIN_COMMITTED=1
fi
SPIN_HEAD="$(git rev-parse HEAD)"
SPIN_PUSHED=0
SPIN_REMOTE_HEAD="$(git ls-remote origin "refs/heads/${SPIN_SESSION_REF}" | cut -f1)"
if [ "$SPIN_REMOTE_HEAD" != "$SPIN_HEAD" ]; then
  git push -q -f origin "$SPIN_HEAD:refs/heads/${SPIN_SESSION_REF}"
  SPIN_PUSHED=1
fi
printf 'SPIN_SYNC committed=%s pushed=%s head=%s\n' "$SPIN_COMMITTED" "$SPIN_PUSHED" "$SPIN_HEAD"
unset SPIN_GIT_PASSWORD`

// MergeWorkspace lands the Job: the Job branch merged into the base branch
// and pushed, verified by a fresh remote lookup. Fast-forward when possible,
// a merge commit otherwise; a base that cannot be merged cleanly fails and
// leaves the remote untouched.
func (d *Docker) MergeWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, merge WorkspaceMerge) (WorkspaceMergeResult, error) {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return WorkspaceMergeResult{}, errors.New("composition has no live Docker capsule")
	}
	if !validGitRef(merge.SourceRef) || !validGitRef(merge.TargetRef) {
		return WorkspaceMergeResult{}, fmt.Errorf("invalid Git refs %q → %q", merge.SourceRef, merge.TargetRef)
	}
	subject := strings.TrimSpace(merge.CommitSubject)
	if subject == "" || len(subject) > 200 || len(merge.CommitBody) > 4000 {
		return WorkspaceMergeResult{}, errors.New("merge commit subject must contain 1 to 200 characters and body at most 4000 characters")
	}
	authentication := merge.Authentication
	if authentication == nil {
		authentication = &GitAuthentication{}
	}
	authorName := strings.TrimSpace(authentication.AuthorName)
	if authorName == "" {
		authorName = "Spin"
	}
	authorEmail := strings.TrimSpace(authentication.AuthorEmail)
	if authorEmail == "" {
		authorEmail = "spin@local.invalid"
	}
	secretInput := []byte(strings.Join([]string{
		singleLine(authentication.Username),
		singleLine(authentication.Password),
		singleLine(authorName),
		singleLine(authorEmail),
	}, "\n") + "\n")
	directory, err := workspaceDirectory(merge.Path)
	if err != nil {
		return WorkspaceMergeResult{}, err
	}
	output, err := d.controlInput(ctx, secretInput,
		"exec", "-i", "-w", directory,
		"-e", "GIT_TERMINAL_PROMPT=0",
		"-e", "SPIN_MERGE_SOURCE="+merge.SourceRef,
		"-e", "SPIN_MERGE_TARGET="+merge.TargetRef,
		"-e", "SPIN_COMMIT_SUBJECT="+subject,
		"-e", "SPIN_COMMIT_BODY="+strings.TrimSpace(merge.CommitBody),
		runtime.ContainerID, "sh", "-lc", mergeWorkspaceScript,
	)
	if err != nil {
		return WorkspaceMergeResult{}, err
	}
	for _, line := range strings.Split(output, "\n") {
		fields := strings.Fields(strings.TrimSpace(line))
		if len(fields) != 2 || fields[0] != "SPIN_MERGE" {
			continue
		}
		return WorkspaceMergeResult{Head: strings.TrimPrefix(fields[1], "head=")}, nil
	}
	return WorkspaceMergeResult{}, fmt.Errorf("merge did not report a result: %s", strings.TrimSpace(output))
}

const mergeWorkspaceScript = gitCredentialEnvironmentScript + `
set -e
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
IFS= read -r SPIN_GIT_AUTHOR_NAME || true
IFS= read -r SPIN_GIT_AUTHOR_EMAIL || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  export GIT_CONFIG_COUNT=1
fi
git fetch -q --depth=200 origin "+refs/heads/${SPIN_MERGE_TARGET}:refs/remotes/origin/${SPIN_MERGE_TARGET}"
git fetch -q --depth=200 origin "+refs/heads/${SPIN_MERGE_SOURCE}:refs/remotes/origin/${SPIN_MERGE_SOURCE}"
SPIN_SOURCE="$(git rev-parse "refs/remotes/origin/${SPIN_MERGE_SOURCE}")"
SPIN_TARGET="$(git rev-parse "refs/remotes/origin/${SPIN_MERGE_TARGET}")"
git checkout -q -B spin-merge "$SPIN_TARGET"
# Always a merge commit, never a fast-forward: the base branch then shows
# one commit per Job, with the Job's own commits visible underneath it.
if ! git -c user.name="$SPIN_GIT_AUTHOR_NAME" -c user.email="$SPIN_GIT_AUTHOR_EMAIL" merge -q --no-ff -m "$SPIN_COMMIT_SUBJECT" -m "$SPIN_COMMIT_BODY" "$SPIN_SOURCE" >/dev/null 2>&1; then
  SPIN_CONFLICTS="$(git diff --name-only --diff-filter=U | tr '\n' ' ' | sed 's/ $//')"
  git merge --abort >/dev/null 2>&1 || true
  echo "SPIN_CONFLICT De Job-branch conflicteert met ${SPIN_MERGE_TARGET} in: ${SPIN_CONFLICTS}. Merge origin/${SPIN_MERGE_TARGET} in de Job-branch (die staat al opgehaald, niet fetchen), los de conflicten op en accept; Spin neemt het resultaat op in de Job-branch en de merge in ${SPIN_MERGE_TARGET} kan daarna opnieuw." >&2
  exit 45
fi
SPIN_HEAD="$(git rev-parse HEAD)"
git push origin "$SPIN_HEAD:refs/heads/${SPIN_MERGE_TARGET}"
# ls-remote matches a bare name against the tail of every ref: "main" would
# also list jobs/<ticket>/main. The full ref name is exact.
SPIN_REMOTE_HEAD="$(git ls-remote --exit-code origin "refs/heads/${SPIN_MERGE_TARGET}" | cut -f1)"
if [ "$SPIN_REMOTE_HEAD" != "$SPIN_HEAD" ]; then
  echo "Remote ${SPIN_MERGE_TARGET} does not match the merged HEAD after push" >&2
  exit 46
fi
printf 'SPIN_MERGE head=%s\n' "$SPIN_HEAD"
unset SPIN_GIT_PASSWORD`

func validGitRef(value string) bool {
	if value == "" || strings.HasPrefix(value, "/") || strings.HasSuffix(value, "/") || strings.Contains(value, "..") {
		return false
	}
	for _, char := range value {
		// # is a valid ref character and Job branches carry ticket numbers
		// (jobs/#1234/main); refs reach the scripts quoted, never bare.
		if (char < 'a' || char > 'z') && (char < 'A' || char > 'Z') && (char < '0' || char > '9') && !strings.ContainsRune("/_-.#", char) {
			return false
		}
	}
	return true
}

// findDockerBinary resolves the Docker CLI: PATH first, then the places
// Docker Desktop and package managers put it, because a runner started by
// a supervisor (launchd, a HOP job) often has a bare PATH.
func findDockerBinary() string {
	if path, err := exec.LookPath("docker"); err == nil {
		return path
	}
	candidates := []string{
		"/usr/local/bin/docker", "/opt/homebrew/bin/docker",
		"/Applications/Docker.app/Contents/Resources/bin/docker",
		"/usr/bin/docker", "/snap/bin/docker",
	}
	if home, err := os.UserHomeDir(); err == nil && home != "" {
		candidates = append(candidates, home+"/.docker/bin/docker", home+"/.rd/bin/docker")
	}
	for _, candidate := range candidates {
		if info, err := os.Stat(candidate); err == nil && !info.IsDir() && info.Mode()&0o111 != 0 {
			return candidate
		}
	}
	return "docker"
}

func (d *Docker) removeContainer(ctx context.Context, id string) error {
	output, code, err := d.run(ctx, "rm", "-f", id)
	if code == 0 || strings.Contains(output, "No such container") {
		return nil
	}
	return fmt.Errorf("docker rm failed (exit %d): %s: %w", code, strings.TrimSpace(output), err)
}

func (d *Docker) containerID(ctx context.Context, name string) (string, error) {
	id, err := d.control(ctx, "container", "inspect", "--format", "{{.Id}}", name)
	return strings.TrimSpace(id), err
}

// materializationArtifact is the image a composition runs from: the base
// layer itself when nothing lies above it, otherwise an image built by
// applying the layers above the base in stack order.
func (d *Docker) materializationArtifact(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact) (domain.Artifact, bool, error) {
	plan, err := PlanLayers(composition, artifacts)
	if err != nil {
		return domain.Artifact{}, false, err
	}
	if len(plan.Steps) == 0 {
		return plan.Base, false, nil
	}
	ref, err := d.buildComposition(ctx, composition, plan)
	if err != nil {
		return domain.Artifact{}, false, err
	}
	return domain.Artifact{
		ID:   "composition:" + composition.ID,
		Kind: domain.ArtifactKind("composition"), Name: composition.ID,
		Snapshot: domain.CapsuleSnapshot{Driver: "docker", Ref: ref, Restorable: true},
	}, true, nil
}

func (d *Docker) buildComposition(ctx context.Context, composition domain.Composition, plan LayerPlan) (string, error) {
	targetName := runtimeName("spin-compose-build", composition.ID)
	imageRef := "spin/composition:" + safeName(composition.ID)
	if d.logger != nil {
		summary := []string{plan.Base.ID + "=base"}
		for _, step := range plan.Steps {
			action := "diff"
			if step.Full {
				action = "full copy"
			}
			summary = append(summary, step.Artifact.ID+"="+action)
		}
		d.logger.Info("compose plan", "composition", composition.ID, "layers", strings.Join(summary, " "))
	}
	// The build container runs, so deletions of a layer diff can be applied
	// inside it before its files are copied in.
	if _, err := d.control(ctx,
		"run", "-d", "--name", targetName,
		"--label", "spin.managed=true",
		"--label", "spin.kind=composition-build",
		"--label", "spin.composition_id="+composition.ID,
		"--network", "none",
		"--entrypoint", "sh", plan.Base.Snapshot.Ref, "-lc", "trap 'exit 0' TERM INT; while :; do sleep 3600; done",
	); err != nil {
		return "", fmt.Errorf("create composition base from %s: %w", plan.Base.ID, err)
	}
	defer func() { _ = d.removeContainer(context.Background(), targetName) }()

	for index, step := range plan.Steps {
		if !step.Full {
			if err := d.applyLayerDiff(ctx, targetName, step.Artifact); err != nil {
				return "", err
			}
			continue
		}
		sourceName := runtimeName("spin-compose-source", composition.ID+"-"+strconv.Itoa(index+1))
		if err := d.mergeSnapshot(ctx, targetName, sourceName, step.Artifact); err != nil {
			return "", err
		}
	}
	if _, err := d.control(ctx,
		"commit", "--pause=true",
		"--change", "LABEL spin.managed=true",
		"--change", "LABEL spin.kind=composition-image",
		"--change", "LABEL spin.composition_id="+composition.ID,
		targetName, imageRef,
	); err != nil {
		return "", fmt.Errorf("commit composition image: %w", err)
	}
	return imageRef, nil
}

func (d *Docker) mergeSnapshot(ctx context.Context, targetName, sourceName string, layer domain.Artifact) error {
	if _, err := d.control(ctx,
		"create", "--name", sourceName,
		"--label", "spin.managed=true",
		"--label", "spin.kind=composition-source",
		"--entrypoint", "sh", layer.Snapshot.Ref, "-lc", "exit 0",
	); err != nil {
		return fmt.Errorf("create composition source from %s: %w", layer.ID, err)
	}
	defer func() { _ = d.removeContainer(context.Background(), sourceName) }()
	if err := d.copyContainerRoot(ctx, sourceName, targetName); err != nil {
		return fmt.Errorf("merge snapshot %s: %w", layer.ID, err)
	}
	return nil
}

func (d *Docker) copyContainerRoot(ctx context.Context, sourceName, targetName string) error {
	exporter := exec.CommandContext(ctx, d.binary, "export", sourceName)
	stream, err := exporter.StdoutPipe()
	if err != nil {
		return err
	}
	var exportError bytes.Buffer
	exporter.Stderr = &exportError
	copier := exec.CommandContext(ctx, d.binary, "cp", "-", targetName+":/")
	filtered, err := copier.StdinPipe()
	if err != nil {
		return err
	}
	var copyError bytes.Buffer
	copier.Stderr = &copyError
	if err := copier.Start(); err != nil {
		return fmt.Errorf("start docker cp: %w", err)
	}
	if err := exporter.Start(); err != nil {
		_ = filtered.Close()
		_ = copier.Wait()
		return fmt.Errorf("start docker export: %w", err)
	}
	// The export passes through this process: kernel and Docker-managed
	// paths are dropped, and a copy that fails does not leave the export
	// blocked on a pipe nobody reads.
	filterErr := filterExport(stream, filtered)
	_ = filtered.Close()
	copyErr := copier.Wait()
	if filterErr != nil || copyErr != nil {
		_ = exporter.Process.Kill()
	}
	exportErr := exporter.Wait()
	if copyErr != nil {
		return fmt.Errorf("docker cp: %s: %w", strings.TrimSpace(copyError.String()), copyErr)
	}
	if filterErr != nil {
		return fmt.Errorf("filter export of %s: %w", sourceName, filterErr)
	}
	if exportErr != nil {
		return fmt.Errorf("docker export: %s: %w", strings.TrimSpace(exportError.String()), exportErr)
	}
	return nil
}

func (d *Docker) control(ctx context.Context, args ...string) (string, error) {
	output, code, err := d.run(ctx, args...)
	if err != nil || code != 0 {
		return output, fmt.Errorf("docker %s failed (exit %d): %s: %w", args[0], code, strings.TrimSpace(output), err)
	}
	return output, nil
}

func (d *Docker) controlInput(ctx context.Context, input []byte, args ...string) (string, error) {
	cmd := exec.CommandContext(ctx, d.binary, args...)
	cmd.Stdin = bytes.NewReader(input)
	output, err := cmd.CombinedOutput()
	if err != nil {
		code := -1
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			code = exitErr.ExitCode()
		}
		return string(output), fmt.Errorf("docker %s failed (exit %d): %s: %w", args[0], code, strings.TrimSpace(string(output)), err)
	}
	return string(output), nil
}

func (d *Docker) run(ctx context.Context, args ...string) (string, int, error) {
	cmd := exec.CommandContext(ctx, d.binary, args...)
	output, err := cmd.CombinedOutput()
	if err == nil {
		return string(output), 0, nil
	}
	var exitErr *exec.ExitError
	if errors.As(err, &exitErr) {
		return string(output), exitErr.ExitCode(), err
	}
	return string(output), -1, err
}

func runtimeName(prefix, id string) string {
	value := safeName(id)
	if len(value) > 32 {
		value = value[:32]
	}
	return prefix + "-" + value
}

func safeName(value string) string {
	var b strings.Builder
	for _, r := range strings.ToLower(value) {
		if r >= 'a' && r <= 'z' || r >= '0' && r <= '9' || r == '_' || r == '-' || r == '.' {
			b.WriteRune(r)
		} else {
			b.WriteByte('-')
		}
	}
	if b.Len() == 0 {
		return strconv.FormatInt(0, 10)
	}
	return b.String()
}

// BundleWorkspace streams a folder or a file of the capsule as a tar: a
// folder relative to itself, a file on its own. Paths are the deliverable
// paths a person or an agent names, so they are checked like tracked ones.
func (d *Docker) BundleWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, path string, sink io.Writer) error {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return errors.New("composition has no live Docker capsule")
	}
	if !validTrackedPath(path) {
		return fmt.Errorf("invalid bundle path %q", path)
	}
	script := `p='` + path + `'
if [ -d "$p" ]; then cd "$p" && tar -cf - .
elif [ -f "$p" ]; then cd "$(dirname "$p")" && tar -cf - "$(basename "$p")"
else echo "$p: no such file or directory" >&2; exit 44
fi`
	cmd := exec.CommandContext(ctx, d.binary, "exec", runtime.ContainerID, "sh", "-c", script)
	var stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = sink, &stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("bundle %s: %s: %w", path, strings.TrimSpace(stderr.String()), err)
	}
	return nil
}

// PlaceBundle unpacks a tar stream at target: a folder target is emptied
// first and receives the stream's tree; a file target receives the stream's
// single file under its own name.
func (d *Docker) PlaceBundle(ctx context.Context, runtime domain.CapsuleRuntime, target string, archive io.Reader) error {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return errors.New("composition has no live Docker capsule")
	}
	if !validTrackedPath(target) {
		return fmt.Errorf("invalid bundle target %q", target)
	}
	script := `t='` + target + `'
rm -rf "$t" && mkdir -p "$(dirname "$t")" && mkdir -p "$t" && tar -C "$t" -xf -`
	cmd := exec.CommandContext(ctx, d.binary, "exec", "-i", runtime.ContainerID, "sh", "-c", script)
	var stderr bytes.Buffer
	cmd.Stdin, cmd.Stderr = archive, &stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("place bundle at %s: %s: %w", target, strings.TrimSpace(stderr.String()), err)
	}
	return nil
}

// validCommitHash is a full or abbreviated hex commit id.
func validCommitHash(value string) bool {
	if len(value) < 7 || len(value) > 64 {
		return false
	}
	for _, r := range value {
		if (r < '0' || r > '9') && (r < 'a' || r > 'f') && (r < 'A' || r > 'F') {
			return false
		}
	}
	return true
}
