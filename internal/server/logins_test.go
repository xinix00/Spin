package server

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"strings"
	"testing"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// trackedTestEngine is the test engine with a file per capsule, the way a
// runner reads and writes tracked files in a container. A capsule nobody
// wrote to yet holds the image's files.
type trackedTestEngine struct {
	testEngine
	image map[string][]byte
	files map[string]map[string][]byte // container -> path -> content
}

func (e *trackedTestEngine) capsule(container string) map[string][]byte {
	if e.files[container] == nil {
		e.files[container] = map[string][]byte{}
		for path, data := range e.image {
			e.files[container][path] = append([]byte(nil), data...)
		}
	}
	return e.files[container]
}

func (e *trackedTestEngine) ReadTrackedFiles(_ context.Context, runtime domain.CapsuleRuntime, selection capsule.TrackedSelection) (map[string][]byte, error) {
	out := map[string][]byte{}
	for path, data := range e.capsule(runtime.ContainerID) {
		if domain.TrackedCovers(path, selection.Paths, selection.Excludes) && !strings.HasSuffix(path, ".lock") {
			out[path] = append([]byte(nil), data...)
		}
	}
	return out, nil
}

func (e *trackedTestEngine) WriteTrackedFiles(_ context.Context, runtime domain.CapsuleRuntime, files map[string][]byte) error {
	capsule := e.capsule(runtime.ContainerID)
	for path, data := range files {
		capsule[path] = append([]byte(nil), data...)
	}
	return nil
}

func (e *trackedTestEngine) token(container, path string) string {
	return string(e.capsule(container)[path])
}

func newLoginTestServer(t *testing.T, kind domain.ArtifactKind, path string) (*Server, *store.Store, *trackedTestEngine, string) {
	t.Helper()
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &trackedTestEngine{image: map[string][]byte{path: []byte("token-1")}, files: map[string]map[string][]byte{}}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	layers := buildLayers(t, srv, "derek", gitLayer(), layerSpec{Kind: kind, Name: "claude", Scope: domain.ScopeUser, From: "tool:git", Install: "login"})
	if _, err := st.SetArtifactTrackedPaths(layers[1].ID, []string{path}); err != nil {
		t.Fatal(err)
	}
	return srv, st, engine, store.LayerKey(layers[1])
}

