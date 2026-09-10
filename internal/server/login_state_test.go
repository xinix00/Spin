package server

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// trackedTestEngine is the test engine with a file per capsule, the way a
// runner reads and writes tracked files in a container.
type trackedTestEngine struct {
	testEngine
	files      map[string]map[string][]byte // container -> path -> content
	failWrites map[string]bool
}

func (e *trackedTestEngine) ReadTrackedFiles(_ context.Context, runtime domain.CapsuleRuntime, paths []string) (map[string][]byte, error) {
	out := map[string][]byte{}
	for _, path := range paths {
		if data, ok := e.files[runtime.ContainerID][path]; ok {
			out[path] = append([]byte(nil), data...)
		}
	}
	return out, nil
}

func (e *trackedTestEngine) WriteTrackedFiles(_ context.Context, runtime domain.CapsuleRuntime, files map[string][]byte) error {
	if e.failWrites[runtime.ContainerID] {
		return errors.New("runner away")
	}
	if e.files[runtime.ContainerID] == nil {
		e.files[runtime.ContainerID] = map[string][]byte{}
	}
	for path, data := range files {
		e.files[runtime.ContainerID][path] = append([]byte(nil), data...)
	}
	return nil
}

// Two capsules of one credential layer on two runners keep one token: a
// change in one goes to the server and into the other at once, a capsule
// that still has what it was given never overwrites a newer copy, and a
// capsule that missed a share gets it at its next turn.
func TestTrackedFilesAreSharedBetweenRunningCapsules(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	const path = "/root/.claude/.credentials.json"
	engine := &trackedTestEngine{files: map[string]map[string][]byte{}, failWrites: map[string]bool{}}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	layers := buildLayers(t, srv, "derek", gitLayer(), layerSpec{Kind: domain.ArtifactCredential, Name: "claude", Scope: domain.ScopeUser, From: "tool:git", Install: "login"})
	if _, err := st.SetArtifactTrackedPaths(layers[1].ID, []string{path}); err != nil {
		t.Fatal(err)
	}
	first := useLayers(t, srv, "derek", "credential:claude")
	second := useLayers(t, srv, "derek", "credential:claude")
	if first.Runtime == nil || second.Runtime == nil || first.Runtime.ContainerID == second.Runtime.ContainerID {
		t.Fatalf("compositions = %+v / %+v", first.Runtime, second.Runtime)
	}
	// Both capsules start with the token the layer holds.
	engine.files[first.Runtime.ContainerID] = map[string][]byte{path: []byte("token-1")}
	engine.files[second.Runtime.ContainerID] = map[string][]byte{path: []byte("token-1")}
	ctx := context.Background()
	srv.restoreLoginState(ctx, first)
	srv.restoreLoginState(ctx, second)
	if _, kept := st.LoginState("derek/credential:claude"); kept {
		t.Fatal("nothing changed yet, nothing should be kept")
	}
	// The first capsule rotates the token: the server keeps it and the
	// second capsule gets it.
	engine.files[first.Runtime.ContainerID][path] = []byte("token-2")
	srv.captureLoginState(ctx, first)
	state, kept := st.LoginState("derek/credential:claude")
	if !kept || string(state.Files[path]) != "token-2" {
		t.Fatalf("kept after the first rotation = %q, %v", state.Files[path], kept)
	}
	if got := string(engine.files[second.Runtime.ContainerID][path]); got != "token-2" {
		t.Fatalf("the second capsule holds %q; the rotation was not shared", got)
	}
	// The second capsule ends its turn with what it was given: the server
	// copy stays.
	srv.captureLoginState(ctx, second)
	if state, _ := st.LoginState("derek/credential:claude"); string(state.Files[path]) != "token-2" {
		t.Fatalf("the second capsule overwrote the token with %q", state.Files[path])
	}
	// The second capsule's runner is away when the first rotates again:
	// the share fails, and the next turn of the second capsule brings the
	// token in.
	engine.failWrites[second.Runtime.ContainerID] = true
	engine.files[first.Runtime.ContainerID][path] = []byte("token-3")
	srv.captureLoginState(ctx, first)
	if got := string(engine.files[second.Runtime.ContainerID][path]); got != "token-2" {
		t.Fatalf("a failed share changed the second capsule to %q", got)
	}
	engine.failWrites[second.Runtime.ContainerID] = false
	srv.captureLoginState(ctx, second)
	if got := string(engine.files[second.Runtime.ContainerID][path]); got != "token-3" {
		t.Fatalf("the second capsule holds %q after its turn; expected the server's token-3", got)
	}
	if state, _ := st.LoginState("derek/credential:claude"); string(state.Files[path]) != "token-3" {
		t.Fatalf("the server holds %q; expected token-3", state.Files[path])
	}
	// The second capsule rotates now; the first follows.
	engine.files[second.Runtime.ContainerID][path] = []byte("token-4")
	srv.captureLoginState(ctx, second)
	if got := string(engine.files[first.Runtime.ContainerID][path]); got != "token-4" {
		t.Fatalf("the first capsule holds %q; expected token-4", got)
	}
	// A new capsule starts with the newest token.
	third := useLayers(t, srv, "derek", "credential:claude")
	engine.files[third.Runtime.ContainerID] = map[string][]byte{path: []byte("token-1")}
	srv.restoreLoginState(ctx, third)
	if got := string(engine.files[third.Runtime.ContainerID][path]); got != "token-4" {
		t.Fatalf("a new capsule starts with %q; expected token-4", got)
	}
}
