package server

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// slowStartEngine holds the capsule start until the test lets go, the way a
// runner does while a base image travels out of the archive.
type slowStartEngine struct {
	*testEngine
	release chan struct{}
	fail    error
}

func (e *slowStartEngine) StartRecording(ctx context.Context, recording domain.Recording, parents []domain.Artifact) (domain.CapsuleRuntime, error) {
	<-e.release
	if e.fail != nil {
		return domain.CapsuleRuntime{}, e.fail
	}
	return e.testEngine.StartRecording(ctx, recording, parents)
}

// RECORD answers with the live recording when the capsule is quick, and with
// a followable job when it is not; the recording exists without a runtime
// meanwhile, cannot be cancelled underneath the job, and ends up live.
func TestRecordAnswersWithProgressWhenStartingTakesLong(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &slowStartEngine{testEngine: &testEngine{}, release: make(chan struct{})}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	srv.startWait = 50 * time.Millisecond
	run := func(line string) (domain.CommandResponse, error) {
		return srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line})
	}
	started, err := run("RECORD tool:codex --scope=global --enable=acp --command=codex-acp")
	if err != nil {
		t.Fatal(err)
	}
	if started.Start == nil || started.Start.Status != "running" || started.Recording == nil || started.Recording.Runtime != nil {
		t.Fatalf("slow RECORD = %+v", started)
	}
	recordingID := started.Recording.ID
	if _, err := run("CANCEL RECORD"); err == nil {
		t.Fatal("cancelled a recording whose capsule is still starting")
	}
	status := func() domain.StartStatus {
		t.Helper()
		recorder := httptest.NewRecorder()
		srv.Handler().ServeHTTP(recorder, httptest.NewRequest(http.MethodGet, "/api/recordings/"+recordingID+"/start", nil))
		if recorder.Code != http.StatusOK {
			t.Fatalf("start status=%d body=%s", recorder.Code, recorder.Body.String())
		}
		var start domain.StartStatus
		if err := json.Unmarshal(recorder.Body.Bytes(), &start); err != nil {
			t.Fatal(err)
		}
		return start
	}
	if start := status(); start.Status != "running" {
		t.Fatalf("start while the capsule comes up = %+v", start)
	}

	close(engine.release)
	deadline := time.Now().Add(5 * time.Second)
	var start domain.StartStatus
	for start = status(); start.Status == "running" && time.Now().Before(deadline); start = status() {
		time.Sleep(10 * time.Millisecond)
	}
	if start.Status != "done" || start.Recording == nil || start.Recording.Runtime == nil || start.Recording.Runtime.ContainerID == "" {
		t.Fatalf("finished start = %+v", start)
	}
	open, err := st.OpenRecording("derek")
	if err != nil || open.ID != recordingID || open.Runtime == nil {
		t.Fatalf("open recording after start = %+v, %v", open, err)
	}
	if ended, err := run("END RECORD"); err != nil || ended.Artifact == nil || ended.Artifact.Slot != "tool:codex" {
		t.Fatalf("END after a slow start = %+v, %v", ended, err)
	}

	// A start that fails leaves no open recording behind.
	failing := &slowStartEngine{testEngine: &testEngine{}, release: make(chan struct{}), fail: errors.New("runner has no room")}
	close(failing.release)
	failingServer := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), failing, ServerOptions{DisableAuthentication: true})
	if _, err := failingServer.runCommand(domain.CommandRequest{Operator: "derek", Line: "RECORD tool:node --scope=global"}); err == nil {
		t.Fatal("RECORD with a failing start did not report the failure")
	}
	if _, err := st.OpenRecording("derek"); err == nil {
		t.Fatal("a failed start left the recording open")
	}
}
