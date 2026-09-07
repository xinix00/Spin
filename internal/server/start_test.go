package server

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
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
	codex := toolLayer("codex")
	codex.Enables = []domain.Enablement{{Name: "acp", Command: "codex-acp"}}
	recording, starting := startLayer(t, srv, "derek", codex)
	if starting == nil || starting.Status != "running" || recording.Runtime != nil {
		t.Fatalf("slow RECORD = %+v / %+v", recording, starting)
	}
	recordingID := recording.ID
	if _, _, err := srv.createCapsuleRecording(toolLayer("node").request(t, srv, "derek")); err == nil || !strings.Contains(err.Error(), "still starting") {
		t.Fatalf("second RECORD while one starts = %v", err)
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
	if ended := saveLayer(t, srv, "derek"); ended.Slot != "tool:codex" {
		t.Fatalf("END after a slow start = %+v", ended)
	}

	// A start that fails leaves no open recording behind.
	failing := &slowStartEngine{testEngine: &testEngine{}, release: make(chan struct{}), fail: errors.New("runner has no room")}
	close(failing.release)
	failingServer := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), failing, ServerOptions{DisableAuthentication: true})
	if _, _, err := failingServer.createCapsuleRecording(toolLayer("node").request(t, failingServer, "derek")); err == nil {
		t.Fatal("RECORD with a failing start did not report the failure")
	}
	if _, err := st.OpenRecording("derek"); err == nil {
		t.Fatal("a failed start left the recording open")
	}
}

// CANCEL RECORD during a start stops the job and cancels the recording; the
// operator is free to record again at once.
func TestCancelStopsARunningStart(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &slowStartEngine{testEngine: &testEngine{}, release: make(chan struct{})}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	srv.startWait = 50 * time.Millisecond
	recording, starting := startLayer(t, srv, "derek", toolLayer("codex"))
	if starting == nil {
		t.Fatalf("slow RECORD = %+v", recording)
	}
	recordingID := recording.ID
	// The engine ignores the context; the server must still stop waiting.
	srv.startCancelWait = 100 * time.Millisecond
	cancelled, err := cancelLayer(srv, "derek")
	if err != nil || cancelled.Status != domain.RecordingCancelled {
		t.Fatalf("CANCEL during start = %+v, %v", cancelled, err)
	}
	if _, err := st.OpenRecording("derek"); err == nil {
		t.Fatal("cancelled start left the recording open")
	}
	close(engine.release)
	deadline := time.Now().Add(5 * time.Second)
	for srv.startInProgress(recordingID) && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if srv.startInProgress(recordingID) {
		t.Fatal("start job kept running after the recording was cancelled")
	}
	if engine.testEngine.cancelled != 1 {
		t.Fatalf("capsule that came up after the cancel was not removed: cancelled=%d", engine.testEngine.cancelled)
	}
	if again := recordLayer(t, srv, "derek", toolLayer("codex")); again.Runtime == nil {
		t.Fatalf("RECORD after a cancelled start = %+v", again)
	}
}

// A server that comes back while a capsule was starting resumes the start:
// the recording is durable and the job is not, and nothing else could finish
// it. The browser finds the job under the same URL.
func TestServerResumesStartsAfterRestart(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	stuck := &slowStartEngine{testEngine: &testEngine{}, release: make(chan struct{})}
	first := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), stuck, ServerOptions{DisableAuthentication: true})
	first.startWait = 50 * time.Millisecond
	recording, starting := startLayer(t, first, "derek", toolLayer("codex"))
	if starting == nil {
		t.Fatalf("slow RECORD = %+v", recording)
	}
	recordingID := recording.ID
	// "Restart": a new server over the same state, with a runner that answers.
	second := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true})
	status := func() domain.StartStatus {
		t.Helper()
		recorder := httptest.NewRecorder()
		second.Handler().ServeHTTP(recorder, httptest.NewRequest(http.MethodGet, "/api/recordings/"+recordingID+"/start", nil))
		if recorder.Code != http.StatusOK {
			t.Fatalf("start status=%d body=%s", recorder.Code, recorder.Body.String())
		}
		var start domain.StartStatus
		if err := json.Unmarshal(recorder.Body.Bytes(), &start); err != nil {
			t.Fatal(err)
		}
		return start
	}
	deadline := time.Now().Add(5 * time.Second)
	var start domain.StartStatus
	for start = status(); start.Status == "running" && time.Now().Before(deadline); start = status() {
		time.Sleep(10 * time.Millisecond)
	}
	if start.Status != "done" || start.Recording == nil || start.Recording.Runtime == nil {
		t.Fatalf("resumed start = %+v", start)
	}
	open, err := st.OpenRecording("derek")
	if err != nil || open.ID != recordingID || open.Runtime == nil || open.Runtime.ContainerID == "" {
		t.Fatalf("open recording after the resumed start = %+v, %v", open, err)
	}
	if ended := saveLayer(t, second, "derek"); ended.ID == "" {
		t.Fatalf("END after a resumed start = %+v", ended)
	}
}