// A credential layer hands its logins out: the layer's own files are login
// 1 and go to the first capsule; a second capsule gets nothing until a
// person logs in once more and saves that as login 2; a capsule's rotation
// stays in its own login; a stopped capsule frees its login for the next.
func TestCredentialLayerHandsOutOneLoginPerCapsule(t *testing.T) {
	const path = "/root/.claude/.credentials.json"
	srv, st, engine, key := newLoginTestServer(t, domain.ArtifactCredential, path)
	ctx := context.Background()

	first := useLayers(t, srv, "derek", "credential:claude")
	logins := st.LoginsFor(key)
	if len(logins) != 1 || logins[0].Number != 1 || string(logins[0].Files[path]) != "token-1" || first.Logins[key] != logins[0].ID {
		t.Fatalf("after the first start: logins=%+v held=%v", logins, first.Logins)
	}
	if _, err := srv.useCapsule(ctx, domain.UseRequest{Operator: "derek", Selector: "credential:claude", Profile: "default"}); !errors.Is(err, store.ErrLoginsBusy) {
		t.Fatalf("a second capsule with one login held: err=%v", err)
	}
	if running := st.RunningCompositions(); len(running) != 1 {
		t.Fatalf("the refused start left %d running compositions", len(running))
	}

	// Log in once more: a capsule without a login, the person logs in, saves.
	fresh, err := srv.useCapsule(ctx, domain.UseRequest{Operator: "derek", Selector: "credential:claude", Profile: "default", ForLogin: true})
	if err != nil {
		t.Fatal(err)
	}
	if len(fresh.Logins) != 0 || engine.token(fresh.Runtime.ContainerID, path) != "token-1" {
		t.Fatalf("a capsule for a new login holds %v and %q", fresh.Logins, engine.token(fresh.Runtime.ContainerID, path))
	}
	if _, err := srv.saveNewLogin(ctx, first.ID, "derek"); err == nil {
		t.Fatal("a capsule that holds a login saved a new one")
	}
	engine.capsule(fresh.Runtime.ContainerID)[path] = []byte("token-B")
	saved, err := srv.saveNewLogin(ctx, fresh.ID, "derek")
	if err != nil || len(saved) != 1 || saved[0].Number != 2 || string(saved[0].Files[path]) != "token-B" {
		t.Fatalf("saved=%+v err=%v", saved, err)
	}
	if _, err := srv.saveNewLogin(ctx, fresh.ID, "derek"); err == nil {
		t.Fatal("saving twice made a third login")
	}
	// The login is held by the capsule it came from until that stops; what
	// the person does there afterwards is kept in it.
	engine.capsule(fresh.Runtime.ContainerID)[path] = []byte("token-B2")
	if _, err := srv.stopCapsule(ctx, fresh.ID, "derek"); err != nil {
		t.Fatal(err)
	}
	second := useLayers(t, srv, "derek", "credential:claude")
	if second.Logins[key] != saved[0].ID || engine.token(second.Runtime.ContainerID, path) != "token-B2" {
		t.Fatalf("the second capsule holds login %s with %q; expected login 2 with token-B2", second.Logins[key], engine.token(second.Runtime.ContainerID, path))
	}

	// A rotation in the first capsule stays in login 1; the second capsule
	// is not touched.
	engine.capsule(first.Runtime.ContainerID)[path] = []byte("token-1b")
	srv.captureLoginState(ctx, first)
	if login, _ := st.Login(first.Logins[key]); string(login.Files[path]) != "token-1b" {
		t.Fatalf("login 1 holds %q after the rotation", login.Files[path])
	}
	if got := engine.token(second.Runtime.ContainerID, path); got != "token-B2" {
		t.Fatalf("the second capsule changed to %q", got)
	}
	// Nothing free: a third capsule waits. The first stops: its login is
	// free and the next capsule starts with the rotated token.
	if _, err := srv.useCapsule(ctx, domain.UseRequest{Operator: "derek", Selector: "credential:claude", Profile: "default"}); !errors.Is(err, store.ErrLoginsBusy) {
		t.Fatalf("a third capsule with both logins held: err=%v", err)
	}
	if _, err := st.DeleteLogin(first.Logins[key]); !errors.Is(err, store.ErrConflict) {
		t.Fatalf("removing a held login: err=%v", err)
	}
	engine.capsule(first.Runtime.ContainerID)[path] = []byte("token-1c")
	if _, err := srv.stopCapsule(ctx, first.ID, "derek"); err != nil {
		t.Fatal(err)
	}
	third := useLayers(t, srv, "derek", "credential:claude")
	if third.Logins[key] != first.Logins[key] || engine.token(third.Runtime.ContainerID, path) != "token-1c" {
		t.Fatalf("the third capsule holds login %s with %q; expected login 1 with token-1c", third.Logins[key], engine.token(third.Runtime.ContainerID, path))
	}
	summaries := srv.store.Snapshot().Logins
	if len(summaries) != 2 || summaries[0].CompositionID != third.ID || summaries[1].CompositionID != second.ID || summaries[0].Files != 1 {
		t.Fatalf("summaries = %+v", summaries)
	}
	if _, err := srv.stopCapsule(ctx, second.ID, "derek"); err != nil {
		t.Fatal(err)
	}
	if _, err := st.DeleteLogin(saved[0].ID); err != nil {
		t.Fatalf("removing a free login: %v", err)
	}
	if logins := st.LoginsFor(key); len(logins) != 1 || logins[0].Number != 1 {
		t.Fatalf("after the removal: %+v", logins)
	}
}

