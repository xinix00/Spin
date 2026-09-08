package server

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"easyacp/internal/capsule"
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
	merged   []capsule.WorkspaceMerge
	synced   []capsule.WorkspaceSync
	browsed  []capsule.RepositoryBrowse
}

func (e *appTestEngine) BrowseRepository(_ context.Context, browse capsule.RepositoryBrowse) (capsule.RepositoryBrowseResult, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.browsed = append(e.browsed, browse)
	switch browse.Mode {
	case "refs":
		return capsule.RepositoryBrowseResult{Refs: []capsule.RepositoryRef{{Name: "feature", CommittedAt: time.Unix(1700000000, 0)}, {Name: "develop", CommittedAt: time.Unix(1600000000, 0)}}}, nil
	case "tree":
		return capsule.RepositoryBrowseResult{Tree: &capsule.WorkspaceTree{Ref: browse.Ref, Entries: []capsule.WorkspaceEntry{{Path: "hello.txt", Size: 6}}}}, nil
	}
	return capsule.RepositoryBrowseResult{File: &capsule.WorkspaceFile{Ref: browse.Ref, Path: browse.Path, Size: 6, Content: "hello\n"}}, nil
}

func (e *appTestEngine) SyncWorkspace(_ context.Context, _ domain.CapsuleRuntime, sync capsule.WorkspaceSync) (capsule.WorkspaceSyncResult, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.synced = append(e.synced, sync)
	return capsule.WorkspaceSyncResult{Head: "wip1234", Committed: true, Pushed: true}, nil
}

func (e *appTestEngine) StartAppServices(_ context.Context, _ domain.CapsuleRuntime, sessionID string, services []domain.AppService, _ []string) ([]domain.AppServiceRuntime, error) {
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

func (e *appTestEngine) MergeWorkspace(_ context.Context, _ domain.CapsuleRuntime, merge capsule.WorkspaceMerge) (capsule.WorkspaceMergeResult, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.merged = append(e.merged, merge)
	return capsule.WorkspaceMergeResult{Head: "abc123"}, nil
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
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
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

// A Template that finalizes by merging lands the Job branch on the base
// branch from a workspace, with the Job done afterwards; no pull request.
func TestMergeFinalizerLandsTheJobOnTheBaseBranch(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &appTestEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://github.com/derek/shop.git", DefaultRef: "develop", CredentialScope: domain.CredentialScopePublic,
		Services: []domain.AppService{{Name: "web", Run: "npm start", Ports: []int{3000}}}})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Test en merge", Finalize: domain.WorkflowFinalizeMerge, Phases: []domain.WorkflowPhase{{
		ID: "test", Name: "Testen", Executor: domain.WorkflowExecutorExpose,
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: "SELF"},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	if last := template.Phases[len(template.Phases)-1]; last.Action == nil || last.Action.Type != domain.WorkflowActionGitMerge || last.Name != "Mergen" {
		t.Fatalf("finalizer = %+v", last)
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
		t.Fatalf("no decision for the expose phase; failures=%+v", srv.sessionPreparations())
	}
	advance, err := st.AnswerWorkflowQuestion(question.ID, "derek", "accept", "")
	if err != nil {
		t.Fatal(err)
	}
	if advance.NextSession == nil {
		t.Fatalf("accept did not queue the merge phase: %+v", advance)
	}
	srv.startQueuedWorkflowLaunch(*advance.NextSession)
	deadline = time.Now().Add(10 * time.Second)
	var job domain.Job
	for job.WorkflowStatus != domain.WorkflowDone && time.Now().Before(deadline) {
		job = st.Snapshot().Jobs[0]
		time.Sleep(10 * time.Millisecond)
	}
	if job.WorkflowStatus != domain.WorkflowDone {
		var reasons []string
		for _, run := range st.Snapshot().PhaseRuns {
			reasons = append(reasons, run.PhaseID+"/"+string(run.Status)+": "+run.RejectReason)
		}
		t.Fatalf("job after merge = %s; runs=%v; failures=%+v", job.WorkflowStatus, reasons, srv.sessionPreparations())
	}
	if len(engine.merged) != 1 || engine.merged[0].SourceRef != job.Branch || engine.merged[0].TargetRef != "develop" {
		t.Fatalf("merged = %+v", engine.merged)
	}
	var mergeRun domain.PhaseRun
	for _, run := range st.Snapshot().PhaseRuns {
		if run.JobID == job.ID && run.PhaseID == domain.WorkflowPullRequestPhaseID {
			mergeRun = run
		}
	}
	if mergeRun.ActionResult == nil || mergeRun.ActionResult.Type != domain.WorkflowActionGitMerge || !strings.Contains(mergeRun.ActionResult.Detail, "merge-commit abc123") {
		t.Fatalf("merge run = %+v", mergeRun)
	}
}

