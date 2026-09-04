package server

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"easyacp/internal/store"
	"easyacp/internal/worker"
)

// The remote engine is the only placement reporter Spin ships. A signature
// drift here would silently turn early reporting back off.
var _ placementReporter = (*worker.RemoteEngine)(nil)

func TestStateReportsSessionPreparationUntilTheLaunchFinishes(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true})
	preparing := func() []sessionPreparation {
		t.Helper()
		recorder := httptest.NewRecorder()
		srv.Handler().ServeHTTP(recorder, httptest.NewRequest(http.MethodGet, "/api/state", nil))
		if recorder.Code != http.StatusOK {
			t.Fatalf("state status=%d body=%s", recorder.Code, recorder.Body.String())
		}
		var state struct {
			Preparing []sessionPreparation `json:"preparing"`
		}
		if err := json.Unmarshal(recorder.Body.Bytes(), &state); err != nil {
			t.Fatal(err)
		}
		return state.Preparing
	}

	if got := preparing(); len(got) != 0 {
		t.Fatalf("preparing before any launch = %+v", got)
	}

	release, finished := make(chan struct{}), make(chan struct{})
	before := time.Now().UTC()
	if !srv.beginTrackedLaunch("ses_slow", nil, func(context.Context) {
		<-release
		close(finished)
	}) {
		t.Fatal("launch did not start")
	}
	// A second launch for the same Session is refused while one is in flight.
	if srv.beginTrackedLaunch("ses_slow", nil, func(context.Context) {}) {
		t.Fatal("started a second launch for the same Session")
	}

	got := preparing()
	if len(got) != 1 || got[0].SessionID != "ses_slow" || got[0].ClientID != "" || got[0].StartedAt.Before(before) {
		t.Fatalf("preparing while launching = %+v", got)
	}

	// The engine reports which runner took the work long before it is ready.
	srv.recordLaunchPlacement("ses_slow", "cli_laptop")
	if got := preparing(); len(got) != 1 || got[0].ClientID != "cli_laptop" {
		t.Fatalf("preparing after placement = %+v", got)
	}
	// Placement for a Session with no launch in flight is ignored.
	srv.recordLaunchPlacement("ses_unknown", "cli_laptop")
	if got := preparing(); len(got) != 1 {
		t.Fatalf("placement invented a preparation: %+v", got)
	}

	close(release)
	<-finished
	deadline := time.Now().Add(5 * time.Second)
	for len(preparing()) != 0 {
		if time.Now().After(deadline) {
			t.Fatalf("preparing after the launch finished = %+v", preparing())
		}
		time.Sleep(5 * time.Millisecond)
	}
}
