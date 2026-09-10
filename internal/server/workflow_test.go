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
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "accept", RemoteURL: "https://github.com/derek/accept.git", CredentialScope: domain.CredentialScopeUser})
	if err != nil {
		t.Fatal(err)
	}
	_, err = st.CreateGitAccount(domain.CreateGitAccountRequest{Operator: "derek", Provider: "github", Host: "github.com", Login: "derek", AccessToken: "github-secret"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Code", Phases: []domain.WorkflowPhase{
		{ID: "develop", Name: "Ontwikkelen", Instructions: "Bouw het", AllowChanges: true, Accept: domain.WorkflowTransition{Target: "NEXT"}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf}},
		{ID: "pr", Name: "Pull request", Executor: domain.WorkflowExecutorAction, Action: &domain.WorkflowAction{Type: domain.WorkflowActionGitPullRequest}, Accept: domain.WorkflowTransition{Target: "DONE"}, Reject: domain.WorkflowTransition{Target: "SELF", Max: 2, Exhausted: "ASK_USER"}},
	}})
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
	reviewRequest := httptest.NewRequest(http.MethodPost, "/api/jobs/"+created.Job.ID+"/code-reviews?operator=derek", bytes.NewBufferString(`{"session_id":"`+created.Session.ID+`","live":true}`))
	reviewRequest.Header.Set("Content-Type", "application/json")
	reviewResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(reviewResponse, reviewRequest)
	var review domain.CodeReviewBundle
	if reviewResponse.Code != http.StatusCreated || json.Unmarshal(reviewResponse.Body.Bytes(), &review) != nil || !review.Annotatable || len(review.Revision.Files) != 1 || review.Revision.Files[0].Path != "active.go" {
		t.Fatalf("code review status=%d body=%s decoded=%+v", reviewResponse.Code, reviewResponse.Body.String(), review)
	}
	commentRequest := httptest.NewRequest(http.MethodPost, "/api/code-reviews/"+review.Revision.ID+"/comments", bytes.NewBufferString(`{"operator":"derek","path":"active.go","side":"new","start_line":2,"end_line":2,"selected_text":"new","body":"Geef dit een duidelijke naam."}`))
	commentRequest.Header.Set("Content-Type", "application/json")
	commentResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(commentResponse, commentRequest)
	if commentResponse.Code != http.StatusCreated || len(st.Snapshot().CodeReviewComments) != 1 {
		t.Fatalf("code comment status=%d body=%s", commentResponse.Code, commentResponse.Body.String())
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
	// A stopped composition is compared on the runner's clone of the remote:
	// nothing is restored, no image travels.
	if historicalResponse.Code != http.StatusOK || engine.materialized != materializedBefore || len(engine.comparisons) != 3 || len(engine.repositories) != 1 {
		t.Fatalf("historical changes status=%d body=%s materialized=%d comparisons=%d repositories=%d", historicalResponse.Code, historicalResponse.Body.String(), engine.materialized, len(engine.comparisons), len(engine.repositories))
	}
	if compared := engine.repositories[0]; compared.RemoteURL == "" || compared.CacheKey != repository.Repository.ID || compared.Comparison.CommitMessageMatch != "Spin-Session: "+created.Session.ID || compared.Comparison.Authentication == nil || compared.Comparison.Authentication.Password != "github-secret" {
		t.Fatalf("repository comparison = %+v", compared)
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
		if run.PhaseID == "pr" && run.ActionResult != nil && run.ActionResult.URL == "https://github.com/derek/accept/pull/7" {
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
	engine := &deliverableTestEngine{files: map[string][]byte{"/root/deliverables/fo.md": []byte("# FO")}}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true, InternalURL: "http://spin.internal"})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "mcp", RemoteURL: "https://example.com/mcp.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Docs", Phases: []domain.WorkflowPhase{{
		ID: "docs", Name: "Documenteer", Instructions: "Schrijf het FO", Deliverables: []domain.DeliverableDefinition{{Name: "FO", Required: true}, {Name: "Preview", Kind: domain.DeliverableKindFolder}},
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Docs", Objective: "FO", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := srv.useCapsule(context.Background(), domain.UseRequest{Selector: "session:" + created.Session.ID, Operator: "derek"}); err != nil {
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
	for _, expected := range []string{`"ask"`, `"accept"`, `"reject"`, `"put_deliverable"`} {
		if !bytes.Contains(encoded, []byte(expected)) {
			t.Fatalf("tools/list missing %s: %s", expected, encoded)
		}
	}
	if bytes.Contains(encoded, []byte(`"commit"`)) {
		t.Fatalf("commit leaked into document phase: %s", encoded)
	}
	outside := call(`{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"put_deliverable","arguments":{"name":"FO","path":"/tmp/fo.md"}}}`)
	outsideText, _ := json.Marshal(outside)
	if !bytes.Contains(outsideText, []byte("inside /root/deliverables")) || len(st.Snapshot().Deliverables) != 0 {
		t.Fatalf("put outside the deliverable directory = %s", outsideText)
	}
	delivered := call(`{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"put_deliverable","arguments":{"name":"FO","path":"/root/deliverables/fo.md"}}}`)
	if delivered.Error != nil || len(st.Snapshot().Deliverables) != 1 || st.Snapshot().Deliverables[0].Content != "# FO" {
		t.Fatalf("deliverable call = %+v, deliverables = %+v", delivered, st.Snapshot().Deliverables)
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
	// One revision per Session: the agent edits on disk and puts again,
	// which updates revision 1 in place.
	engine.files["/root/deliverables/fo.md"] = []byte("# FO v2")
	edited := call(`{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"put_deliverable","arguments":{"name":"FO","path":"/root/deliverables/fo.md"}}}`)
	editedText, _ := json.Marshal(edited.Result)
	if edited.Error != nil || !bytes.Contains(editedText, []byte("revisie 1")) || len(st.Snapshot().Deliverables) != 1 || st.Snapshot().Deliverables[0].Content != "# FO v2" {
		t.Fatalf("second put = %+v, deliverables = %+v", edited, st.Snapshot().Deliverables)
	}
	missing := call(`{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"put_deliverable","arguments":{"name":"FO","path":"/root/deliverables/bestaat-niet.md"}}}`)
	missingText, _ := json.Marshal(missing)
	if !bytes.Contains(missingText, []byte("does not exist")) || len(st.Snapshot().Deliverables) != 1 {
		t.Fatalf("put of a missing file = %s", missingText)
	}
	// A visual deliverable is bundled by the runner and kept as such.
	engine.bundle = domain.DeliverableBundle{Ref: "bundle:abc", Digest: "sha256:abc", Size: 1234, Files: 3, Folder: true, Entry: "index.html", ContentType: "text/html; charset=utf-8"}
	visual := call(`{"jsonrpc":"2.0","id":23,"method":"tools/call","params":{"name":"put_deliverable","arguments":{"name":"Preview","path":"/root/deliverables/preview/"}}}`)
	if visual.Error != nil || engine.bundled != "/root/deliverables/preview" {
		t.Fatalf("visual put = %+v, bundled %q", visual, engine.bundled)
	}
	var preview domain.Deliverable
	for _, candidate := range st.Snapshot().Deliverables {
		if candidate.Name == "Preview" {
			preview = candidate
		}
	}
	if preview.Kind != domain.DeliverableKindFolder || preview.Bundle == nil || preview.Bundle.Ref != "bundle:abc" || preview.Content != "" || preview.CapsulePath() != "/root/deliverables/preview" {
		t.Fatalf("visual deliverable = %+v", preview)
	}
	downloadRequest := httptest.NewRequest(http.MethodGet, "/api/deliverables/"+firstRevision.ID+"/download", nil)
	downloadResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(downloadResponse, downloadRequest)
	if downloadResponse.Code != http.StatusOK || downloadResponse.Header().Get("Content-Disposition") != "attachment; filename=FO-r1.md" || downloadResponse.Body.String() != "# FO v2\n" {
		t.Fatalf("download status=%d disposition=%q body=%q", downloadResponse.Code, downloadResponse.Header().Get("Content-Disposition"), downloadResponse.Body.String())
	}
	asked := call(`{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ask","arguments":{"questions":[{"question":"Doorgaan?","options":["Ja","Nee"]},{"question":"Welke naam?"}]}}}`)
	if asked.Error != nil || len(st.Snapshot().WorkflowQuestions) != 1 || st.Snapshot().Jobs[0].WorkflowStatus != domain.WorkflowPending {
		t.Fatalf("ask call = %+v, snapshot = %+v", asked, st.Snapshot())
	}
	form := st.Snapshot().WorkflowQuestions[0]
	if len(form.Items) != 2 || form.Items[0].Question != "Doorgaan?" || len(form.Items[0].Options) != 2 || form.Items[1].Question != "Welke naam?" || len(form.Items[1].Options) != 0 {
		t.Fatalf("ask form = %+v", form.Items)
	}
	// Answering records first and resumes the agent second. Without a running
	// capsule the second step fails, and the answers must already be durable.
	answerRequest := httptest.NewRequest(http.MethodPost, "/api/workflow/questions/"+form.ID+"/answer", bytes.NewBufferString(`{"action":"answer","answers":[{"item_id":"q1","answer":"Ja"},{"item_id":"q2","answer":"Notulen"}]}`))
	answerRequest.Header.Set("Content-Type", "application/json")
	answerRequest = answerRequest.WithContext(context.WithValue(answerRequest.Context(), authContextKey{}, authenticatedIdentity{User: domain.User{Username: "derek"}}))
	answerResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(answerResponse, answerRequest)
	if answerResponse.Code == http.StatusOK || !strings.Contains(answerResponse.Body.String(), "answers are recorded") {
		t.Fatalf("answer without a capsule status=%d body=%s", answerResponse.Code, answerResponse.Body.String())
	}
	answered := st.Snapshot().WorkflowQuestions[0]
	if answered.Status != "answered" || answered.Items[0].Answer != "Ja" || answered.Items[0].Other || answered.Items[1].Answer != "Notulen" || !answered.Items[1].Other {
		t.Fatalf("answered form = %+v", answered.Items)
	}
	if st.Snapshot().Jobs[0].WorkflowStatus != domain.WorkflowBusy {
		t.Fatalf("job after answers = %+v", st.Snapshot().Jobs[0])
	}
}

func TestWorkflowPromptInjectsOnlySelectedLatestDeliverablesAndAlwaysGoal(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
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
	for _, expected := range []string{"OP TE LEVEREN", "FO (VERPLICHT): Functioneel ontwerp", "Markdown-bestand; bijvoorbeeld /root/deliverables/fo.md", "TO (OPTIONEEL): Technisch ontwerp", "put_deliverable(name, path)"} {
		if !strings.Contains(designPrompt, expected) {
			t.Fatalf("design prompt missing %q:\n%s", expected, designPrompt)
		}
	}
	oldFO, err := st.PutWorkflowDeliverable(created.Session.ID, "FO", "oude FO", nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.AddDeliverableComment(oldFO.ID, "john", domain.CreateDeliverableCommentRequest{SelectedText: "oude FO", StartOffset: 0, EndOffset: 7, Body: "oude comment hoort bij r1"}); err != nil {
		t.Fatal(err)
	}
	// A second design attempt is a new Session and so writes revision 2.
	redo, err := st.CompleteWorkflowPhase(created.Session.ID, "reject", "nog niet af")
	if err != nil || redo.NextSession == nil {
		t.Fatalf("design retry = %+v, error = %v", redo, err)
	}
	designRetry := redo.NextSession.ID
	if _, err := st.MarkWorkflowPhaseRunning(designRetry); err != nil {
		t.Fatal(err)
	}
	latestFO, err := st.PutWorkflowDeliverable(designRetry, "FO", "laatste FO", nil)
	if err != nil || latestFO.Revision != 2 {
		t.Fatalf("latest FO = %+v, error = %v", latestFO, err)
	}
	if _, err := st.AddDeliverableComment(latestFO.ID, "derek", domain.CreateDeliverableCommentRequest{SelectedText: "laatste FO", StartOffset: 0, EndOffset: 10, Body: "Neem deelbetalingen expliciet op."}); err != nil {
		t.Fatal(err)
	}
	if _, err := st.PutWorkflowDeliverable(designRetry, "TO", "geheim technisch ontwerp", nil); err != nil {
		t.Fatal(err)
	}
	advance, err := st.CompleteWorkflowPhase(designRetry, "accept", "documenten klaar")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("advance = %+v, error = %v", advance, err)
	}
	prompt, err := srv.workflowPrompt(advance.NextSession.ID)
	if err != nil {
		t.Fatal(err)
	}
	// The deliverables are files in the capsule: the prompt names them and
	// which are required reading; their text is not in the prompt.
	for _, expected := range []string{"Goal: Eén werkende switch", "DELIVERABLES", "FO: /root/deliverables/fo.md (revisie 2, Markdown)", "TO: /root/deliverables/to.md (revisie 1, Markdown)", "Verplichte context voor deze stap: FO.", "COMMENTS OP ACTUELE DELIVERABLES", "Neem deelbetalingen expliciet op."} {
		if !strings.Contains(prompt, expected) {
			t.Fatalf("prompt missing %q:\n%s", expected, prompt)
		}
	}
	for _, excluded := range []string{"oude FO", "oude comment hoort bij r1", "geheim technisch ontwerp", "--- FO (revisie"} {
		if strings.Contains(prompt, excluded) {
			t.Fatalf("prompt unexpectedly contains %q:\n%s", excluded, prompt)
		}
	}
	if !strings.Contains(prompt, "Deze fase vraagt geen deliverables; put_deliverable is daarom niet beschikbaar") || strings.Contains(prompt, "Zet ieder hierboven gevraagd document") {
		t.Fatalf("build prompt has ambiguous deliverable instructions:\n%s", prompt)
	}
	resources, err := srv.acpPromptAttachments(advance.NextSession.ID, acpPromptCapabilities{EmbeddedContext: true}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(resources) != 0 {
		t.Fatalf("deliverables are files in the capsule, yet %d were attached: %#v", len(resources), resources)
	}
	_ = latestFO
	if _, err := st.MarkWorkflowPhaseRunning(advance.NextSession.ID); err != nil {
		t.Fatal(err)
	}
	var buildRun domain.PhaseRun
	for _, candidate := range st.Snapshot().PhaseRuns {
		if candidate.SessionID == advance.NextSession.ID {
			buildRun = candidate
			break
		}
	}
	if buildRun.ID == "" {
		t.Fatal("build PhaseRun not found")
	}
	review, err := st.SaveCodeReviewRevision(domain.CodeReviewRevision{
		JobID: created.Job.ID, SourcePhaseRunID: buildRun.ID, ContextPhaseRunID: buildRun.ID,
		SessionID: advance.NextSession.ID, PhaseID: buildRun.PhaseID, PhaseName: buildRun.PhaseName,
		Attempt: buildRun.Attempt, Scope: "phase", ScopeKey: "job:" + created.Job.ID + ":phase:" + buildRun.PhaseID,
		Digest: "review-feedback", CreatedBy: "derek", Files: []domain.CodeReviewFile{{Path: "ui/theme.go", Patch: "+fallback := light"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.AddCodeReviewComment(review.ID, "john", domain.CreateCodeReviewCommentRequest{Path: "ui/theme.go", Side: "new", StartLine: 42, EndLine: 42, SelectedText: "fallback := light", Body: "Gebruik hier het opgeslagen user preference."}); err != nil {
		t.Fatal(err)
	}
	rejected, err := st.CompleteWorkflowPhase(advance.NextSession.ID, "reject", "Darkmode onthoudt de keuze nog niet")
	if err != nil || rejected.NextSession == nil {
		t.Fatalf("reject advance = %+v, error = %v", rejected, err)
	}
	retryPrompt, err := srv.workflowPrompt(rejected.NextSession.ID)
	if err != nil {
		t.Fatal(err)
	}
	for _, expected := range []string{"CODECOMMENTS UIT AFGEWEZEN REVIEWS", "ui/theme.go:42", "fallback := light", "Gebruik hier het opgeslagen user preference."} {
		if !strings.Contains(retryPrompt, expected) {
			t.Fatalf("retry prompt missing %q:\n%s", expected, retryPrompt)
		}
	}
}

func TestForkedWorkflowReceivesPreviousGoalAndLatestDeliverables(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "follow-up", RemoteURL: "https://example.com/follow-up.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Context", Phases: []domain.WorkflowPhase{{
		ID: "plan", Name: "Plan", Instructions: "Maak een plan", Deliverables: []domain.DeliverableDefinition{{Name: "FO", Required: true}},
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	source, err := st.CreateJob(domain.CreateJobRequest{Title: "Betalingen synchroniseren", Objective: "Synchroniseer Exact dagelijks", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(source.Session.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := st.PutWorkflowDeliverable(source.Session.ID, "FO", "oude context", nil); err != nil {
		t.Fatal(err)
	}
	latest, err := st.PutWorkflowDeliverable(source.Session.ID, "FO", "# Laatste FO\n\nVerwerk deelbetalingen.", nil)
	if err != nil {
		t.Fatal(err)
	}
	closed, err := st.CloseJob(source.Job.ID, "derek")
	if err != nil {
		t.Fatal(err)
	}
	fork, err := st.CreateJob(domain.CreateJobRequest{Title: "Nagekomen bug", Objective: "Los afrondingsverschil op", Operator: "derek", ForkedFromJobID: closed.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	prompt, err := srv.workflowPrompt(fork.Session.ID)
	if err != nil {
		t.Fatal(err)
	}
	for _, expected := range []string{"VERVOLGCONTEXT", closed.Title, closed.Branch, "Synchroniseer Exact dagelijks", "# Laatste FO", "Verwerk deelbetalingen."} {
		if !strings.Contains(prompt, expected) {
			t.Fatalf("fork prompt missing %q:\n%s", expected, prompt)
		}
	}
	if strings.Contains(prompt, "oude context") {
		t.Fatalf("fork prompt contains stale deliverable revision:\n%s", prompt)
	}
	resources, err := srv.acpPromptAttachments(fork.Session.ID, acpPromptCapabilities{EmbeddedContext: true}, nil)
	if err != nil {
		t.Fatal(err)
	}
	wantedID := "fork-deliverable:" + latest.ID
	found := false
	for _, resource := range resources {
		if resource.ID == wantedID {
			found = true
		}
	}
	if !found {
		t.Fatalf("latest source deliverable %q was not supplied as ACP context: %#v", wantedID, resources)
	}
}

func TestWorkflowRetryEndpointStopsOldCompositionAndReturnsImmediately(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &blockingStopEngine{started: make(chan struct{}), release: make(chan struct{})}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
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
	select {
	case <-engine.started:
	case <-time.After(time.Second):
		t.Fatal("previous composition cleanup did not start")
	}
	replacement, err := srv.useCapsule(context.Background(), domain.UseRequest{Selector: "session:" + created.Session.ID, Operator: "derek"})
	if err != nil {
		t.Fatalf("materialize replacement while old runner cleanup is pending: %v", err)
	}
	close(engine.release)

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		stopped, err := st.Composition(composition.ID)
		if err == nil && stopped.Runtime != nil && stopped.Runtime.Status == "stopped" {
			snapshot := st.Snapshot()
			var current domain.Session
			for _, session := range snapshot.Sessions {
				if session.ID == created.Session.ID {
					current = session
					break
				}
			}
			if len(snapshot.PhaseRuns) != 1 || snapshot.PhaseRuns[0].Status != domain.PhaseRunQueued || snapshot.PhaseRuns[0].Attempt != 1 {
				t.Fatalf("retry phase = %+v", snapshot.PhaseRuns)
			}
			if current.PreparedCompositionID != replacement.ID {
				t.Fatalf("returning old runner displaced replacement: old=%s replacement=%s current=%+v", composition.ID, replacement.ID, current)
			}
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("retry did not replace the previous composition while cleaning it in the background: %+v", st.Snapshot())
}

// deliverableTestEngine is the test engine with files a capsule holds and a
// bundle the runner would deliver.
type deliverableTestEngine struct {
	testEngine
	files   map[string][]byte
	bundle  domain.DeliverableBundle
	bundled string
	placed  []string
}

func (e *deliverableTestEngine) ReadTrackedFiles(_ context.Context, _ domain.CapsuleRuntime, paths []string) (map[string][]byte, error) {
	out := map[string][]byte{}
	for _, path := range paths {
		if data, ok := e.files[path]; ok {
			out[path] = data
		}
	}
	return out, nil
}

func (e *deliverableTestEngine) WriteTrackedFiles(_ context.Context, _ domain.CapsuleRuntime, files map[string][]byte) error {
	for path, data := range files {
		e.files[path] = data
		e.placed = append(e.placed, path)
	}
	return nil
}

func (e *deliverableTestEngine) BundleDeliverable(_ context.Context, _ domain.CapsuleRuntime, path string) (domain.DeliverableBundle, error) {
	e.bundled = path
	return e.bundle, nil
}

func (e *deliverableTestEngine) PlaceDeliverable(_ context.Context, _ domain.CapsuleRuntime, target string, _ domain.DeliverableBundle) error {
	e.placed = append(e.placed, target)
	return nil
}

// A brainstorm chat offers one tool, start_process; its prompt says the
// goal is still open, and calling the tool sets the goal and queues the
// Template's first step.
func TestBrainstormOffersOnlyStartProcess(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true, InternalURL: "http://spin.internal"})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://example.com/shop.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Code", Phases: []domain.WorkflowPhase{{
		ID: "dev", Name: "Ontwikkel", Instructions: "Bouw het", AllowChanges: true, Deliverables: []domain.DeliverableDefinition{{Name: "FO", Required: true}},
		Accept: domain.WorkflowTransition{Target: "DONE"}, Reject: domain.WorkflowTransition{Target: "SELF"},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Brainstorm: true, Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	prompt, err := srv.workflowPrompt(created.Session.ID)
	if err != nil {
		t.Fatal(err)
	}
	for _, expected := range []string{`workflowfase "Brainstorm"`, "Goal: (nog te bepalen", "Dit is een brainstorm, geen uitvoering", "start_process(goal)"} {
		if !strings.Contains(prompt, expected) {
			t.Fatalf("brainstorm prompt missing %q:\n%s", expected, prompt)
		}
	}
	for _, excluded := range []string{"OP TE LEVEREN", "put_deliverable", "accept, of reject"} {
		if strings.Contains(prompt, excluded) {
			t.Fatalf("brainstorm prompt contains %q:\n%s", excluded, prompt)
		}
	}
	internal, err := srv.workflowMCPServer(created.Session.ID)
	if err != nil {
		t.Fatal(err)
	}
	call := func(payload string) workflowMCPResponse {
		t.Helper()
		request := httptest.NewRequest(http.MethodPost, "/api/workflow/mcp/"+created.Session.ID, bytes.NewBufferString(payload))
		request.Header.Set("Authorization", internal.Headers[0].Value)
		request.Header.Set("Content-Type", "application/json")
		response := httptest.NewRecorder()
		srv.Handler().ServeHTTP(response, request)
		var decoded workflowMCPResponse
		if err := json.Unmarshal(response.Body.Bytes(), &decoded); err != nil {
			t.Fatal(err)
		}
		return decoded
	}
	listed, _ := json.Marshal(call(`{"jsonrpc":"2.0","id":1,"method":"tools/list"}`).Result)
	if !bytes.Contains(listed, []byte(`"start_process"`)) || bytes.Contains(listed, []byte(`"accept"`)) || bytes.Contains(listed, []byte(`"ask"`)) || bytes.Contains(listed, []byte(`"put_deliverable"`)) {
		t.Fatalf("brainstorm tools = %s", listed)
	}
	started := call(`{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"start_process","arguments":{"goal":"# Darkmode\n\nEén werkende switch."}}}`)
	startedText, _ := json.Marshal(started.Result)
	if started.Error != nil || !bytes.Contains(startedText, []byte("stap 1")) {
		t.Fatalf("start_process = %+v", started)
	}
	snapshot := st.Snapshot()
	if snapshot.Jobs[0].Objective != "# Darkmode\n\nEén werkende switch." {
		t.Fatalf("goal after start_process = %q", snapshot.Jobs[0].Objective)
	}
	var next domain.PhaseRun
	for _, run := range snapshot.PhaseRuns {
		if run.PhaseID == "dev" {
			next = run
		}
	}
	if next.ID == "" || next.Status != domain.PhaseRunQueued {
		t.Fatalf("first step after the brainstorm = %+v", next)
	}
	nextPrompt, err := srv.workflowPrompt(next.SessionID)
	if err != nil || !strings.Contains(nextPrompt, "Goal: # Darkmode") {
		t.Fatalf("next prompt = %q, %v", nextPrompt, err)
	}
	after, _ := json.Marshal(call(`{"jsonrpc":"2.0","id":3,"method":"tools/list"}`).Result)
	if bytes.Contains(after, []byte(`"start_process"`)) {
		t.Fatalf("a finished brainstorm still offers start_process: %s", after)
	}
}
