package server

import (
	"context"
	"io"
	"log/slog"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// appTestEngine runs app services in memory: it records what was asked and
// reports every service reachable on a fixed host port.
type appTestEngine struct {
	acpTestEngine
	mu       sync.Mutex
	started  []domain.AppService
	stopped  int
	sessions map[string]bool
}

func (e *appTestEngine) StartAppServices(_ context.Context, _ domain.CapsuleRuntime, sessionID string, services []domain.AppService) ([]domain.AppServiceRuntime, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.sessions == nil {
		e.sessions = map[string]bool{}
	}
	e.sessions[sessionID] = true
	e.started = append(e.started, services...)
	return appTestResults(services), nil
}

func appTestResults(services []domain.AppService) []domain.AppServiceRuntime {
	results := make([]domain.AppServiceRuntime, 0, len(services))
	for index, service := range services {
		result := domain.AppServiceRuntime{Service: service.Name, Status: "running", Host: "192.168.1.5", Reachable: true, Ports: map[string]int{}}
		for _, port := range service.Ports {
			result.Ports[itoa(port)] = 40000 + index
		}
		results = append(results, result)
	}
	return results
}

func (e *appTestEngine) StopAppServicesOn(_ context.Context, _, sessionID string) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.stopped++
	delete(e.sessions, sessionID)
	return nil
}

func (e *appTestEngine) AppServiceStatusOn(_ context.Context, _, sessionID string) ([]domain.AppServiceRuntime, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if !e.sessions[sessionID] {
		return nil, nil
	}
	return appTestResults(e.started), nil
}

func (e *appTestEngine) AppServiceLogsOn(_ context.Context, _, _, service string, _ int) (string, error) {
	return "hello from " + service, nil
}

func itoa(value int) string { return strconv.Itoa(value) }

// An expose phase starts the repository's services on its workspace and then
// waits for a person, with the addresses in the decision text; a decision
// stops the app with the workspace.
func TestExposePhaseStartsTheAppAndWaitsForAVerdict(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &appTestEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	for _, line := range []string{
		"RECORD tool:git --scope=global --enable=git", "install git", "END RECORD",
		"RECORD tool:agent --scope=global --from=tool:git --enable=acp --command=agent-acp", "install agent", "END RECORD",
	} {
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://github.com/derek/shop.git", DefaultRef: "main", CredentialScope: domain.CredentialScopePublic,
		Services: []domain.AppService{{Name: "web", Prepare: []string{"npm ci"}, Run: "npm start", Ports: []int{3000}, Env: "shop"}}})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Test", Phases: []domain.WorkflowPhase{{
		ID: "test", Name: "Testen", Executor: domain.WorkflowExecutorExpose,
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: "SELF"},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Objective: "Werkend", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	srv.launchQueuedWorkflowPhases("test")
	deadline := time.Now().Add(10 * time.Second)
	var question domain.WorkflowQuestion
	for question.ID == "" && time.Now().Before(deadline) {
		for _, candidate := range st.Snapshot().WorkflowQuestions {
			if candidate.SessionID == created.Session.ID && candidate.Status == "open" {
				question = candidate
			}
		}
		time.Sleep(10 * time.Millisecond)
	}
	if question.ID == "" {
		t.Fatalf("no decision opened for the expose phase; failures=%+v", srv.sessionPreparations())
	}
	if question.Kind != "approval" || !strings.Contains(question.Question, "test-app") || !strings.Contains(question.AgentDetail+question.Question, "http://192.168.1.5:40000") {
		t.Fatalf("expose decision = %+v", question)
	}
	if len(engine.started) != 1 || engine.started[0].Name != "web" || engine.started[0].Env != "shop" {
		t.Fatalf("started services = %+v", engine.started)
	}
	job := st.Snapshot().Jobs[0]
	if job.WorkflowStatus != domain.WorkflowPending {
		t.Fatalf("job status = %q, want pending", job.WorkflowStatus)
	}

	// The person accepts: the Job is done and the app goes with the workspace.
	if _, err := st.AnswerWorkflowQuestion(question.ID, "derek", "accept", ""); err != nil {
		t.Fatal(err)
	}
	srv.retireWorkflowCompositions(job.ID, "")
	if engine.stopped == 0 {
		t.Fatal("app services were not stopped with the workspace")
	}
}
