//go:build !tamago

package capsule

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"slices"
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
}

type Docker struct {
	binary    string
	baseImage string
	network   string
	info      domain.CapsuleEngineInfo
}

func NewDocker(ctx context.Context, cfg DockerConfig) (*Docker, error) {
	if cfg.Binary == "" {
		cfg.Binary = "docker"
	}
	if cfg.BaseImage == "" {
		cfg.BaseImage = "alpine:3.24"
	}
	if cfg.Network == "" {
		cfg.Network = "bridge"
	}
	d := &Docker{binary: cfg.Binary, baseImage: cfg.BaseImage, network: cfg.Network}
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
	_, err := d.control(ctx,
		"run", "-d", "--pull=missing", "--init", "--name", name,
		"--label", "spin.managed=true",
		"--label", "spin.kind=recording",
		"--label", "spin.recording_id="+recording.ID,
		"--network", d.network,
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
		"exec", "-it", "-w", "/workspace", recording.Runtime.ContainerID,
		"sh", "-lc", input,
	)
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
	_, err := d.control(ctx,
		"commit", "--pause=true",
		"--change", "LABEL spin.managed=true",
		"--change", "LABEL spin.recording_id="+recording.ID,
		recording.Runtime.ContainerID, tag,
	)
	if err != nil {
		return domain.CapsuleSnapshot{}, err
	}
	digest, err := d.control(ctx, "image", "inspect", "--format", "{{.Id}}", tag)
	if err != nil {
		return domain.CapsuleSnapshot{}, err
	}
	// The immutable image is already safe when cleanup fails, so leave any
	// stubborn container discoverable through its spin.* labels.
	_, _, _ = d.run(ctx, "rm", "-f", recording.Runtime.ContainerID)
	return domain.CapsuleSnapshot{
		Driver:               "docker",
		Ref:                  tag,
		Digest:               strings.TrimSpace(digest),
		Restorable:           true,
		IncludesProcessState: false,
	}, nil
}

