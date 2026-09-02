package worker_test

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"log/slog"
	"net/http/httptest"
	"sync"
	"testing"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	spinserver "easyacp/internal/server"
	"easyacp/internal/store"
	"easyacp/internal/worker"
)

type fakeRunnerEngine struct {
	name   string
	mu     sync.Mutex
	calls  []string
	images map[string][]byte
}

func (e *fakeRunnerEngine) Info() domain.CapsuleEngineInfo {
	return domain.CapsuleEngineInfo{Driver: "fake", Available: true, FilesystemSnapshots: true}
}

func (e *fakeRunnerEngine) StartRecording(_ context.Context, recording domain.Recording, _ []domain.Artifact) (domain.CapsuleRuntime, error) {
	e.called("start:" + recording.ID)
	return domain.CapsuleRuntime{Driver: "fake", ContainerID: e.name + ":" + recording.ID, Status: "recording"}, nil
}

func (e *fakeRunnerEngine) Execute(_ context.Context, _ domain.Recording, input string) (capsule.Execution, error) {
	e.called("execute:" + input)
	return capsule.Execution{Output: e.name, ExitCode: 0}, nil
}

func (e *fakeRunnerEngine) Seal(_ context.Context, recording domain.Recording) (domain.CapsuleSnapshot, error) {
	e.called("seal:" + recording.ID)
	return domain.CapsuleSnapshot{Driver: "fake", Ref: recording.ID, Digest: recording.ID, Restorable: true}, nil
}

func (e *fakeRunnerEngine) Cancel(_ context.Context, recording domain.Recording) error {
	e.called("cancel:" + recording.ID)
	return nil
}

func (e *fakeRunnerEngine) Materialize(_ context.Context, composition domain.Composition, artifacts []domain.Artifact) (domain.CapsuleRuntime, error) {
	e.called("materialize:" + composition.ID)
	e.mu.Lock()
	defer e.mu.Unlock()
	for _, artifact := range artifacts {
		if artifact.Snapshot.Ref != "" && len(e.images[artifact.Snapshot.Ref]) == 0 {
			return domain.CapsuleRuntime{}, fmt.Errorf("image %s is missing on %s", artifact.Snapshot.Ref, e.name)
		}
	}
	return domain.CapsuleRuntime{Driver: "fake", ContainerID: e.name + ":" + composition.ID, Status: "ready"}, nil
}

func (e *fakeRunnerEngine) Stop(_ context.Context, runtime domain.CapsuleRuntime) error {
	e.called("stop:" + runtime.ContainerID)
	return nil
}

func (e *fakeRunnerEngine) called(value string) {
	e.mu.Lock()
	e.calls = append(e.calls, value)
	e.mu.Unlock()
}

func (e *fakeRunnerEngine) ExportSnapshot(_ context.Context, snapshot domain.CapsuleSnapshot, destination io.Writer) error {
	e.mu.Lock()
	data := append([]byte(nil), e.images[snapshot.Ref]...)
	e.mu.Unlock()
	if len(data) == 0 {
		return fmt.Errorf("image %s is missing on %s", snapshot.Ref, e.name)
	}
	_, err := destination.Write(data)
	return err
}

func (e *fakeRunnerEngine) ImportSnapshot(_ context.Context, snapshot domain.CapsuleSnapshot, source io.Reader) error {
	data, err := io.ReadAll(source)
	if err != nil {
		return err
	}
	e.mu.Lock()
	if e.images == nil {
		e.images = map[string][]byte{}
	}
	e.images[snapshot.Ref] = data
	e.mu.Unlock()
	return nil
}

func (e *fakeRunnerEngine) RemoveSnapshot(_ context.Context, snapshot domain.CapsuleSnapshot) error {
	e.mu.Lock()
	delete(e.images, snapshot.Ref)
	e.mu.Unlock()
	return nil
}

