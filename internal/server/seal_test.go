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
	codex := toolLayer("codex")
	codex.Enables = []domain.Enablement{{Name: "acp", Command: "codex-acp"}}
	recordingID := recordLayer(t, srv, "derek", codex).ID

	artifact, ending, err := endLayer(srv, "derek")
	if err != nil {
		t.Fatal(err)
	}
	if artifact.ID != "" || ending == nil || ending.Status != "running" || ending.RecordingID != recordingID {
		t.Fatalf("slow END RECORD = %+v / %+v", artifact, ending)
	}
	_, again, err := endLayer(srv, "derek")
	if err != nil || again == nil || !again.StartedAt.Equal(ending.StartedAt) {
		t.Fatalf("repeated END RECORD did not join the running seal: %+v, %v", again, err)
	}
	if _, err := cancelLayer(srv, "derek"); err == nil {
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
	if artifacts := st.Snapshot().Artifacts; len(artifacts) != 1 || artifacts[0].ID != seal.Artifact.ID {
		t.Fatalf("artifact after seal: %+v", artifacts)
	}

	// The REST form of END answers the same way.
	quick := &testEngine{}
	quickServer := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), quick, ServerOptions{DisableAuthentication: true})
	second := recordLayer(t, quickServer, "derek", toolLayer("node"))
	recorder := httptest.NewRecorder()
	request := httptest.NewRequest(http.MethodPost, "/api/recordings/"+second.ID+"/end", strings.NewReader(`{"actor":"derek"}`))
	request.Header.Set("Content-Type", "application/json")
	quickServer.Handler().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusCreated {
		t.Fatalf("quick REST END status=%d body=%s", recorder.Code, recorder.Body.String())
	}
}
