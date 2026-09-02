package server

import (
	"bytes"
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

func TestWorkflowAcceptOwnsCommitAndPublishesSessionToJobBranch(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &testEngine{}
	httpClient := &http.Client{Transport: roundTripFunc(func(request *http.Request) (*http.Response, error) {
		if request.Method == http.MethodGet {
			return jsonResponse(http.StatusOK, `[]`), nil
		}
		return jsonResponse(http.StatusCreated, `{"number":7,"html_url":"https://github.com/derek/accept/pull/7","title":"Feature"}`), nil
	})}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true, InternalURL: "http://spin.internal", HTTPClient: httpClient})
	for _, line := range []string{
		"RECORD tool:git --scope=global --enable=git", "install git", "END RECORD",
		"RECORD tool:agent --scope=global --from=tool:git --enable=acp --command=agent-acp", "install agent", "END RECORD",
	} {
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "accept", RemoteURL: "https://github.com/derek/accept.git", CredentialScope: domain.CredentialScopeUser})
	if err != nil {
		t.Fatal(err)
	}
	_, err = st.CreateGitAccount(domain.CreateGitAccountRequest{Operator: "derek", Provider: "github", Host: "github.com", Login: "derek", AccessToken: "github-secret"})
	if err != nil {
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
	if _, err := srv.useCapsule(context.Background(), domain.UseRequest{Selector: "session:" + created.Session.ID, Operator: "derek"}); err != nil {
		t.Fatal(err)
	}
	overallRequest := httptest.NewRequest(http.MethodGet, "/api/jobs/"+created.Job.ID+"/changes?operator=derek", nil)
	overallResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(overallResponse, overallRequest)
	if overallResponse.Code != http.StatusOK || len(engine.comparisons) != 1 {
		t.Fatalf("overall Job changes status=%d body=%s comparisons=%+v", overallResponse.Code, overallResponse.Body.String(), engine.comparisons)
	}
	if comparison := engine.comparisons[0]; comparison.BaseRef != created.Job.BaseRef || comparison.HeadRef != created.Job.Branch || comparison.CommitMessageMatch != "" || comparison.Authentication == nil || comparison.Authentication.Password != "github-secret" {
		t.Fatalf("overall Job comparison = %+v", comparison)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	tools, err := srv.workflowTools(created.Session.ID)
	if err != nil {
		t.Fatal(err)
	}
	for _, tool := range tools {
		if tool.Name == "commit" {
			t.Fatal("commit must never be exposed as an agent workflow tool")
		}
	}
	if _, err := srv.callWorkflowTool(context.Background(), created.Session.ID, "accept", map[string]any{"summary": "Darkmode gereed"}); err != nil {
		t.Fatal(err)
	}
	if len(engine.accepted) != 1 {
		t.Fatalf("workspace accept calls = %d", len(engine.accepted))
	}
	phaseRequest := httptest.NewRequest(http.MethodGet, "/api/jobs/"+created.Job.ID+"/changes?operator=derek&session_id="+created.Session.ID, nil)
	phaseResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(phaseResponse, phaseRequest)
	if phaseResponse.Code != http.StatusOK || len(engine.comparisons) != 2 {
		t.Fatalf("phase changes status=%d body=%s comparisons=%+v", phaseResponse.Code, phaseResponse.Body.String(), engine.comparisons)
	}
	if match := engine.comparisons[1].CommitMessageMatch; match != "Spin-Session: "+created.Session.ID {
		t.Fatalf("phase commit match = %q", match)
	}
	compositionID := ""
	for _, session := range st.Snapshot().Sessions {
		if session.ID == created.Session.ID {
			compositionID = session.PreparedCompositionID
			break
		}
	}
	if compositionID == "" {
		t.Fatal("accepted Session has no prepared composition")
	}
	if _, err := srv.stopCapsule(context.Background(), compositionID, "derek"); err != nil {
		t.Fatal(err)
	}
	materializedBefore := engine.materialized
	historicalRequest := httptest.NewRequest(http.MethodGet, "/api/jobs/"+created.Job.ID+"/changes?operator=derek&session_id="+created.Session.ID, nil)
	historicalResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(historicalResponse, historicalRequest)
	if historicalResponse.Code != http.StatusOK || engine.materialized != materializedBefore+1 || len(engine.comparisons) != 3 {
		t.Fatalf("historical changes status=%d body=%s materialized=%d comparisons=%d", historicalResponse.Code, historicalResponse.Body.String(), engine.materialized, len(engine.comparisons))
	}
	acceptance := engine.accepted[0]
	if !acceptance.AllowChanges || acceptance.RemoteRef != created.Job.Branch || !strings.Contains(acceptance.RemoteRef, "/main") {
		t.Fatalf("workspace acceptance = %+v, Job branch = %s", acceptance, created.Job.Branch)
	}
	for _, expected := range []string{created.Job.ID, created.Session.ID, "Spin-Phase: develop", "Spin-Accepted-By: agent"} {
		if !strings.Contains(acceptance.CommitBody, expected) {
			t.Fatalf("commit body %q missing %q", acceptance.CommitBody, expected)
		}
	}
	deadline := time.Now().Add(time.Second)
	for st.Snapshot().Jobs[0].WorkflowStatus != domain.WorkflowDone && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	snapshot := st.Snapshot()
	if snapshot.Jobs[0].WorkflowStatus != domain.WorkflowDone {
		t.Fatalf("Job did not complete through automatic PR: %+v", snapshot.Jobs[0])
	}
	foundPR := false
	for _, run := range snapshot.PhaseRuns {
		if run.PhaseID == domain.WorkflowPullRequestPhaseID && run.ActionResult != nil && run.ActionResult.URL == "https://github.com/derek/accept/pull/7" {
			foundPR = true
		}
	}
	if !foundPR {
		t.Fatalf("automatic PR result missing: %+v", snapshot.PhaseRuns)
	}
}

func TestWorkflowMCPPublishesOnlyPhaseToolsAndPausesOnOneQuestion(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true, InternalURL: "http://spin.internal"})
	for _, line := range []string{
		"RECORD tool:git --scope=global --enable=git", "install git", "END RECORD",
		"RECORD tool:agent --scope=global --from=tool:git --enable=acp --command=agent-acp", "install agent", "END RECORD",
	} {
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "mcp", RemoteURL: "https://example.com/mcp.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Docs", Phases: []domain.WorkflowPhase{{
		ID: "docs", Name: "Documenteer", Instructions: "Schrijf het FO", Deliverables: []domain.DeliverableDefinition{{Name: "FO", Required: true}},
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Docs", Objective: "FO", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	internal, err := srv.workflowMCPServer(created.Session.ID)
	if err != nil {
		t.Fatal(err)
	}
	authorization := internal.Headers[0].Value
	call := func(payload string) workflowMCPResponse {
		t.Helper()
		request := httptest.NewRequest(http.MethodPost, "/api/workflow/mcp/"+created.Session.ID, bytes.NewBufferString(payload))
		request.Header.Set("Authorization", authorization)
		request.Header.Set("Content-Type", "application/json")
		response := httptest.NewRecorder()
		srv.Handler().ServeHTTP(response, request)
		if response.Code != http.StatusOK {
			t.Fatalf("MCP status = %d, body = %s", response.Code, response.Body.String())
		}
		var decoded workflowMCPResponse
		if err := json.Unmarshal(response.Body.Bytes(), &decoded); err != nil {
			t.Fatal(err)
		}
		return decoded
	}
	listed := call(`{"jsonrpc":"2.0","id":1,"method":"tools/list"}`)
	encoded, _ := json.Marshal(listed.Result)
	for _, expected := range []string{`"ask"`, `"accept"`, `"reject"`, `"add_deliverable"`} {
		if !bytes.Contains(encoded, []byte(expected)) {
			t.Fatalf("tools/list missing %s: %s", expected, encoded)
		}
	}
	if bytes.Contains(encoded, []byte(`"commit"`)) {
		t.Fatalf("commit leaked into document phase: %s", encoded)
	}
	delivered := call(`{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"add_deliverable","arguments":{"name":"FO","content":"# FO"}}}`)
	if delivered.Error != nil || len(st.Snapshot().Deliverables) != 1 {
		t.Fatalf("deliverable call = %+v", delivered)
	}
	firstRevision := st.Snapshot().Deliverables[0]
	commentRequest := httptest.NewRequest(http.MethodPost, "/api/deliverables/"+firstRevision.ID+"/comments", bytes.NewBufferString(`{"operator":"mallory","selected_text":"FO","start_offset":0,"end_offset":2,"body":"Maak dit concreter."}`))
	commentRequest.Header.Set("Content-Type", "application/json")
	commentRequest = commentRequest.WithContext(context.WithValue(commentRequest.Context(), authContextKey{}, authenticatedIdentity{User: domain.User{Username: "derek"}}))
	commentResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(commentResponse, commentRequest)
	var comment domain.DeliverableComment
	if commentResponse.Code != http.StatusCreated || json.Unmarshal(commentResponse.Body.Bytes(), &comment) != nil || comment.Author != "derek" {
		t.Fatalf("comment status=%d body=%s decoded=%+v", commentResponse.Code, commentResponse.Body.String(), comment)
	}
	if _, err := st.AddWorkflowDeliverable(created.Session.ID, "FO", "# FO v2"); err != nil {
		t.Fatal(err)
	}
	historicalRequest := httptest.NewRequest(http.MethodPost, "/api/deliverables/"+firstRevision.ID+"/comments", bytes.NewBufferString(`{"selected_text":"FO","start_offset":0,"end_offset":2,"body":"Achteraf toegevoegd."}`))
	historicalRequest.Header.Set("Content-Type", "application/json")
	historicalRequest = historicalRequest.WithContext(context.WithValue(historicalRequest.Context(), authContextKey{}, authenticatedIdentity{User: domain.User{Username: "derek"}}))
	historicalResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(historicalResponse, historicalRequest)
	if historicalResponse.Code != http.StatusConflict {
		t.Fatalf("historical comment status=%d body=%s", historicalResponse.Code, historicalResponse.Body.String())
	}
	asked := call(`{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ask","arguments":{"question":"Doorgaan?"}}}`)
	if asked.Error != nil || len(st.Snapshot().WorkflowQuestions) != 1 || st.Snapshot().Jobs[0].WorkflowStatus != domain.WorkflowPending {
		t.Fatalf("ask call = %+v, snapshot = %+v", asked, st.Snapshot())
	}
}

func TestWorkflowPromptInjectsOnlySelectedLatestDeliverablesAndAlwaysGoal(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true})
	for _, line := range []string{
		"RECORD tool:git --scope=global --enable=git", "install git", "END RECORD",
		"RECORD tool:agent --scope=global --from=tool:git --enable=acp --command=agent-acp", "install agent", "END RECORD",
	} {
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "prompt", RemoteURL: "https://example.com/prompt.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Selective context", Phases: []domain.WorkflowPhase{
		{ID: "design", Name: "Design", Instructions: "Maak documenten", Deliverables: []domain.DeliverableDefinition{{Name: "FO", Description: "Functioneel ontwerp", Required: true}, {Name: "TO", Description: "Technisch ontwerp", Required: false}}, Accept: domain.WorkflowTransition{Target: "build"}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf}},
		{ID: "build", Name: "Build", Instructions: "Bouw alleen vanuit het FO", Inject: []string{"FO"}, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Darkmode", Objective: "Eén werkende switch", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	designPrompt, err := srv.workflowPrompt(created.Session.ID)
	if err != nil {
		t.Fatal(err)
	}
	for _, expected := range []string{"OP TE LEVEREN", "FO (VERPLICHT): Functioneel ontwerp", `add_deliverable met name "FO"`, "TO (OPTIONEEL): Technisch ontwerp"} {
		if !strings.Contains(designPrompt, expected) {
			t.Fatalf("design prompt missing %q:\n%s", expected, designPrompt)
		}
	}
	oldFO, err := st.AddWorkflowDeliverable(created.Session.ID, "FO", "oude FO")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.AddDeliverableComment(oldFO.ID, "john", domain.CreateDeliverableCommentRequest{SelectedText: "oude FO", StartOffset: 0, EndOffset: 7, Body: "oude comment hoort bij r1"}); err != nil {
		t.Fatal(err)
	}
	latestFO, err := st.AddWorkflowDeliverable(created.Session.ID, "FO", "laatste FO")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.AddDeliverableComment(latestFO.ID, "derek", domain.CreateDeliverableCommentRequest{SelectedText: "laatste FO", StartOffset: 0, EndOffset: 10, Body: "Neem deelbetalingen expliciet op."}); err != nil {
		t.Fatal(err)
	}
	if _, err := st.AddWorkflowDeliverable(created.Session.ID, "TO", "geheim technisch ontwerp"); err != nil {
		t.Fatal(err)
	}
	advance, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "documenten klaar")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("advance = %+v, error = %v", advance, err)
	}
	prompt, err := srv.workflowPrompt(advance.NextSession.ID)
	if err != nil {
		t.Fatal(err)
	}
	for _, expected := range []string{"Goal: Eén werkende switch", "GEÏNJECTEERDE DELIVERABLES", "FO (revisie 2)", "laatste FO", "COMMENTS OP ACTUELE DELIVERABLES", "Neem deelbetalingen expliciet op."} {
		if !strings.Contains(prompt, expected) {
			t.Fatalf("prompt missing %q:\n%s", expected, prompt)
		}
	}
	for _, excluded := range []string{"oude FO", "oude comment hoort bij r1", "geheim technisch ontwerp", "TO (revisie"} {
		if strings.Contains(prompt, excluded) {
			t.Fatalf("prompt unexpectedly contains %q:\n%s", excluded, prompt)
		}
	}
	if !strings.Contains(prompt, "Deze fase vraagt geen deliverables; add_deliverable is daarom niet beschikbaar") || strings.Contains(prompt, "Lever ieder hierboven gevraagd document") {
		t.Fatalf("build prompt has ambiguous deliverable instructions:\n%s", prompt)
	}
	nativePrompt, err := srv.workflowPromptForACP(advance.NextSession.ID, acpPromptCapabilities{EmbeddedContext: true})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(nativePrompt, "FO (revisie 2) is als verplichte Markdown-bijlage") || strings.Contains(nativePrompt, "--- FO (revisie 2) ---") || strings.Contains(nativePrompt, "geheim technisch ontwerp") {
		t.Fatalf("native ACP prompt does not describe selective deliverable attachment correctly:\n%s", nativePrompt)
	}
	resources, err := srv.acpPromptAttachments(advance.NextSession.ID, acpPromptCapabilities{EmbeddedContext: true}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(resources) != 1 || resources[0].ID != "deliverable:"+latestFO.ID || resources[0].Block["type"] != "resource" {
		t.Fatalf("ACP deliverable resources = %#v", resources)
	}
	resource := resources[0].Block["resource"].(map[string]any)
	if resource["mimeType"] != "text/markdown" || resource["text"] != "laatste FO" {
		t.Fatalf("ACP deliverable resource = %#v", resource)
	}
}

func TestWorkflowRetryEndpointStopsOldCompositionAndReturnsImmediately(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &testEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	for _, line := range []string{
		"RECORD tool:git --scope=global --enable=git", "install git", "END RECORD",
		"RECORD tool:agent --scope=global --from=tool:git --enable=acp --command=agent-acp", "install agent", "END RECORD",
	} {
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "retry-endpoint", RemoteURL: "https://example.com/retry.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Retry endpoint", Phases: []domain.WorkflowPhase{{
		ID: "plan", Name: "Plan", Instructions: "Plan", Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Retry", Objective: "Retry", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	composition, err := srv.useCapsule(context.Background(), domain.UseRequest{Selector: "session:" + created.Session.ID, Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}

	request := httptest.NewRequest(http.MethodPost, "/api/sessions/"+created.Session.ID+"/retry?operator=derek", nil)
	response := httptest.NewRecorder()
	srv.Handler().ServeHTTP(response, request)
	if response.Code != http.StatusAccepted {
		t.Fatalf("retry status = %d; body = %s", response.Code, response.Body.String())
	}
	var retried domain.CreateJobResponse
	if err := json.Unmarshal(response.Body.Bytes(), &retried); err != nil {
		t.Fatal(err)
	}
	if retried.Session.ID != created.Session.ID || retried.Session.PhaseRunID != created.Session.PhaseRunID {
		t.Fatalf("retry replaced Session: %+v", retried)
	}

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		stopped, err := st.Composition(composition.ID)
		if err == nil && stopped.Runtime != nil && stopped.Runtime.Status == "stopped" {
			snapshot := st.Snapshot()
			if len(snapshot.PhaseRuns) != 1 || snapshot.PhaseRuns[0].Status != domain.PhaseRunQueued || snapshot.PhaseRuns[0].Attempt != 1 {
				t.Fatalf("retry phase = %+v", snapshot.PhaseRuns)
			}
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatal("retry did not stop the previous composition in the background")
}