// A layer that is not a credential layer has one login every capsule
// shares: both start with it, and what one capsule kept is what the next
// capsule gets.
func TestSharedLayerHasOneLoginForEveryCapsule(t *testing.T) {
	const path = "/root/.config/tool.json"
	srv, st, engine, key := newLoginTestServer(t, domain.ArtifactTool, path)
	ctx := context.Background()
	first := useLayers(t, srv, "derek", "tool:claude")
	second := useLayers(t, srv, "derek", "tool:claude")
	if first.Logins[key] == "" || first.Logins[key] != second.Logins[key] {
		t.Fatalf("two capsules hold %v and %v; expected the same login", first.Logins, second.Logins)
	}
	if logins := st.LoginsFor(key); len(logins) != 1 {
		t.Fatalf("a shared layer made %d logins", len(logins))
	}
	engine.capsule(first.Runtime.ContainerID)[path] = []byte("token-2")
	srv.captureLoginState(ctx, first)
	third := useLayers(t, srv, "derek", "tool:claude")
	if got := engine.token(third.Runtime.ContainerID, path); got != "token-2" {
		t.Fatalf("a new capsule starts with %q; expected token-2", got)
	}
}

// A stack with two versions of one credential layer is one login target:
// saving a new login makes one login, not one per version.
func TestTwoVersionsOfOneLayerAreOneLoginTarget(t *testing.T) {
	const path = "/root/.claude/.credentials.json"
	srv, st, engine, key := newLoginTestServer(t, domain.ArtifactCredential, path)
	// A new version of the credential layer, on the same key.
	layers := buildLayers(t, srv, "derek", layerSpec{Kind: domain.ArtifactCredential, Name: "claude", Scope: domain.ScopeUser, From: "credential:claude", Install: "relogin"})
	if _, err := st.SetArtifactTrackedPaths(layers[0].ID, []string{path}); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	fresh, err := srv.useCapsule(ctx, domain.UseRequest{Operator: "derek", Selector: "credential:claude", Profile: "default", ForLogin: true})
	if err != nil {
		t.Fatal(err)
	}
	if targets := srv.trackedTargets(fresh); len(targets) != 1 || targets[0].key != key {
		t.Fatalf("targets = %+v", targets)
	}
	engine.capsule(fresh.Runtime.ContainerID)[path] = []byte("token-B")
	saved, err := srv.saveNewLogin(ctx, fresh.ID, "derek")
	if err != nil || len(saved) != 1 {
		t.Fatalf("saved=%+v err=%v", saved, err)
	}
	if logins := st.LoginsFor(key); len(logins) != 1 {
		t.Fatalf("saving once made %d logins", len(logins))
	}
}

// A tracked folder is kept whole: its files travel apart from the excludes
// and lock files, and a file gone from the folder in the capsule goes from
// the login too.
func TestTrackedFolderIsKeptWholeWithoutExcludesAndLocks(t *testing.T) {
	srv, st, engine, key := newLoginTestServer(t, domain.ArtifactCredential, "/root/.claude/.credentials.json")
	layers := st.Snapshot().Artifacts
	var credential domain.Artifact
	for _, artifact := range layers {
		if artifact.Kind == domain.ArtifactCredential {
			credential = artifact
		}
	}
	if _, err := st.SetArtifactTracked(credential.ID, []string{"/root/.claude/"}, []string{"/root/.claude/cache/", "/root/.claude/history.jsonl", "/elsewhere/"}); err != nil {
		t.Fatal(err)
	}
	updated, _ := st.Artifact(credential.ID)
	if len(updated.TrackedExcludes) != 2 {
		t.Fatalf("excludes = %v; the one outside the folder should be dropped", updated.TrackedExcludes)
	}
	engine.image["/root/.claude/settings.json"] = []byte("{}")
	engine.image["/root/.claude/cache/big.bin"] = []byte("cache")
	engine.image["/root/.claude/history.jsonl"] = []byte("history")
	engine.image["/root/.claude/.credentials.json.lock"] = []byte("lock")
	ctx := context.Background()
	first := useLayers(t, srv, "derek", "credential:claude")
	login, _ := st.Login(first.Logins[key])
	if len(login.Files) != 2 || login.Files["/root/.claude/settings.json"] == nil || login.Files["/root/.claude/.credentials.json"] == nil {
		t.Fatalf("login 1 files = %v; expected the two kept files only", keys(login.Files))
	}
	// The agent rotates the token into a new file and drops the old one.
	capsule := engine.capsule(first.Runtime.ContainerID)
	delete(capsule, "/root/.claude/settings.json")
	capsule["/root/.claude/.credentials.json"] = []byte("token-2")
	capsule["/root/.claude/projects/notes.md"] = []byte("notes")
	srv.captureLoginState(ctx, first)
	login, _ = st.Login(first.Logins[key])
	if login.Files["/root/.claude/settings.json"] != nil || string(login.Files["/root/.claude/.credentials.json"]) != "token-2" || login.Files["/root/.claude/projects/notes.md"] == nil || login.Files["/root/.claude/cache/big.bin"] != nil {
		t.Fatalf("login 1 after the turn = %v", keys(login.Files))
	}
}

