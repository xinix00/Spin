package server

import (
	"context"
	"encoding/json"
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

// slowSealEngine holds the commit until the test lets go, the way a runner
// exporting a gigabyte does.
type slowSealEngine struct {
	*testEngine
	release chan struct{}
}

func (e *slowSealEngine) Seal(ctx context.Context, recording domain.Recording) (domain.CapsuleSnapshot, error) {
	<-e.release
	return e.testEngine.Seal(ctx, recording)
}

// END RECORD answers with the artifact when the seal is quick, and with a
// followable job when it is not; the recording stays open, cannot be cancelled
// while saving, and a repeated END RECORD joins the running seal.
func TestEndRecordAnswersWithProgressWhenSealingTakesLong(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &slowSealEngine{testEngine: &testEngine{}, release: make(chan struct{})}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	srv.sealWait = 50 * time.Millisecond
	run := func(line string) (domain.CommandResponse, error) {
		return srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line})
	}
	started, err := run("RECORD tool:codex --scope=global --enable=acp --command=codex-acp")
	if err != nil || started.Recording == nil {
		t.Fatalf("record: %+v, %v", started, err)
	}
	recordingID := started.Recording.ID

	ending, err := run("END RECORD")
	if err != nil {
		t.Fatal(err)
	}
	if ending.Artifact != nil || ending.Seal == nil || ending.Seal.Status != "running" || ending.Seal.RecordingID != recordingID {
		t.Fatalf("slow END RECORD = %+v", ending)
	}
	again, err := run("END RECORD")
	if err != nil || again.Seal == nil || !again.Seal.StartedAt.Equal(ending.Seal.StartedAt) {
		t.Fatalf("repeated END RECORD did not join the running seal: %+v, %v", again, err)
	}
	if _, err := run("CANCEL RECORD"); err == nil {
		t.Fatal("cancelled a recording that is being saved")
	}
	status := func() domain.SealStatus {
		t.Helper()
		recorder := httptest.NewRecorder()
		srv.Handler().ServeHTTP(recorder, httptest.NewRequest(http.MethodGet, "/api/recordings/"+recordingID+"/seal", nil))
		if recorder.Code != http.StatusOK {
			t.Fatalf("seal status=%d body=%s", recorder.Code, recorder.Body.String())
		}
		var seal domain.SealStatus
		if err := json.Unmarshal(recorder.Body.Bytes(), &seal); err != nil {
			t.Fatal(err)
		}
		return seal
	}
	if seal := status(); seal.Status != "running" || seal.Stage != "commit" {
		t.Fatalf("seal while committing = %+v", seal)
	}
	if open, err := st.OpenRecording("derek"); err != nil || open.ID != recordingID {
		t.Fatalf("recording closed before the seal finished: %+v, %v", open, err)
	}

	close(engine.release)
	deadline := time.Now().Add(5 * time.Second)
	var seal domain.SealStatus
	for seal = status(); seal.Status == "running" && time.Now().Before(deadline); seal = status() {
		time.Sleep(10 * time.Millisecond)
	}
	if seal.Status != "done" || seal.Artifact == nil || seal.Artifact.Slot != "tool:codex" {
		t.Fatalf("finished seal = %+v", seal)
	}
	if _, err := st.OpenRecording("derek"); err == nil {
		t.Fatal("recording still open after the seal finished")
	}
	if listed, err := run("LIST tool"); err != nil || len(listed.Artifacts) != 1 || listed.Artifacts[0].ID != seal.Artifact.ID {
		t.Fatalf("artifact after seal: %+v, %v", listed.Artifacts, err)
	}

	// The REST form of END answers the same way.
	quick := &testEngine{}
	quickServer := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), quick, ServerOptions{DisableAuthentication: true})
	second, err := quickServer.runCommand(domain.CommandRequest{Operator: "derek", Line: "RECORD tool:node --scope=global"})
	if err != nil {
		t.Fatal(err)
	}
	recorder := httptest.NewRecorder()
	request := httptest.NewRequest(http.MethodPost, "/api/recordings/"+second.Recording.ID+"/end", strings.NewReader(`{"actor":"derek"}`))
	request.Header.Set("Content-Type", "application/json")
	quickServer.Handler().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusCreated {
		t.Fatalf("quick REST END status=%d body=%s", recorder.Code, recorder.Body.String())
	}
}
