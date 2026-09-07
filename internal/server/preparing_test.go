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

// A launch that fails leaves its reason for the browser and the phase queued;
// the next sweep launches again and clears the failure once it starts.
func TestFailedLaunchIsReportedAndSweptAgain(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &acpTestEngine{}
	engine.materializeErr = errors.New("runner has no disk left")
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "easyacp", RemoteURL: "https://github.com/derek/easyacp.git", DefaultRef: "main", CredentialScope: domain.CredentialScopeUser})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.CreateGitAccount(domain.CreateGitAccountRequest{Operator: "derek", Provider: "github", Host: "github.com", Login: "derek", AccessToken: "github-secret"}); err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Code", Phases: []domain.WorkflowPhase{{
		ID: "develop", Name: "Ontwikkelen", Instructions: "Bouw het", AllowChanges: true,
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Feature", Objective: "Werkend", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	preparation := func() *sessionPreparation {
		t.Helper()
		for _, item := range srv.sessionPreparations() {
			if item.SessionID == created.Session.ID {
				return &item
			}
		}
		return nil
	}
	launchAndWait := func() {
		t.Helper()
		srv.launchQueuedWorkflowPhases("test")
		deadline := time.Now().Add(5 * time.Second)
		for {
			srv.jobLaunchMu.Lock()
			_, running := srv.jobLaunching[created.Session.ID]
			srv.jobLaunchMu.Unlock()
			if !running {
				return
			}
			if time.Now().After(deadline) {
				t.Fatal("launch did not finish")
			}
			time.Sleep(5 * time.Millisecond)
		}
	}

	launchAndWait()
	got := preparation()
	if got == nil || got.Failure == nil || got.Failure.Error == "" || !strings.Contains(got.Failure.Error, "no disk left") {
		t.Fatalf("preparation after a failed launch = %+v", got)
	}

	// The runner recovered; the sweep launches again and the old failure is
	// gone. The test engine has no ACP, so the phase now fails one step
	// later, and that is the failure on the card.
	engine.materializeErr = nil
	launchAndWait()
	if got := preparation(); got == nil || got.Failure == nil || strings.Contains(got.Failure.Error, "no disk left") || !strings.Contains(got.Failure.Error, "start workflow ACP") {
		t.Fatalf("failure after the second launch = %+v", got)
	}
	// The failed attempt counted too: exactly one more materialization.
	if engine.materialized != 2 {
		t.Fatalf("materialized %d times after the sweep, want 2", engine.materialized)
	}

	// Once the phase moves on, the old reason is no longer shown.
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	if got := preparation(); got != nil {
		t.Fatalf("stale failure shown for a running phase: %+v", got)
	}
}