func TestRemoteEngineRoundRobinsAndRetainsAffinityAcrossReconnect(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	broker := worker.NewBroker(st, logger)
	remote := worker.NewRemoteEngine(broker)
	const token = "runner-test-token-with-enough-entropy"
	handler := spinserver.NewWithOptions(st, logger, remote, spinserver.ServerOptions{
		DisableAuthentication: true, WorkerToken: token, RunnerBroker: broker,
	}).Handler()
	server := httptest.NewServer(handler)
	defer server.Close()

	engineA := &fakeRunnerEngine{name: "runner-a", images: map[string][]byte{}}
	engineB := &fakeRunnerEngine{name: "runner-b", images: map[string][]byte{}}
	ctxA, cancelA := context.WithCancel(context.Background())
	ctxB, cancelB := context.WithCancel(context.Background())
	defer cancelB()
	runWorker(t, ctxA, worker.Config{ServerURL: server.URL, InstanceID: "stable-a", Name: "A", Token: token, Engine: engineA})
	runWorker(t, ctxB, worker.Config{ServerURL: server.URL, InstanceID: "stable-b", Name: "B", Token: token, Engine: engineB})
	waitFor(t, func() bool { return onlineClients(st) == 2 })

	callCtx, callCancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer callCancel()
	recordingA := domain.Recording{ID: "one"}
	runtimeA, err := remote.StartRecording(callCtx, recordingA, nil)
	if err != nil {
		t.Fatal(err)
	}
	recordingA.Runtime = &runtimeA
	recordingB := domain.Recording{ID: "two"}
	runtimeB, err := remote.StartRecording(callCtx, recordingB, nil)
	if err != nil {
		t.Fatal(err)
	}
	if runtimeA.ClientID == "" || runtimeB.ClientID == "" || runtimeA.ClientID == runtimeB.ClientID {
		t.Fatalf("round-robin placement = %q, %q", runtimeA.ClientID, runtimeB.ClientID)
	}
	ownerOfB, err := remote.Execute(callCtx, domain.Recording{ID: recordingB.ID, Runtime: &runtimeB}, "locate snapshot")
	if err != nil {
		t.Fatal(err)
	}
	sourceEngine := engineA
	if ownerOfB.Output == "runner-b" {
		sourceEngine = engineB
	}
	sourceEngine.mu.Lock()
	sourceEngine.images["fake:snapshot"] = []byte("opaque snapshot bytes")
	sourceEngine.mu.Unlock()
	recording, err := st.CreateRecording(domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "portable", Scope: domain.ScopeGlobal})
	if err != nil {
		t.Fatal(err)
	}
	artifact, err := st.EndRecording(recording.ID, domain.EndRecordingRequest{Actor: "derek", Snapshot: domain.CapsuleSnapshot{
		Driver: "fake", ClientID: runtimeB.ClientID, Ref: "fake:snapshot", Digest: "sha256:portable", Restorable: true,
	}})
	if err != nil {
		t.Fatal(err)
	}
	materialized, err := remote.Materialize(callCtx, domain.Composition{ID: "portable-composition"}, []domain.Artifact{artifact})
	if err != nil {
		t.Fatal(err)
	}
	if materialized.ClientID != runtimeA.ClientID {
		t.Fatalf("next round-robin target = %s, want %s", materialized.ClientID, runtimeA.ClientID)
	}
	storedArtifact, err := st.Artifact(artifact.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(storedArtifact.Snapshot.ReplicaClientIDs) != 1 || storedArtifact.Snapshot.ReplicaClientIDs[0] != runtimeA.ClientID {
		t.Fatalf("snapshot replicas = %+v", storedArtifact.Snapshot.ReplicaClientIDs)
	}
	targetEngine := engineA
	if ownerOfB.Output == "runner-a" {
		targetEngine = engineB
	}
	targetEngine.mu.Lock()
	transferred := append([]byte(nil), targetEngine.images["fake:snapshot"]...)
	targetEngine.mu.Unlock()
	if !bytes.Equal(transferred, []byte("opaque snapshot bytes")) {
		t.Fatalf("transferred snapshot = %q", transferred)
	}
	if err := remote.RemoveSnapshot(callCtx, storedArtifact.Snapshot); err != nil {
		t.Fatal(err)
	}
	for _, engine := range []*fakeRunnerEngine{engineA, engineB} {
		engine.mu.Lock()
		_, retained := engine.images["fake:snapshot"]
		engine.mu.Unlock()
		if retained {
			t.Fatalf("runner %s retained removed replica", engine.name)
		}
	}

	execution, err := remote.Execute(callCtx, recordingA, "before reconnect")
	if err != nil {
		t.Fatal(err)
	}
	expectedRunner := execution.Output
	cancelAForRuntime := cancelA
	reconnectEngine := engineA
	reconnectID := "stable-a"
	reconnectName := "A"
	if expectedRunner == "runner-b" {
		cancelAForRuntime = cancelB
		reconnectEngine = engineB
		reconnectID = "stable-b"
		reconnectName = "B"
	}
	cancelAForRuntime()
	waitFor(t, func() bool { return clientStatus(st, runtimeA.ClientID) == "offline" })

	result := make(chan capsule.Execution, 1)
	errors := make(chan error, 1)
	go func() {
		execution, err := remote.Execute(callCtx, recordingA, "after reconnect")
		result <- execution
		errors <- err
	}()
	select {
	case <-result:
		t.Fatal("affinity call completed while its runner was offline")
	case <-time.After(150 * time.Millisecond):
	}

	reconnectCtx, reconnectCancel := context.WithCancel(context.Background())
	defer reconnectCancel()
	runWorker(t, reconnectCtx, worker.Config{ServerURL: server.URL, InstanceID: reconnectID, Name: reconnectName, Token: token, Engine: reconnectEngine})
	select {
	case execution := <-result:
		if err := <-errors; err != nil {
			t.Fatal(err)
		}
		if execution.Output != expectedRunner {
			t.Fatalf("reconnected call ran on %q, want %q", execution.Output, expectedRunner)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("affinity call did not resume after runner reconnect")
	}
}

func runWorker(t *testing.T, ctx context.Context, config worker.Config) {
	t.Helper()
	go func() {
		if err := worker.New(config, slog.New(slog.NewTextHandler(io.Discard, nil))).Run(ctx); err != nil && ctx.Err() == nil {
			t.Errorf("run worker %s: %v", config.Name, err)
		}
	}()
}

func waitFor(t *testing.T, condition func() bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if condition() {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("condition was not met")
}

func onlineClients(st *store.Store) int {
	count := 0
	for _, client := range st.Snapshot().Clients {
		if client.Status == "online" {
			count++
		}
	}
	return count
}

func clientStatus(st *store.Store, id string) string {
	for _, client := range st.Snapshot().Clients {
		if client.ID == id {
			return client.Status
		}
	}
	return fmt.Sprintf("missing:%s", id)
}