func (d *Docker) Cancel(ctx context.Context, recording domain.Recording) error {
	if recording.Runtime == nil || recording.Runtime.ContainerID == "" {
		return nil
	}
	return d.removeContainer(ctx, recording.Runtime.ContainerID)
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
	if composition.Git != nil {
		workspaceRef, err = d.prepareGitWorkspace(ctx, composition, selected, authentication)
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

func (d *Docker) prepareGitWorkspace(ctx context.Context, composition domain.Composition, selected domain.Artifact, authentication *GitAuthentication) (string, error) {
	workspace := composition.Git
	if workspace == nil || composition.SessionID == "" {
		return "", errors.New("Git workspace requires a Session")
	}
	if selected.Snapshot.Driver != "docker" || !selected.Snapshot.Restorable || selected.Snapshot.Ref == "" {
		return "", fmt.Errorf("Git helper artifact %s is not a restorable Docker snapshot", selected.ID)
	}
	requiresAuthentication := workspace.AccountID != "" || workspace.CredentialScope == domain.CredentialScopeUser || workspace.CredentialScope == domain.CredentialScopeGlobal
	if requiresAuthentication && (authentication == nil || authentication.Password == "") {
		return "", errors.New("Git account is bound but no checkout authentication was supplied")
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
	secretInput := []byte("\n\n\n\n")
	if authentication != nil {
		secretInput = []byte(strings.Join([]string{
			authentication.Username,
			authentication.Password,
			authentication.AuthorName,
			authentication.AuthorEmail,
		}, "\n") + "\n")
	}
	_, err := d.controlInput(ctx, secretInput,
		"run", "-i", "--rm", "--read-only", "--tmpfs", "/tmp:rw,nosuid,nodev,size=1m",
		"--label", "spin.managed=true",
		"--label", "spin.kind=git-checkout",
		"--network", d.network,
		"--mount", "type=volume,src="+volume+",dst=/workspace",
		"--workdir", "/workspace",
		"--env", "GIT_TERMINAL_PROMPT=0",
		"--env", "SPIN_GIT_REMOTE="+workspace.RemoteURL,
		"--env", "SPIN_GIT_BASE="+workspace.BaseRef,
		"--env", "SPIN_GIT_BOOTSTRAP="+workspace.BootstrapRef,
		"--env", "SPIN_GIT_HEAD="+workspace.HeadRef,
		"--env", "SPIN_GIT_TARGET="+workspace.TargetRef,
		"--entrypoint", "sh",
		selected.Snapshot.Ref, "-lc", gitWorkspaceScript,
	)
	if err != nil {
		return "", err
	}
	return volume, nil
}

const gitCredentialEnvironmentScript = `export SPIN_GIT_USERNAME SPIN_GIT_PASSWORD
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=credential.helper
export GIT_CONFIG_VALUE_0='!f() { printf "username=%s\npassword=%s\n" "$SPIN_GIT_USERNAME" "$SPIN_GIT_PASSWORD"; }; f'`

const gitWorkspaceScript = `set -eu
command -v git >/dev/null
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
if ! git ls-remote --exit-code --heads origin "$SPIN_GIT_TARGET" >/dev/null 2>&1; then
  git fetch --depth=1 origin "$SPIN_GIT_BOOTSTRAP"
  SPIN_BOOTSTRAP_HEAD="$(git rev-parse FETCH_HEAD)"
  if ! git push origin "$SPIN_BOOTSTRAP_HEAD:refs/heads/$SPIN_GIT_TARGET"; then
    git ls-remote --exit-code --heads origin "$SPIN_GIT_TARGET" >/dev/null
  fi
fi
if git show-ref --verify --quiet "refs/heads/$SPIN_GIT_HEAD"; then
  git checkout -q "$SPIN_GIT_HEAD"
else
  git fetch --depth=1 origin "$SPIN_GIT_BASE"
  git checkout -q -B "$SPIN_GIT_HEAD" FETCH_HEAD
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

func (d *Docker) ExportSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot, destination io.Writer) error {
	if snapshot.Driver != "docker" || strings.TrimSpace(snapshot.Ref) == "" {
		return errors.New("snapshot is not an exportable Docker image")
	}
	cmd := exec.CommandContext(ctx, d.binary, "image", "save", snapshot.Ref)
	cmd.Stdout = destination
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("docker image save %s: %s: %w", snapshot.Ref, strings.TrimSpace(stderr.String()), err)
	}
	return nil
}

func (d *Docker) ImportSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot, source io.Reader) error {
	if snapshot.Driver != "docker" || strings.TrimSpace(snapshot.Ref) == "" {
		return errors.New("snapshot is not an importable Docker image")
	}
	cmd := exec.CommandContext(ctx, d.binary, "image", "load")
	cmd.Stdin = source
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
	if expected := strings.TrimSpace(snapshot.Digest); expected != "" && strings.TrimSpace(loadedDigest) != expected {
		return fmt.Errorf("imported image %s has digest %s, expected %s", snapshot.Ref, strings.TrimSpace(loadedDigest), expected)
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
	return "set -a; if [ -f " + environmentFile + " ]; then . " + environmentFile + "; fi; set +a; echo $$ > " + pidFile + "; exec " + enabled.Command, nil
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
		if strings.TrimSpace(attachment.SourcePath) == "" || !strings.HasPrefix(attachment.TargetPath, "/spin/job-attachments/") || strings.Contains(strings.TrimPrefix(attachment.TargetPath, "/spin/job-attachments/"), "/") {
			return fmt.Errorf("invalid Job attachment target %q", attachment.TargetPath)
		}
		if output, code, err := d.run(ctx, "cp", attachment.SourcePath, runtime.ContainerID+":"+attachment.TargetPath); err != nil || code != 0 {
			return fmt.Errorf("copy Job attachment %s (exit %d): %s: %w", attachment.TargetPath, code, strings.TrimSpace(output), err)
		}
		if output, code, err := d.run(ctx, "exec", runtime.ContainerID, "chmod", "0444", attachment.TargetPath); err != nil || code != 0 {
			return fmt.Errorf("protect Job attachment %s (exit %d): %s: %w", attachment.TargetPath, code, strings.TrimSpace(output), err)
		}
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
	authentication := comparison.Authentication
	if authentication == nil {
		authentication = &GitAuthentication{}
	}
	secretInput := []byte(strings.Join([]string{singleLine(authentication.Username), singleLine(authentication.Password)}, "\n") + "\n")
	output, err := d.controlInput(ctx, secretInput,
		"exec", "-i", "-w", "/workspace",
		"-e", "GIT_TERMINAL_PROMPT=0",
		"-e", "SPIN_COMPARE_BASE="+comparison.BaseRef,
		"-e", "SPIN_COMPARE_HEAD="+comparison.HeadRef,
		"-e", "SPIN_COMPARE_COMMIT_MATCH="+comparison.CommitMessageMatch,
		runtime.ContainerID, "sh", "-lc", compareWorkspaceScript,
	)
	if err != nil {
		return changes, fmt.Errorf("prepare Job comparison: %w", err)
	}
	baseCommit, headCommit := "", ""
	for _, line := range strings.Split(output, "\n") {
		fields := strings.Fields(strings.TrimSpace(line))
		if len(fields) == 3 && fields[0] == "SPIN_COMPARE" {
			baseCommit = strings.TrimPrefix(fields[1], "base=")
			headCommit = strings.TrimPrefix(fields[2], "head=")
		}
	}
	if len(baseCommit) < 7 {
		return changes, errors.New("Git comparison did not resolve a base commit")
	}
	if comparison.CommitMessageMatch != "" && len(headCommit) < 7 {
		return changes, nil
	}
	return d.inspectWorkspace(ctx, runtime, baseCommit, headCommit)
}

const compareWorkspaceScript = `set -eu
IFS= read -r SPIN_GIT_USERNAME || true
IFS= read -r SPIN_GIT_PASSWORD || true
if [ -n "$SPIN_GIT_PASSWORD" ]; then
  ` + gitCredentialEnvironmentScript + `
fi
git fetch -q --depth=256 origin "+refs/heads/$SPIN_COMPARE_BASE:refs/remotes/spin/base" "+refs/heads/$SPIN_COMPARE_HEAD:refs/remotes/spin/head"
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

func (d *Docker) inspectWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, diffBase, diffHead string) (WorkspaceChanges, error) {
	const maxPatchBytes = 512 << 10
	const maxTotalPatchBytes = 2 << 20
	changes := WorkspaceChanges{Files: []WorkspaceFileChange{}}
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return changes, errors.New("composition has no live Docker capsule")
	}
	branch, code, err := d.run(ctx, "exec", "-w", "/workspace", runtime.ContainerID, "git", "branch", "--show-current")
	if err != nil && code < 0 {
		return changes, err
	}
	changes.Branch = strings.TrimSpace(branch)
	statusOutput := ""
	if diffHead == "" {
		var statusCode int
		var statusErr error
		statusOutput, statusCode, statusErr = d.run(ctx, "exec", "-w", "/workspace", runtime.ContainerID, "git", "status", "--porcelain=v1", "--untracked-files=all", "-z")
		if statusErr != nil && statusCode != 0 {
			return changes, fmt.Errorf("git status failed (exit %d): %s", statusCode, strings.TrimSpace(statusOutput))
		}
	}
	byPath := map[string]int{}
	if diffBase != "HEAD" || diffHead != "" {
		nameArgs := []string{"exec", "-w", "/workspace", runtime.ContainerID, "git", "diff", "--name-only", "-z", diffBase}
		if diffHead != "" {
			nameArgs = append(nameArgs, diffHead)
		}
		nameOutput, nameCode, nameErr := d.run(ctx, nameArgs...)
		if nameErr != nil && nameCode != 0 {
			return changes, fmt.Errorf("git range names failed (exit %d): %s", nameCode, strings.TrimSpace(nameOutput))
		}
		for _, path := range strings.Split(nameOutput, "\x00") {
			if path == "" {
				continue
			}
			byPath[path] = len(changes.Files)
			changes.Files = append(changes.Files, WorkspaceFileChange{Path: path, Status: "M "})
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
	numstatArgs := []string{"exec", "-w", "/workspace", runtime.ContainerID, "git", "diff", "--numstat", diffBase}
	if diffHead != "" {
		numstatArgs = append(numstatArgs, diffHead)
	}
	diffOutput, _, _ := d.run(ctx, numstatArgs...)
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
		lineOutput, lineCode, _ := d.run(ctx, "exec", "-w", "/workspace", runtime.ContainerID, "wc", "-l", "--", file.Path)
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
			output, exitCode, _ := d.run(ctx, "exec", "-w", "/workspace", runtime.ContainerID, "git", "diff", "--no-index", "--no-ext-diff", "--no-textconv", "--no-color", "--unified=3", "--", "/dev/null", file.Path)
			if exitCode == 0 || exitCode == 1 {
				patch = output
			}
		} else {
			patchArgs := []string{"exec", "-w", "/workspace", runtime.ContainerID, "git", "diff", "--no-ext-diff", "--no-textconv", "--no-color", "--unified=3", diffBase}
			if diffHead != "" {
				patchArgs = append(patchArgs, diffHead)
			}
			patchArgs = append(patchArgs, "--", file.Path)
			output, exitCode, _ := d.run(ctx, patchArgs...)
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
	output, err := d.controlInput(ctx, secretInput,
		"exec", "-i", "-w", "/workspace",
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
if [ "$SPIN_CHANGED" = 1 ] && [ "$SPIN_ALLOW_CHANGES" != 1 ]; then
  echo 'This workflow phase may not modify the repository; ACCEPT is blocked while the Session differs from its base' >&2
  exit 42
fi
SPIN_COMMITTED=0
if [ "$SPIN_CHANGED" = 1 ]; then
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
fi
if git ls-remote --exit-code --heads origin "$SPIN_GIT_REF" >/dev/null 2>&1; then
  git fetch --depth=50 origin "$SPIN_GIT_REF"
  if ! git merge-base --is-ancestor FETCH_HEAD HEAD; then
    echo 'The Job branch advanced after this Session started; automatic ACCEPT cannot overwrite it' >&2
    exit 43
  fi
fi
git push origin "HEAD:$SPIN_GIT_REF"
SPIN_HEAD="$(git rev-parse HEAD)"
SPIN_REMOTE_HEAD="$(git ls-remote --exit-code --heads origin "$SPIN_GIT_REF" | cut -f1)"
if [ "$SPIN_REMOTE_HEAD" != "$SPIN_HEAD" ]; then
  echo 'Remote Job branch does not match the accepted Session HEAD after push' >&2
  exit 44
fi
printf 'SPIN_ACCEPT committed=%s head=%s\n' "$SPIN_COMMITTED" "$SPIN_HEAD"
unset SPIN_GIT_PASSWORD`

func validGitRef(value string) bool {
	if value == "" || strings.HasPrefix(value, "/") || strings.HasSuffix(value, "/") || strings.Contains(value, "..") {
		return false
	}
	for _, char := range value {
		if (char < 'a' || char > 'z') && (char < 'A' || char > 'Z') && (char < '0' || char > '9') && !strings.ContainsRune("/_-.", char) {
			return false
		}
	}
	return true
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

func materializationRoot(composition domain.Composition, artifacts []domain.Artifact) (domain.Artifact, error) {
	byID := make(map[string]domain.Artifact, len(artifacts))
	for _, artifact := range artifacts {
		byID[artifact.ID] = artifact
	}
	required := make([]string, 0, len(composition.SlotBindings))
	for _, id := range composition.SlotBindings {
		required = append(required, id)
	}
	for i := len(composition.ResolvedArtifacts) - 1; i >= 0; i-- {
		candidate, ok := byID[composition.ResolvedArtifacts[i].ArtifactID]
		if !ok || candidate.Snapshot.Driver != "docker" || !candidate.Snapshot.Restorable || candidate.Snapshot.Ref == "" {
			continue
		}
		closure := map[string]bool{}
		collectParents(candidate.ID, byID, closure)
		if allContained(required, closure) {
			return candidate, nil
		}
	}
	selection := composition.Selector
	if len(composition.WithSelectors) > 0 {
		selection += " WITH " + strings.Join(composition.WithSelectors, " WITH ")
	}
	return domain.Artifact{}, fmt.Errorf("Docker cannot stack independent snapshots for %s; record the later layer with --from=<earlier-layer> or use an overlay-capable engine", selection)
}

func materializationLayers(composition domain.Composition, artifacts []domain.Artifact) ([]domain.Artifact, error) {
	if root, err := materializationRoot(composition, artifacts); err == nil {
		return []domain.Artifact{root}, nil
	}
	byID := make(map[string]domain.Artifact, len(artifacts))
	for _, artifact := range artifacts {
		byID[artifact.ID] = artifact
	}
	requested := append([]string{}, composition.RequestedArtifactIDs...)
	if len(requested) == 0 {
		bound := map[string]bool{}
		for _, id := range composition.SlotBindings {
			bound[id] = true
		}
		for _, resolved := range composition.ResolvedArtifacts {
			if bound[resolved.ArtifactID] {
				requested = append(requested, resolved.ArtifactID)
			}
		}
	}
	requested = uniqueIDs(requested)
	needed := make([]domain.Artifact, 0, len(requested))
	for _, id := range requested {
		artifact, ok := byID[id]
		if !ok {
			return nil, fmt.Errorf("composition artifact %s is missing", id)
		}
		if artifact.Snapshot.Driver != "docker" || !artifact.Snapshot.Restorable || artifact.Snapshot.Ref == "" {
			return nil, fmt.Errorf("artifact %s is not a restorable Docker snapshot", id)
		}
		isAncestor := false
		for _, otherID := range requested {
			if otherID == id {
				continue
			}
			closure := map[string]bool{}
			collectParents(otherID, byID, closure)
			if closure[id] {
				isAncestor = true
				break
			}
		}
		if !isAncestor {
			needed = append(needed, artifact)
		}
	}
	covered := map[string]bool{}
	for _, artifact := range needed {
		collectParents(artifact.ID, byID, covered)
	}
	required := make([]string, 0, len(composition.SlotBindings))
	for _, id := range composition.SlotBindings {
		required = append(required, id)
	}
	if len(needed) == 0 || !allContained(required, covered) {
		return nil, fmt.Errorf("composition %s does not contain every bound snapshot", composition.ID)
	}
	return needed, nil
}

func uniqueIDs(values []string) []string {
	out := make([]string, 0, len(values))
	seen := map[string]bool{}
	for _, value := range values {
		if value != "" && !seen[value] {
			seen[value] = true
			out = append(out, value)
		}
	}
	return out
}

func (d *Docker) materializationArtifact(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact) (domain.Artifact, bool, error) {
	layers, err := materializationLayers(composition, artifacts)
	if err != nil {
		return domain.Artifact{}, false, err
	}
	if len(layers) == 1 {
		return layers[0], false, nil
	}
	ref, err := d.mergeSnapshots(ctx, composition, layers)
	if err != nil {
		return domain.Artifact{}, false, err
	}
	return domain.Artifact{
		ID:   "composition:" + composition.ID,
		Kind: domain.ArtifactKind("composition"), Name: composition.ID,
		Snapshot: domain.CapsuleSnapshot{Driver: "docker", Ref: ref, Restorable: true},
	}, true, nil
}

func (d *Docker) mergeSnapshots(ctx context.Context, composition domain.Composition, layers []domain.Artifact) (string, error) {
	targetName := runtimeName("spin-compose-build", composition.ID)
	imageRef := "spin/composition:" + safeName(composition.ID)
	if _, err := d.control(ctx,
		"create", "--name", targetName,
		"--label", "spin.managed=true",
		"--label", "spin.kind=composition-build",
		"--label", "spin.composition_id="+composition.ID,
		"--entrypoint", "sh", layers[0].Snapshot.Ref, "-lc", "exit 0",
	); err != nil {
		return "", fmt.Errorf("create composition base from %s: %w", layers[0].ID, err)
	}
	defer func() { _ = d.removeContainer(context.Background(), targetName) }()

	for index, layer := range layers[1:] {
		sourceName := runtimeName("spin-compose-source", composition.ID+"-"+strconv.Itoa(index+1))
		if err := d.mergeSnapshot(ctx, targetName, sourceName, layer); err != nil {
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
	copier.Stdin = stream
	var copyError bytes.Buffer
	copier.Stderr = &copyError
	if err := copier.Start(); err != nil {
		return fmt.Errorf("start docker cp: %w", err)
	}
	if err := exporter.Start(); err != nil {
		_ = copier.Process.Kill()
		_ = copier.Wait()
		return fmt.Errorf("start docker export: %w", err)
	}
	exportErr := exporter.Wait()
	copyErr := copier.Wait()
	if exportErr != nil {
		return fmt.Errorf("docker export: %s: %w", strings.TrimSpace(exportError.String()), exportErr)
	}
	if copyErr != nil {
		return fmt.Errorf("docker cp: %s: %w", strings.TrimSpace(copyError.String()), copyErr)
	}
	return nil
}

func collectParents(id string, artifacts map[string]domain.Artifact, found map[string]bool) {
	if found[id] {
		return
	}
	found[id] = true
	for _, parentID := range artifacts[id].ParentArtifactIDs {
		collectParents(parentID, artifacts, found)
	}
}

func allContained(ids []string, found map[string]bool) bool {
	return !slices.ContainsFunc(ids, func(id string) bool { return !found[id] })
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
