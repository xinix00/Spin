package capsule

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"easyacp/internal/domain"
)

func TestAcceptWorkspaceScriptCommitsAndPushesOnlyWhenPolicyAllowsChanges(t *testing.T) {
	root := t.TempDir()
	remote := filepath.Join(root, "remote.git")
	seed := filepath.Join(root, "seed")
	workspace := filepath.Join(root, "workspace")
	testGit(t, root, "init", "--bare", remote)
	testGit(t, root, "init", "-b", "main", seed)
	testGit(t, seed, "config", "user.name", "Test")
	testGit(t, seed, "config", "user.email", "test@example.invalid")
	if err := os.WriteFile(filepath.Join(seed, "README.md"), []byte("base\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	testGit(t, seed, "add", "README.md")
	testGit(t, seed, "commit", "-m", "base")
	testGit(t, seed, "remote", "add", "origin", remote)
	testGit(t, seed, "push", "origin", "main")
	testGit(t, remote, "symbolic-ref", "HEAD", "refs/heads/main")
	testGit(t, root, "clone", remote, workspace)
	testGit(t, workspace, "checkout", "-b", "jobs/feature/sessions/ses-test")
	base := strings.TrimSpace(testGit(t, workspace, "rev-parse", "HEAD"))
	testGit(t, workspace, "config", "user.name", "Agent")
	testGit(t, workspace, "config", "user.email", "agent@example.invalid")
	if err := os.WriteFile(filepath.Join(workspace, "README.md"), []byte("changed\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	testGit(t, workspace, "add", "README.md")
	testGit(t, workspace, "commit", "-m", "agent-created commit")
	if err := os.WriteFile(filepath.Join(workspace, "UNCOMMITTED.md"), []byte("also include this\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	output, err := runAcceptanceScript(workspace, true, "jobs/feature/main")
	if err != nil {
		t.Fatalf("accept workspace: %v\n%s", err, output)
	}
	if !strings.Contains(output, "SPIN_ACCEPT committed=1") {
		t.Fatalf("accept output = %q", output)
	}
	head := strings.TrimSpace(testGit(t, workspace, "rev-parse", "HEAD"))
	remoteHead := strings.Fields(testGit(t, workspace, "ls-remote", "origin", "refs/heads/jobs/feature/main"))[0]
	if remoteHead != head || strings.TrimSpace(testGit(t, workspace, "log", "-1", "--format=%s")) != "workflow(develop): accepted" || strings.TrimSpace(testGit(t, workspace, "log", "-1", "--format=%P")) != base {
		t.Fatalf("accepted head=%s remote=%s", head, remoteHead)
	}
	next := filepath.Join(root, "next")
	testGit(t, root, "clone", remote, next)
	testGit(t, next, "checkout", "-b", "jobs/feature/sessions/ses-next", "origin/jobs/feature/main")
	testGit(t, next, "config", "spin.baseCommit", head)
	if err := os.WriteFile(filepath.Join(next, "NEXT.md"), []byte("next phase\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	output, err = runAcceptanceScript(next, true, "jobs/feature/main")
	if err != nil {
		t.Fatalf("accept next phase: %v\n%s", err, output)
	}
	nextHead := strings.TrimSpace(testGit(t, next, "rev-parse", "HEAD"))
	nextParent := strings.TrimSpace(testGit(t, next, "log", "-1", "--format=%P"))
	if nextParent != head || strings.Fields(testGit(t, next, "ls-remote", "origin", "refs/heads/jobs/feature/main"))[0] != nextHead {
		t.Fatalf("next phase did not extend Job tree: parent=%s previous=%s", nextParent, head)
	}

	readonly := filepath.Join(root, "readonly")
	testGit(t, root, "clone", remote, readonly)
	testGit(t, readonly, "checkout", "-b", "jobs/feature/sessions/ses-review", "origin/jobs/feature/main")
	readonlyBase := strings.TrimSpace(testGit(t, readonly, "rev-parse", "HEAD"))
	testGit(t, readonly, "config", "spin.baseCommit", readonlyBase)
	if err := os.WriteFile(filepath.Join(readonly, "README.md"), []byte("reviewer changed this\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	// A phase without write policy may experiment freely; ACCEPT confirms the
	// base, commits nothing and leaves the reviewer's workspace untouched.
	output, err = runAcceptanceScript(readonly, false, "jobs/readonly/main")
	if err != nil {
		t.Fatalf("read-only accept: %v\n%s", err, output)
	}
	if !strings.Contains(output, "SPIN_ACCEPT committed=0 head="+readonlyBase) {
		t.Fatalf("read-only accept output = %q", output)
	}
	if published := strings.Fields(testGit(t, readonly, "ls-remote", "origin", "refs/heads/jobs/readonly/main"))[0]; published != readonlyBase {
		t.Fatalf("read-only phase published %s, want the base %s", published, readonlyBase)
	}
	if kept, err := os.ReadFile(filepath.Join(readonly, "README.md")); err != nil || string(kept) != "reviewer changed this\n" {
		t.Fatalf("reviewer workspace was altered by ACCEPT: %q, %v", kept, err)
	}
	if head := strings.TrimSpace(testGit(t, readonly, "rev-parse", "HEAD")); head != readonlyBase {
		t.Fatalf("reviewer HEAD moved to %s", head)
	}
}

func TestGitWorkspaceScriptCreatesRemoteJobBranchBeforeSessionAndUsesItAsBase(t *testing.T) {
	root := t.TempDir()
	remote := filepath.Join(root, "remote.git")
	seed := filepath.Join(root, "seed")
	testGit(t, root, "init", "--bare", remote)
	testGit(t, root, "init", "-b", "main", seed)
	testGit(t, seed, "config", "user.name", "Test")
	testGit(t, seed, "config", "user.email", "test@example.invalid")
	if err := os.WriteFile(filepath.Join(seed, "README.md"), []byte("base\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	testGit(t, seed, "add", "README.md")
	testGit(t, seed, "commit", "-m", "base")
	testGit(t, seed, "remote", "add", "origin", remote)
	testGit(t, seed, "push", "origin", "main")

	const jobBranch = "jobs/remote-stack/main"
	first := filepath.Join(root, "first")
	if err := os.Mkdir(first, 0o700); err != nil {
		t.Fatal(err)
	}
	if output, err := runGitWorkspaceScript(first, remote, jobBranch, "main", "jobs/remote-stack/sessions/first", jobBranch); err != nil {
		t.Fatalf("bootstrap first Session: %v\n%s", err, output)
	}
	baseHead := strings.TrimSpace(testGit(t, first, "rev-parse", "HEAD"))
	remoteHead := strings.Fields(testGit(t, first, "ls-remote", "origin", "refs/heads/"+jobBranch))[0]
	if baseHead != remoteHead || strings.TrimSpace(testGit(t, first, "branch", "--show-current")) != "jobs/remote-stack/sessions/first" {
		t.Fatalf("initial Session head=%s remote=%s", baseHead, remoteHead)
	}

	testGit(t, first, "config", "user.name", "Spin")
	testGit(t, first, "config", "user.email", "spin@example.invalid")
	if err := os.WriteFile(filepath.Join(first, "ACCEPTED.md"), []byte("accepted\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	testGit(t, first, "add", "ACCEPTED.md")
	testGit(t, first, "commit", "-m", "accepted")
	testGit(t, first, "push", "origin", "HEAD:"+jobBranch)
	acceptedHead := strings.TrimSpace(testGit(t, first, "rev-parse", "HEAD"))

	second := filepath.Join(root, "second")
	if err := os.Mkdir(second, 0o700); err != nil {
		t.Fatal(err)
	}
	if output, err := runGitWorkspaceScript(second, remote, jobBranch, "main", "jobs/remote-stack/sessions/second", jobBranch); err != nil {
		t.Fatalf("start second Session: %v\n%s", err, output)
	}
	if head := strings.TrimSpace(testGit(t, second, "rev-parse", "HEAD")); head != acceptedHead {
		t.Fatalf("second Session started at %s, want remote Job head %s", head, acceptedHead)
	}
	if immutableBase := strings.TrimSpace(testGit(t, second, "config", "--get", "spin.baseCommit")); immutableBase != acceptedHead {
		t.Fatalf("second Session base = %s, want %s", immutableBase, acceptedHead)
	}
}

func testGit(t *testing.T, directory string, arguments ...string) string {
	t.Helper()
	command := exec.Command("git", arguments...)
	command.Dir = directory
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("git %s: %v\n%s", strings.Join(arguments, " "), err, output)
	}
	return string(output)
}

func runAcceptanceScript(directory string, allowChanges bool, remoteRef string) (string, error) {
	allow := "0"
	if allowChanges {
		allow = "1"
	}
	command := exec.Command("sh", "-lc", acceptWorkspaceScript)
	command.Dir = directory
	command.Env = append(os.Environ(),
		"SPIN_ALLOW_CHANGES="+allow,
		"SPIN_GIT_REF="+remoteRef,
		"SPIN_COMMIT_SUBJECT=workflow(develop): accepted",
		"SPIN_COMMIT_BODY=Spin-Session: ses-test",
	)
	command.Stdin = strings.NewReader("\n\nSpin Agent\nspin@example.invalid\n")
	output, err := command.CombinedOutput()
	return string(output), err
}

func runGitWorkspaceScript(directory, remote, baseRef, bootstrapRef, headRef, targetRef string) (string, error) {
	command := exec.Command("sh", "-lc", gitWorkspaceScript)
	command.Dir = directory
	command.Env = append(os.Environ(),
		"SPIN_GIT_REMOTE="+remote,
		"SPIN_GIT_BASE="+baseRef,
		"SPIN_GIT_BOOTSTRAP="+bootstrapRef,
		"SPIN_GIT_HEAD="+headRef,
		"SPIN_GIT_TARGET="+targetRef,
	)
	command.Stdin = strings.NewReader("\n\nSpin\nspin@example.invalid\n")
	output, err := command.CombinedOutput()
	return string(output), err
}

func TestEnabledLaunchCommandSourcesLayerEnvironment(t *testing.T) {
	command, err := enabledLaunchCommand(domain.Enablement{Name: "acp", Command: "codex-acp"}, "/tmp/spin-enabled-test.pid")
	if err != nil {
		t.Fatal(err)
	}
	for _, expected := range []string{"/etc/spin/enabled/acp.env", "set -a", "exec codex-acp"} {
		if !strings.Contains(command, expected) {
			t.Fatalf("launch command %q does not contain %q", command, expected)
		}
	}
	if _, err := enabledLaunchCommand(domain.Enablement{Name: "../acp", Command: "codex-acp"}, "/tmp/test.pid"); err == nil {
		t.Fatal("unsafe capability name was accepted")
	}
}

func TestDockerGitAuthenticationIsTransientLive(t *testing.T) {
	if os.Getenv("SPIN_DOCKER_LIVE") != "1" {
		t.Skip("set SPIN_DOCKER_LIVE=1 and SPIN_TEST_GIT_IMAGE to run the Docker integration")
	}
	image := strings.TrimSpace(os.Getenv("SPIN_TEST_GIT_IMAGE"))
	if image == "" {
		t.Fatal("SPIN_TEST_GIT_IMAGE is required")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()
	engine, err := NewDocker(ctx, DockerConfig{BaseImage: "alpine:3.24"})
	if err != nil {
		t.Fatal(err)
	}
	artifact := domain.Artifact{
		ID: "live-git", Kind: domain.ArtifactTool, Name: "git",
		Snapshot: domain.CapsuleSnapshot{Driver: "docker", Ref: image, Restorable: true},
	}
	composition := domain.Composition{
		ID: "cmp_git_auth_live", Operator: "derek", Selector: "session:live", SessionID: "ses_git_auth_live",
		ResolvedArtifacts: []domain.ResolvedArtifact{{ArtifactID: artifact.ID}}, SlotBindings: map[string]string{"tool:git": artifact.ID},
		Git: &domain.GitWorkspace{
			RepositoryID: "live", RepositoryName: "hello-world", RemoteURL: "https://github.com/octocat/Hello-World.git",
			BaseRef: "master", HeadRef: "jobs/live/sessions/smoke", TargetRef: "jobs/live/main", AccountID: "gac_live",
		},
	}
	const token = "spin-secret-must-not-persist"
	runtime, err := engine.MaterializeWithGitAuthentication(ctx, composition, []domain.Artifact{artifact}, &GitAuthentication{
		Username: "octocat", Password: token, AuthorName: "Spin Smoke", AuthorEmail: "spin-smoke@example.invalid",
	})
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		_ = engine.Stop(context.Background(), runtime)
		_, _ = engine.control(context.Background(), "volume", "rm", runtime.WorkspaceRef)
	}()
	inspect, code, err := engine.run(ctx, "container", "inspect", runtime.ContainerID)
	if err != nil || code != 0 {
		t.Fatalf("inspect: code=%d err=%v output=%s", code, err, inspect)
	}
	if strings.Contains(inspect, token) {
		t.Fatal("Git token persisted in Session container metadata")
	}
	output, code, err := engine.run(ctx, "exec", runtime.ContainerID, "sh", "-lc", "git remote get-url origin; git branch --show-current; git config --get user.email")
	if err != nil || code != 0 {
		t.Fatalf("workspace check: code=%d err=%v output=%s", code, err, output)
	}
	for _, expected := range []string{"https://github.com/octocat/Hello-World.git", "jobs/live/sessions/smoke", "spin-smoke@example.invalid"} {
		if !strings.Contains(output, expected) {
			t.Fatalf("workspace output %q does not contain %q", output, expected)
		}
	}
}

func TestDockerAcceptWorkspaceCommitsInsideSessionAndPushesJobBranchLive(t *testing.T) {
	if os.Getenv("SPIN_DOCKER_LIVE") != "1" {
		t.Skip("set SPIN_DOCKER_LIVE=1 and SPIN_TEST_GIT_IMAGE to run the Docker integration")
	}
	image := strings.TrimSpace(os.Getenv("SPIN_TEST_GIT_IMAGE"))
	if image == "" {
		t.Fatal("SPIN_TEST_GIT_IMAGE is required")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	engine, err := NewDocker(ctx, DockerConfig{BaseImage: "alpine:3.24"})
	if err != nil {
		t.Fatal(err)
	}
	volume := "spin-test-workflow-accept"
	_, _ = engine.control(ctx, "volume", "rm", volume)
	if _, err := engine.control(ctx, "volume", "create", volume); err != nil {
		t.Fatal(err)
	}
	containerID, err := engine.control(ctx, "run", "-d", "--rm", "--mount", "type=volume,src="+volume+",dst=/workspace", "--entrypoint", "sh", image, "-lc", "sleep 120")
	if err != nil {
		t.Fatal(err)
	}
	runtime := domain.CapsuleRuntime{Driver: "docker", ContainerID: strings.TrimSpace(containerID), WorkspaceRef: volume, Status: "ready"}
	defer func() {
		_ = engine.Stop(context.Background(), runtime)
		_, _ = engine.control(context.Background(), "volume", "rm", volume)
	}()
	setup := `set -eu
cd /workspace
git init -q -b main
git config user.name Agent
git config user.email agent@example.invalid
printf 'base\n' > README.md
git add README.md
git commit -q -m base
git init -q --bare /tmp/remote.git
git remote add origin /tmp/remote.git
git push -q origin main
git checkout -q -b jobs/live/sessions/ses-develop
git config spin.baseCommit "$(git rev-parse HEAD)"
printf 'changed in Session\n' > README.md`
	if _, err := engine.control(ctx, "exec", runtime.ContainerID, "sh", "-lc", setup); err != nil {
		t.Fatal(err)
	}
	accepted, err := engine.AcceptWorkspace(ctx, runtime, WorkspaceAcceptance{
		AllowChanges: true, CommitSubject: "workflow(develop): accepted", CommitBody: "Spin-Session: ses-develop",
		RemoteRef: "jobs/live/main", Authentication: &GitAuthentication{AuthorName: "Spin Agent", AuthorEmail: "spin@example.invalid"},
	})
	if err != nil || !accepted.Committed {
		t.Fatalf("Docker ACCEPT = %+v, error = %v", accepted, err)
	}
	verification, err := engine.control(ctx, "exec", "-w", "/workspace", runtime.ContainerID, "sh", "-lc", `set -eu
test "$(git rev-parse HEAD)" = "$(git ls-remote origin refs/heads/jobs/live/main | cut -f1)"
test "$(git log -1 --format=%s)" = 'workflow(develop): accepted'
git checkout -q -b jobs/live/sessions/ses-review
git config spin.baseCommit "$(git rev-parse HEAD)"
printf 'reviewer mutation\n' > REVIEW.md`)
	if err != nil {
		t.Fatalf("verify accepted Job branch: %v\n%s", err, verification)
	}
	reviewed, err := engine.AcceptWorkspace(ctx, runtime, WorkspaceAcceptance{
		AllowChanges: false, CommitSubject: "workflow(review): accepted", CommitBody: "Spin-Session: ses-review",
		RemoteRef: "jobs/live/main", Authentication: &GitAuthentication{AuthorName: "Spin Agent", AuthorEmail: "spin@example.invalid"},
	})
	if err != nil || reviewed.Committed || reviewed.Head != accepted.Head {
		t.Fatalf("read-only Docker ACCEPT = %+v (develop head %s), error = %v", reviewed, accepted.Head, err)
	}
}

func TestDockerCredentialHelperWorksWithNoExecTmpfsLive(t *testing.T) {
	if os.Getenv("SPIN_DOCKER_LIVE") != "1" {
		t.Skip("set SPIN_DOCKER_LIVE=1 and SPIN_TEST_GIT_IMAGE to run the Docker integration")
	}
	image := strings.TrimSpace(os.Getenv("SPIN_TEST_GIT_IMAGE"))
	if image == "" {
		t.Fatal("SPIN_TEST_GIT_IMAGE is required")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	engine, err := NewDocker(ctx, DockerConfig{BaseImage: "alpine:3.24"})
	if err != nil {
		t.Fatal(err)
	}
	const username = "spin-test-user"
	const password = "spin-test-password"
	probe := `set -eu
IFS= read -r SPIN_GIT_USERNAME
IFS= read -r SPIN_GIT_PASSWORD
` + gitCredentialEnvironmentScript + `
printf 'protocol=https\nhost=example.test\n\n' | git credential fill`
	output, err := engine.controlInput(ctx, []byte(username+"\n"+password+"\n"),
		"run", "-i", "--rm", "--read-only", "--tmpfs", "/tmp:rw,nosuid,nodev,size=1m",
		"--entrypoint", "sh", image, "-lc", probe,
	)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(output, "username="+username) || !strings.Contains(output, "password="+password) {
		t.Fatalf("credential helper output is incomplete")
	}
}

func TestDockerMaterializesIndependentSnapshotsLive(t *testing.T) {
	if os.Getenv("SPIN_DOCKER_LIVE") != "1" {
		t.Skip("set SPIN_DOCKER_LIVE=1 to run the Docker integration")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()
	engine, err := NewDocker(ctx, DockerConfig{BaseImage: "alpine:3.24"})
	if err != nil {
		t.Fatal(err)
	}
	suffix := time.Now().UTC().Format("150405.000000000")
	record := func(id, path string) domain.CapsuleSnapshot {
		recording := domain.Recording{ID: id + suffix, Kind: domain.ArtifactKind("test"), Name: id}
		runtime, err := engine.StartRecording(ctx, recording, nil)
		if err != nil {
			t.Fatal(err)
		}
		recording.Runtime = &runtime
		if execution, err := engine.Execute(ctx, recording, "printf present > "+path); err != nil || execution.ExitCode != 0 {
			t.Fatalf("record %s: exit=%d err=%v output=%s", id, execution.ExitCode, err, execution.Output)
		}
		snapshot, err := engine.Seal(ctx, recording)
		if err != nil {
			t.Fatal(err)
		}
		return snapshot
	}
	firstSnapshot := record("merge-a", "/spin-layer-a")
	defer func() { _ = engine.RemoveSnapshot(context.Background(), firstSnapshot) }()
	secondSnapshot := record("merge-b", "/spin-layer-b")
	defer func() { _ = engine.RemoveSnapshot(context.Background(), secondSnapshot) }()
	first := domain.Artifact{ID: "layer-a", Snapshot: firstSnapshot}
	second := domain.Artifact{ID: "layer-b", Snapshot: secondSnapshot}
	composition := domain.Composition{
		ID: "cmp_merge_" + suffix, Operator: "tester", Selector: "test:a", WithSelectors: []string{"test:b"},
		RequestedArtifactIDs: []string{first.ID, second.ID}, Layers: []string{first.ID, second.ID},
		ResolvedArtifacts: []domain.ResolvedArtifact{{ArtifactID: first.ID}, {ArtifactID: second.ID}},
		SlotBindings:      map[string]string{"test:a": first.ID, "test:b": second.ID},
	}
	runtime, err := engine.Materialize(ctx, composition, []domain.Artifact{first, second})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(runtime.BaseRef, "spin/composition:") {
		t.Fatalf("base ref = %q", runtime.BaseRef)
	}
	output, code, err := engine.run(ctx, "exec", runtime.ContainerID, "sh", "-lc", "test -f /spin-layer-a -a -f /spin-layer-b")
	if err != nil || code != 0 {
		t.Fatalf("merged files: code=%d err=%v output=%s", code, err, output)
	}
	if err := engine.Stop(ctx, runtime); err != nil {
		t.Fatal(err)
	}
	if _, code, _ := engine.run(ctx, "image", "inspect", runtime.BaseRef); code == 0 {
		t.Fatalf("ephemeral composition image %s still exists after Stop", runtime.BaseRef)
	}
}

// A credential recorded on an older tool is edited on the tool as it is now:
// the capsule runs on the parent's stack, every layer at its newest version,
// and the saved version goes back on the credential's own image as a small
// delta. Composing the stack afterwards gives the new tool and both
// credential versions.
func TestDockerRecordingRunsOnTheParentsCurrentStackLive(t *testing.T) {
	if os.Getenv("SPIN_DOCKER_LIVE") != "1" {
		t.Skip("set SPIN_DOCKER_LIVE=1 to run the Docker integration")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Minute)
	defer cancel()
	engine, err := NewDocker(ctx, DockerConfig{BaseImage: "alpine:3.24"})
	if err != nil {
		t.Fatal(err)
	}
	suffix := time.Now().UTC().Format("150405.000000000")
	exec := func(recording domain.Recording, script string) string {
		t.Helper()
		execution, err := engine.Execute(ctx, recording, script)
		if err != nil || execution.ExitCode != 0 {
			t.Fatalf("%s: exit=%d err=%v output=%s", script, execution.ExitCode, err, execution.Output)
		}
		return execution.Output
	}
	record := func(id, script string, parent *domain.Artifact, parents ...string) domain.Artifact {
		t.Helper()
		recording := domain.Recording{ID: id + suffix, Kind: domain.ArtifactKind("test"), Name: id, ParentArtifactIDs: parents}
		var from []domain.Artifact
		if parent != nil {
			from = []domain.Artifact{*parent}
		}
		runtime, err := engine.StartRecording(ctx, recording, from)
		if err != nil {
			t.Fatal(err)
		}
		recording.Runtime = &runtime
		exec(recording, script)
		snapshot, err := engine.Seal(ctx, recording)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { _ = engine.RemoveSnapshot(context.Background(), snapshot) })
		return domain.Artifact{ID: id, ParentArtifactIDs: parents, Snapshot: snapshot}
	}
	tool1 := record("tool1", "mkdir -p /tool && echo 2.223 > /tool/version", nil)
	credential := record("cred1", "echo secret > /root/credential", &tool1, "tool1")
	tool2 := record("tool2", "echo 2.280 > /tool/version", &tool1, "tool1")
	tool1.SupersededBy = "tool2"

	// Editing the credential: its own image still holds the old tool.
	edit := domain.Recording{ID: "cred2" + suffix, Kind: domain.ArtifactKind("test"), Name: "cred2", ParentArtifactIDs: []string{"cred1"}}
	stack := RecordingStack{Layers: []string{"tool1", "tool2", "cred1"}, Artifacts: []domain.Artifact{tool1, tool2, credential}}
	runtime, err := engine.StartRecordingOnStack(ctx, edit, []domain.Artifact{credential}, stack)
	if err != nil {
		t.Fatal(err)
	}
	if runtime.ParentRef != credential.Snapshot.Ref {
		t.Fatalf("parent ref = %q, want %q", runtime.ParentRef, credential.Snapshot.Ref)
	}
	edit.Runtime = &runtime
	if got := strings.TrimSpace(exec(edit, "cat /tool/version")); got != "2.280" {
		t.Fatalf("the edit runs on tool %s, not the newest", got)
	}
	if got := strings.TrimSpace(exec(edit, "cat /root/credential")); got != "secret" {
		t.Fatalf("the credential is missing from the edit: %q", got)
	}
	exec(edit, "echo renewed > /root/credential")
	sealed, err := engine.Seal(ctx, edit)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = engine.RemoveSnapshot(context.Background(), sealed) })
	if !sealed.Delta || sealed.ParentRef != credential.Snapshot.Ref || !engine.imageExtends(ctx, sealed.Ref, credential.Snapshot.Ref) {
		t.Fatalf("the new version is not one layer over the credential: delta=%v parent=%q", sealed.Delta, sealed.ParentRef)
	}
	renewed := domain.Artifact{ID: "cred2", ParentArtifactIDs: []string{"cred1"}, Snapshot: sealed}
	credential.SupersededBy = "cred2"

	composition := domain.Composition{ID: "cmp_stack_" + suffix, Operator: "tester", Layers: []string{"tool1", "tool2", "cred1", "cred2"}}
	use, err := engine.Materialize(ctx, composition, []domain.Artifact{tool1, tool2, credential, renewed})
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = engine.Stop(context.Background(), use) }()
	output, code, err := engine.run(ctx, "exec", use.ContainerID, "sh", "-lc", "cat /tool/version /root/credential")
	if err != nil || code != 0 || strings.Fields(output)[0] != "2.280" || strings.Fields(output)[1] != "renewed" {
		t.Fatalf("composed stack reads %q (code %d, err %v)", output, code, err)
	}
}
