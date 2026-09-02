package server

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"strings"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

func TestGitPullRequestActionUsesJobOwnerProviderAccountInsteadOfRepositoryCreator(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	requests := 0
	httpClient := &http.Client{Transport: roundTripFunc(func(request *http.Request) (*http.Response, error) {
		requests++
		if request.Header.Get("Authorization") != "Bearer john-secret" {
			t.Fatalf("authorization = %q", request.Header.Get("Authorization"))
		}
		switch requests {
		case 1:
			if request.Method != http.MethodGet || request.URL.Path != "/repos/derek/easyacp/pulls" || !strings.Contains(request.URL.RawQuery, "head=derek%3Ajobs%2F") {
				t.Fatalf("PR lookup = %s %s", request.Method, request.URL.String())
			}
			return jsonResponse(http.StatusOK, `[]`), nil
		case 2:
			var payload map[string]string
			if err := json.NewDecoder(request.Body).Decode(&payload); err != nil {
				t.Fatal(err)
			}
			if payload["title"] != "Implementeer darkmode" || payload["body"] != "Werkende single switch met volledige Goal" || payload["base"] != "main" || !strings.HasPrefix(payload["head"], "jobs/implementeer-darkmode-") {
				t.Fatalf("PR payload = %+v", payload)
			}
			return jsonResponse(http.StatusCreated, `{"number":42,"html_url":"https://github.com/derek/easyacp/pull/42","title":"Implementeer darkmode"}`), nil
		default:
			t.Fatalf("unexpected GitHub request %d", requests)
			return nil, nil
		}
	})}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true, HTTPClient: httpClient})
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
	_, err = st.CreateGitAccount(domain.CreateGitAccountRequest{Operator: "derek", Provider: "github", Host: "github.com", Login: "derek", AccessToken: "repository-creator-secret"})
	if err != nil {
		t.Fatal(err)
	}
	_, err = st.CreateGitAccount(domain.CreateGitAccountRequest{Operator: "john", Provider: "github", Host: "github.com", Login: "john", AccessToken: "john-secret"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Publish", Phases: []domain.WorkflowPhase{{
		ID: "review", Name: "Review", Instructions: "Controleer het resultaat",
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Implementeer darkmode", Objective: "Werkende single switch met volledige Goal", Operator: "derek", Owner: "john", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.UpdateGitRepository(repository.Repository.ID, domain.UpdateGitRepositoryRequest{
		Operator: "derek", Name: "moved", RemoteURL: "https://github.com/elsewhere/moved.git", DefaultRef: "trunk", CredentialScope: domain.CredentialScopeUser,
	}); err != nil {
		t.Fatal(err)
	}
	if len(template.Phases) != 2 || template.Phases[1].ID != domain.WorkflowPullRequestPhaseID {
		t.Fatalf("Template has no automatic PR finalizer: %+v", template.Phases)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	advance, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "review akkoord")
	if err != nil || advance.NextSession == nil || advance.NextSession.Executor != domain.WorkflowExecutorAction {
		t.Fatalf("advance to automatic PR = %+v, error = %v", advance, err)
	}
	if advance.NextSession.Operator != "john" {
		t.Fatalf("PR action operator = %s, want john", advance.NextSession.Operator)
	}
	srv.launchWorkflowAction(context.Background(), advance.NextSession.ID)
	snapshot := st.Snapshot()
	if requests != 2 || len(snapshot.Compositions) != 0 || snapshot.Jobs[0].WorkflowStatus != domain.WorkflowDone {
		t.Fatalf("action state: requests=%d compositions=%d job=%+v", requests, len(snapshot.Compositions), snapshot.Jobs[0])
	}
	var run domain.PhaseRun
	for _, candidate := range snapshot.PhaseRuns {
		if candidate.PhaseID == domain.WorkflowPullRequestPhaseID {
			run = candidate
			break
		}
	}
	if run.ActionResult == nil || run.ActionResult.ExternalID != "42" || run.ActionResult.URL != "https://github.com/derek/easyacp/pull/42" || run.Status != domain.PhaseRunAccepted {
		t.Fatalf("action result = %+v, run = %+v", run.ActionResult, run)
	}
}

func TestGitHubRepositoryRequiresCanonicalHTTPSRemote(t *testing.T) {
	owner, repository, err := githubRepository("https://github.com/derek/easyacp.git")
	if err != nil || owner != "derek" || repository != "easyacp" {
		t.Fatalf("parsed repository = %s/%s, error = %v", owner, repository, err)
	}
	if _, _, err := githubRepository("git@github.com:derek/easyacp.git"); err == nil {
		t.Fatal("SSH remote unexpectedly accepted for GitHub API action")
	}
}

func jsonResponse(status int, body string) *http.Response {
	return &http.Response{StatusCode: status, Header: make(http.Header), Body: io.NopCloser(strings.NewReader(body))}
}
