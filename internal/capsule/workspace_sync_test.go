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

// The browser scripts list a ref's files with sizes and read one file, for
// the working tree and for a Git ref.
func TestWorkspaceBrowseScriptsListAndReadFiles(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git is not installed")
	}
	dir := t.TempDir()
	run(t, dir, "git", "init", "-q")
	if err := os.MkdirAll(filepath.Join(dir, "src"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "src", "main.go"), []byte("package main\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, dir, "git", "add", "-A")
	run(t, dir, "git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "one")
	if err := os.WriteFile(filepath.Join(dir, "notes.txt"), []byte("untracked\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	listing := script(t, dir, []string{"SPIN_REF=workspace"}, listWorkspaceScript)
	if !strings.Contains(listing, "0\tsrc/main.go") || !strings.Contains(listing, "0\tnotes.txt") {
		t.Fatalf("working tree listing = %q", listing)
	}
	listing = script(t, dir, []string{"SPIN_REF=HEAD"}, listWorkspaceScript)
	if !strings.Contains(listing, "13\tsrc/main.go") || strings.Contains(listing, "notes.txt") {
		t.Fatalf("HEAD listing = %q", listing)
	}
	content := script(t, dir, []string{"SPIN_REF=HEAD", "SPIN_PATH=src/main.go", "SPIN_LIMIT=5"}, readWorkspaceFileScript)
	if content != "SPIN_SIZE 13\npacka" {
		t.Fatalf("HEAD read = %q", content)
	}
	content = script(t, dir, []string{"SPIN_REF=workspace", "SPIN_PATH=notes.txt", "SPIN_LIMIT=1024"}, readWorkspaceFileScript)
	if content != "SPIN_SIZE 10\nuntracked\n" {
		t.Fatalf("working tree read = %q", content)
	}
	for _, path := range []string{"../x", "/etc/passwd", "a/../b", "-flag"} {
		if validWorkspacePath(path) {
			t.Fatalf("%q was accepted as a workspace path", path)
		}
	}
}

// The repository browse script keeps a shallow clone and lists branches, a
// tree and a file from the remote.
func TestRepositoryBrowseScriptReadsARemote(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git is not installed")
	}
	root := t.TempDir()
	remote := filepath.Join(root, "remote.git")
	run(t, root, "git", "init", "-q", "--bare", remote)
	seed := filepath.Join(root, "seed")
	run(t, root, "git", "init", "-q", seed)
	if err := os.WriteFile(filepath.Join(seed, "hello.txt"), []byte("hello\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, seed, "git", "add", "-A")
	run(t, seed, "git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "one")
	run(t, seed, "git", "branch", "-M", "develop")
	run(t, seed, "git", "branch", "feature")
	run(t, seed, "git", "remote", "add", "origin", remote)
	run(t, seed, "git", "push", "-q", "origin", "develop", "feature")

	clone := filepath.Join(root, "clone")
	if err := os.MkdirAll(clone, 0o755); err != nil {
		t.Fatal(err)
	}
	env := []string{"SPIN_GIT_REMOTE=" + remote, "SPIN_LIMIT=1024"}
	refs := browse(t, clone, append(env, "SPIN_MODE=refs", "SPIN_REF=", "SPIN_PATH="))
	if !strings.Contains(refs, "develop\n") || !strings.Contains(refs, "feature\n") {
		t.Fatalf("refs = %q", refs)
	}
	tree := browse(t, clone, append(env, "SPIN_MODE=tree", "SPIN_REF=develop", "SPIN_PATH="))
	if !strings.Contains(tree, "6\thello.txt") {
		t.Fatalf("tree = %q", tree)
	}
	file := browse(t, clone, append(env, "SPIN_MODE=file", "SPIN_REF=feature", "SPIN_PATH=hello.txt"))
	if file != "SPIN_SIZE 6\nhello\n" {
		t.Fatalf("file = %q", file)
	}
	// The clone is reused: a second tree read needs no fresh init.
	if _, err := os.Stat(filepath.Join(clone, ".git")); err != nil {
		t.Fatal("no clone was kept")
	}
	tree = browse(t, clone, append(env, "SPIN_MODE=tree", "SPIN_REF=feature", "SPIN_PATH="))
	if !strings.Contains(tree, "hello.txt") {
		t.Fatalf("second tree = %q", tree)
	}
}

// browse runs the repository browse script with empty credentials.
func browse(t *testing.T, dir string, env []string) string {
	t.Helper()
	command := exec.Command("sh", "-c", browseRepositoryScript)
	command.Dir = dir
	command.Env = append(append(os.Environ(), "GIT_TERMINAL_PROMPT=0"), env...)
	command.Stdin = strings.NewReader("\n\n")
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("browse script: %v\n%s", err, output)
	}
	return string(output)
}
