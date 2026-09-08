//go:build !tamago

package capsule

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// The three git scripts together: a workspace is made from the Job branch,
// its work in progress is pushed to the Session branch, a fresh workspace
// (another runner) resumes from that branch with the Job base as its base,
// and ACCEPT folds everything into one commit and removes the Session
// branch from the remote.
func TestWorkspaceSyncPushesAndResumesWorkInProgress(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git is not installed")
	}
	root := t.TempDir()
	remote := filepath.Join(root, "remote.git")
	run(t, root, "git", "init", "-q", "--bare", remote)
	seed := filepath.Join(root, "seed")
	run(t, root, "git", "init", "-q", seed)
	run(t, seed, "git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "base")
	run(t, seed, "git", "branch", "-M", "develop")
	run(t, seed, "git", "remote", "add", "origin", remote)
	run(t, seed, "git", "push", "-q", "origin", "develop")

	env := []string{
		"SPIN_GIT_REMOTE=" + remote, "SPIN_GIT_BOOTSTRAP=develop", "SPIN_GIT_BASE=jobs/#1/main",
		"SPIN_GIT_TARGET=jobs/#1/main", "SPIN_GIT_HEAD=jobs/#1/sessions/ses_a",
	}
	first := filepath.Join(root, "first")
	if err := os.MkdirAll(first, 0o755); err != nil {
		t.Fatal(err)
	}
	script(t, first, env, gitWorkspaceScript)
	if err := os.WriteFile(filepath.Join(first, "feature.txt"), []byte("work\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	out := script(t, first, append(env, "SPIN_SESSION_REF=jobs/#1/sessions/ses_a"), syncWorkspaceScript)
	if !strings.Contains(out, "SPIN_SYNC committed=1 pushed=1") {
		t.Fatalf("first sync = %s", out)
	}
	out = script(t, first, append(env, "SPIN_SESSION_REF=jobs/#1/sessions/ses_a"), syncWorkspaceScript)
	if !strings.Contains(out, "SPIN_SYNC committed=0 pushed=0") {
		t.Fatalf("second sync without changes = %s", out)
	}
	remoteRefs := run(t, root, "git", "--git-dir", remote, "branch", "--list")
	if !strings.Contains(remoteRefs, "jobs/#1/sessions/ses_a") {
		t.Fatalf("session branch not on the remote: %s", remoteRefs)
	}

	// Another runner: a fresh workspace resumes from the pushed branch.
	second := filepath.Join(root, "second")
	if err := os.MkdirAll(second, 0o755); err != nil {
		t.Fatal(err)
	}
	script(t, second, env, gitWorkspaceScript)
	if _, err := os.Stat(filepath.Join(second, "feature.txt")); err != nil {
		t.Fatalf("resumed workspace lacks the pushed work: %v", err)
	}
	base := strings.TrimSpace(run(t, second, "git", "config", "spin.baseCommit"))
	jobHead := strings.TrimSpace(run(t, second, "git", "rev-parse", "refs/remotes/origin/jobs/#1/main"))
	if base != jobHead {
		t.Fatalf("resumed base = %s, want the Job branch %s", base, jobHead)
	}
	if err := os.WriteFile(filepath.Join(second, "more.txt"), []byte("more\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	out = script(t, second, append(env, "SPIN_ALLOW_CHANGES=1", "SPIN_GIT_REF=jobs/#1/main", "SPIN_COMMIT_SUBJECT=Feature", "SPIN_COMMIT_BODY=done"), acceptWorkspaceScript)
	if !strings.Contains(out, "SPIN_ACCEPT committed=1") {
		t.Fatalf("accept = %s", out)
	}
	log := run(t, second, "git", "log", "--oneline", "refs/remotes/origin/jobs/#1/main", "^"+jobHead)
	if strings.Count(strings.TrimSpace(log), "\n")+1 != 1 || !strings.Contains(log, "Feature") {
		t.Fatalf("Job branch after accept should carry one folded commit:\n%s", log)
	}
	remoteRefs = run(t, root, "git", "--git-dir", remote, "branch", "--list")
	if strings.Contains(remoteRefs, "sessions/ses_a") {
		t.Fatalf("session branch survived accept: %s", remoteRefs)
	}
}

func run(t *testing.T, dir string, name string, args ...string) string {
	t.Helper()
	command := exec.Command(name, args...)
	command.Dir = dir
	command.Env = append(os.Environ(), "GIT_TERMINAL_PROMPT=0")
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("%s %s: %v\n%s", name, strings.Join(args, " "), err, output)
	}
	return string(output)
}

// script runs one of the capsule's git scripts the way the runner does:
// four credential lines on stdin, the rest as environment.
func script(t *testing.T, dir string, env []string, body string) string {
	t.Helper()
	command := exec.Command("sh", "-c", body)
	command.Dir = dir
	command.Env = append(append(os.Environ(), "GIT_TERMINAL_PROMPT=0"), env...)
	command.Stdin = strings.NewReader("\n\nSpin Test\ntest@spin.invalid\n")
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("script: %v\n%s", err, output)
	}
	return string(output)
}
