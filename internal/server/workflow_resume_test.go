package server

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"testing"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// acpTestEngine is a testEngine that also satisfies capsule.EnabledEngine, so a
// workflow phase takes the ordinary materialize-then-ACP path. Starting ACP
// itself fails: the phase is already marked running by then, which is the state
// this test observes.
type acpTestEngine struct{ testEngine }

func (e *acpTestEngine) StartEnabled(context.Context, domain.CapsuleRuntime, domain.Enablement) (capsule.EnabledProcess, error) {
	return nil, errors.New("no ACP in this test")
}

func TestQueuedWorkflowPhaseResumesWhenARunnerConnects(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &acpTestEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	for _, line := range []string{
		"RECORD tool:git --scope=global --enable=git", "install git", "END RECORD",
		"RECORD tool:agent --scope=global --from=tool:git --enable=acp --command=agent-acp", "install agent", "END RECORD",
	} {
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
	}
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

	// The launch that ran while no runner was connected gave up: the phase is
	// queued and nothing has been materialized for it.
	phaseStatus := func() domain.PhaseRunStatus {
		t.Helper()
		for _, run := range st.Snapshot().PhaseRuns {
			if run.ID == created.Session.PhaseRunID {
				return run.Status
			}
		}
		t.Fatalf("phase run %s disappeared", created.Session.PhaseRunID)
		return ""
	}
	if status := phaseStatus(); status != domain.PhaseRunQueued {
		t.Fatalf("phase status before a runner = %q, want queued", status)
	}
	if engine.materialized != 0 {
		t.Fatalf("materialized %d compositions before a runner connected", engine.materialized)
	}

	srv.resumeQueuedWorkflowPhases()

	deadline := time.Now().Add(10 * time.Second)
	for phaseStatus() != domain.PhaseRunRunning {
		if time.Now().After(deadline) {
			t.Fatalf("phase status after a runner connected = %q, want running", phaseStatus())
		}
		time.Sleep(5 * time.Millisecond)
	}
	if engine.materialized != 1 {
		t.Fatalf("materialized %d compositions, want 1", engine.materialized)
	}

	// A phase that is no longer queued is left alone.
	srv.resumeQueuedWorkflowPhases()
	time.Sleep(50 * time.Millisecond)
	if engine.materialized != 1 {
		t.Fatalf("re-resumed a running phase: materialized %d", engine.materialized)
	}
}