// After an agent turn the Session's work in progress goes to the remote:
// a phase that may change code pushes its Session branch, one that may not
// pushes nothing, and calls closer together than the interval are dropped.
func TestWorkspaceSyncPushesWorkInProgressAfterATurn(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &appTestEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "sync", RemoteURL: "https://example.com/sync.git", DefaultRef: "develop"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Sync", Phases: []domain.WorkflowPhase{
		{ID: "design", Name: "Ontwerp", Instructions: "Denk", Accept: domain.WorkflowTransition{Target: "build"}, Reject: domain.WorkflowTransition{Target: "SELF"}},
		{ID: "build", Name: "Bouw", Instructions: "Bouw", AllowChanges: true, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: "SELF"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Sync", Objective: "Werkend", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	srv.launchQueuedWorkflowPhases("test")
	ready := func(sessionID string) bool {
		_, composition, err := srv.sessionComposition(sessionID, "derek")
		return err == nil && composition.Runtime != nil && composition.Runtime.Status == "ready"
	}
	for deadline := time.Now().Add(10 * time.Second); !ready(created.Session.ID) && time.Now().Before(deadline); {
		time.Sleep(10 * time.Millisecond)
	}
	if !ready(created.Session.ID) {
		t.Fatalf("design workspace never came up; failures=%+v", srv.sessionPreparations())
	}
	// The design phase may not change code: nothing is pushed.
	srv.syncWorkspace(created.Session.ID)
	if len(engine.synced) != 0 {
		t.Fatalf("a read-only phase pushed: %+v", engine.synced)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	advance, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "ontwerp staat")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("advance = %+v, error = %v", advance, err)
	}
	build := advance.NextSession.ID
	srv.launchQueuedWorkflowPhases("test")
	for deadline := time.Now().Add(10 * time.Second); !ready(build) && time.Now().Before(deadline); {
		time.Sleep(10 * time.Millisecond)
	}
	if !ready(build) {
		t.Fatalf("build workspace never came up; failures=%+v", srv.sessionPreparations())
	}
	srv.syncWorkspace(build)
	srv.syncWorkspace(build)
	if len(engine.synced) != 1 || !strings.HasSuffix(engine.synced[0].SessionRef, "/sessions/"+build) {
		t.Fatalf("synced = %+v", engine.synced)
	}
	for _, session := range st.Snapshot().Sessions {
		if session.ID == build && (session.SyncedHead != "wip1234" || session.SyncedAt == nil) {
			t.Fatalf("session after sync = %+v", session)
		}
	}
}

// A Job handed to a colleague: the assignee can open its Session (the chat,
// the changes), anyone else cannot.
func TestAssigneeMayOpenAJobsSession(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &appTestEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "assign", RemoteURL: "https://example.com/assign.git", DefaultRef: "develop"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Kort", Phases: []domain.WorkflowPhase{{ID: "build", Name: "Bouw", Instructions: "Bouw", AllowChanges: true, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: "SELF"}}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Assign", Objective: "x", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	srv.launchQueuedWorkflowPhases("test")
	for deadline := time.Now().Add(10 * time.Second); time.Now().Before(deadline); {
		if _, composition, err := srv.sessionComposition(created.Session.ID, "derek"); err == nil && composition.Runtime != nil {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	if _, _, err := srv.sessionComposition(created.Session.ID, "john"); err == nil || !strings.Contains(err.Error(), "belongs to derek") {
		t.Fatalf("a stranger opened the session: %v", err)
	}
	admin, err := st.CreateInitialUser(domain.User{Username: "derek", PasswordHash: "x"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.CreateUser(admin.ID, domain.User{Username: "john", PasswordHash: "x"}); err != nil {
		t.Fatal(err)
	}
	if _, err := st.AssignJob(created.Job.ID, "derek", "john"); err != nil {
		t.Fatal(err)
	}
	if _, _, err := srv.sessionComposition(created.Session.ID, "john"); err != nil {
		t.Fatalf("the assignee could not open the session: %v", err)
	}
}

// Explore browses a repository without a Job: branches, a tree on the
// default branch, and a file on a chosen branch, through the runner.
func TestExploreBrowsesARepositoryThroughTheRunner(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &appTestEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "explore", RemoteURL: "https://example.com/explore.git", DefaultRef: "develop"})
	if err != nil {
		t.Fatal(err)
	}
	get := func(path string) (int, string) {
		recorder := httptest.NewRecorder()
		srv.Handler().ServeHTTP(recorder, httptest.NewRequest(http.MethodGet, path, nil))
		return recorder.Code, recorder.Body.String()
	}
	base := "/api/git/repositories/" + repository.Repository.ID + "/code/"
	if code, body := get(base + "refs"); code != http.StatusOK || !strings.Contains(body, `"name":"feature"`) || !strings.Contains(body, `"default_ref":"develop"`) {
		t.Fatalf("refs: %d %s", code, body)
	}
	if code, body := get(base + "tree"); code != http.StatusOK || !strings.Contains(body, `"ref":"develop"`) || !strings.Contains(body, `"hello.txt"`) {
		t.Fatalf("tree: %d %s", code, body)
	}
	if code, body := get(base + "file?ref=feature&path=hello.txt"); code != http.StatusOK || !strings.Contains(body, `"content":"hello\n"`) {
		t.Fatalf("file: %d %s", code, body)
	}
	last := engine.browsed[len(engine.browsed)-1]
	if last.RemoteURL != "https://example.com/explore.git" || last.CacheKey != repository.Repository.ID || last.Ref != "feature" {
		t.Fatalf("browse request = %+v", last)
	}
}