func keys(files map[string][]byte) []string {
	var out []string
	for path := range files {
		out = append(out, path)
	}
	return out
}

// A runner's report that tracked files changed is kept in the held login
// at once: nothing waits for the end of a turn.
func TestRunnerReportKeepsLoginOnChange(t *testing.T) {
	const path = "/root/.claude/.credentials.json"
	srv, st, engine, key := newLoginTestServer(t, domain.ArtifactCredential, path)
	first := useLayers(t, srv, "derek", "credential:claude")
	engine.capsule(first.Runtime.ContainerID)[path] = []byte("token-9")
	srv.trackedFilesChanged(first.Runtime.ClientID, *first.Runtime, capsule.TrackedSelection{Paths: []string{path}}, map[string][]byte{path: []byte("token-9"), "/root/elsewhere": []byte("x")})
	login, _ := st.Login(first.Logins[key])
	if string(login.Files[path]) != "token-9" || login.Files["/root/elsewhere"] != nil {
		t.Fatalf("login after the report = %v", keys(login.Files))
	}
}

// A report from a watcher that still reads the old selection (the folder
// was chosen after the capsule started) never drops the folder's files.
func TestStaleWatcherReportDropsNothing(t *testing.T) {
	const path = "/root/.claude/.credentials.json"
	srv, st, engine, key := newLoginTestServer(t, domain.ArtifactCredential, path)
	first := useLayers(t, srv, "derek", "credential:claude")
	var credential domain.Artifact
	for _, artifact := range st.Snapshot().Artifacts {
		if artifact.Kind == domain.ArtifactCredential {
			credential = artifact
		}
	}
	// The person now keeps the whole folder; the login gets a second file
	// from the folder at the next full read.
	if _, err := st.SetArtifactTracked(credential.ID, []string{"/root/.claude/"}, nil); err != nil {
		t.Fatal(err)
	}
	engine.capsule(first.Runtime.ContainerID)["/root/.claude/settings.json"] = []byte("{}")
	srv.captureLoginState(context.Background(), first)
	if login, _ := st.Login(first.Logins[key]); len(login.Files) != 2 {
		t.Fatalf("login after the full read = %v", keys(login.Files))
	}
	// The old watcher reports with its old selection: the one file.
	srv.trackedFilesChanged(first.Runtime.ClientID, *first.Runtime, capsule.TrackedSelection{Paths: []string{path}}, map[string][]byte{path: []byte("token-3")})
	login, _ := st.Login(first.Logins[key])
	if string(login.Files[path]) != "token-3" || login.Files["/root/.claude/settings.json"] == nil {
		t.Fatalf("a stale report changed the login to %v", keys(login.Files))
	}
}
