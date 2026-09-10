package server

import (
	"context"
	"errors"
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
	// conflicts is how many merges still fail on a conflict.
	conflicts int
	synced    []capsule.WorkspaceSync
	browsed   []capsule.RepositoryBrowse
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
	if e.conflicts > 0 {
		e.conflicts--
		return capsule.WorkspaceMergeResult{}, errors.New("docker exec failed (exit 45): Auto-merging src/en.json\nCONFLICT (content): Merge conflict in src/en.json\nSPIN_CONFLICT De Job-branch conflicteert met develop in: src/en.json src/nl.json. Merge origin/develop in de Job-branch (die staat al opgehaald, niet fetchen), los de conflicten op, commit de merge; daarna kan de merge in develop opnieuw.: exit status 45")
	}
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

// A Template with its own merge step: a conflict is a reject like any
// other, the next step is an agent that merges the base branch into the
// Job branch and reads the conflict as feedback, its accept returns to the
// merge step, and a clean merge ends the Job.
func TestMergeStepRejectsOnConflictAndTheAIMergeStepReturnsToIt(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &appTestEngine{conflicts: 1}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://github.com/derek/shop.git", DefaultRef: "develop", CredentialScope: domain.CredentialScopePublic,
		Services: []domain.AppService{{Name: "web", Run: "npm start", Ports: []int{3000}}}})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Test, merge, AI merge", Phases: []domain.WorkflowPhase{
		{ID: "test", Name: "Testen", Executor: domain.WorkflowExecutorExpose, Accept: domain.WorkflowTransition{Target: "merge"}, Reject: domain.WorkflowTransition{Target: "SELF"}},
		{ID: "merge", Name: "Merge", Executor: domain.WorkflowExecutorAction, Action: &domain.WorkflowAction{Type: domain.WorkflowActionGitMerge}, Accept: domain.WorkflowTransition{Target: "DONE"}, Reject: domain.WorkflowTransition{Target: "ai-merge"}},
		{ID: "ai-merge", Name: "AI merge", Executor: domain.WorkflowExecutorAgent, Instructions: "Merge origin/develop in de Job-branch en los de conflicten op.", AllowChanges: true, Accept: domain.WorkflowTransition{Target: "merge"}, Reject: domain.WorkflowTransition{Target: "ASK_USER"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	var mergePhase domain.WorkflowPhase
	for _, phase := range template.Phases {
		if phase.ID == "merge" {
			mergePhase = phase
		}
	}
	if mergePhase.Action == nil || mergePhase.Action.Type != domain.WorkflowActionGitMerge || mergePhase.Accept.Target != domain.WorkflowTargetDone || mergePhase.Reject.Target != "ai-merge" {
		t.Fatalf("merge step after normalization = %+v", mergePhase)
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
	if err != nil || advance.NextSession == nil {
		t.Fatalf("accept did not queue the merge step: %+v, %v", advance, err)
	}
	srv.startQueuedWorkflowLaunch(*advance.NextSession)
	// The merge conflicts: the merge step is rejected with the conflict
	// as its reason, and the AI merge step is queued.
	var aiSession domain.Session
	deadline = time.Now().Add(10 * time.Second)
	for aiSession.ID == "" && time.Now().Before(deadline) {
		snapshot := st.Snapshot()
		for _, run := range snapshot.PhaseRuns {
			if run.JobID != created.Job.ID || run.PhaseID != "ai-merge" {
				continue
			}
			for _, session := range snapshot.Sessions {
				if session.ID == run.SessionID {
					aiSession = session
				}
			}
		}
		time.Sleep(10 * time.Millisecond)
	}
	if aiSession.ID == "" {
		var reasons []string
		for _, run := range st.Snapshot().PhaseRuns {
			reasons = append(reasons, run.PhaseID+"/"+string(run.Status)+": "+run.RejectReason)
		}
		t.Fatalf("the conflict did not queue the AI merge step; runs=%v", reasons)
	}
	var mergeRun domain.PhaseRun
	for _, run := range st.Snapshot().PhaseRuns {
		if run.JobID == created.Job.ID && run.PhaseID == "merge" {
			mergeRun = run
		}
	}
	if mergeRun.Status != domain.PhaseRunRejected || !strings.HasPrefix(mergeRun.RejectReason, "De Job-branch conflicteert met develop in: src/en.json src/nl.json.") || strings.Contains(mergeRun.RejectReason, "docker exec") {
		t.Fatalf("merge run after the conflict = %s %q", mergeRun.Status, mergeRun.RejectReason)
	}
	prompt, err := srv.workflowPrompt(aiSession.ID)
	if err != nil || !strings.Contains(prompt, "conflicteert met develop in: src/en.json src/nl.json") || !strings.Contains(prompt, "Merge origin/develop in de Job-branch") {
		t.Fatalf("AI merge prompt = %q, %v", prompt, err)
	}
	// The agent resolves and accepts: back to the merge step, which now
	// merges cleanly and ends the Job.
	if _, err := st.MarkWorkflowPhaseRunning(aiSession.ID); err != nil {
		t.Fatal(err)
	}
	// The agent accepts: the step's accept points at the merge step, so
	// nobody is asked and the merge step is queued again.
	advance, err = st.CompleteWorkflowPhase(aiSession.ID, "accept", "Conflicten opgelost, merge gecommit.")
	if err != nil || advance.Question != nil || advance.NextSession == nil {
		t.Fatalf("accept of the AI merge step did not queue the merge step: %+v, %v", advance, err)
	}
	if _, _, _, nextPhase, _, _, err := st.WorkflowForSession(advance.NextSession.ID); err != nil || nextPhase.ID != "merge" {
		t.Fatalf("accept of the AI merge step went to %q, not back to the merge step (%v)", nextPhase.ID, err)
	}
	srv.startQueuedWorkflowLaunch(*advance.NextSession)
	deadline = time.Now().Add(10 * time.Second)
	var job domain.Job
	for job.WorkflowStatus != domain.WorkflowDone && time.Now().Before(deadline) {
		job = st.Snapshot().Jobs[0]
		time.Sleep(10 * time.Millisecond)
	}
	if job.WorkflowStatus != domain.WorkflowDone || len(engine.merged) != 2 {
		var reasons []string
		for _, run := range st.Snapshot().PhaseRuns {
			reasons = append(reasons, run.PhaseID+"/"+string(run.Status)+": "+run.RejectReason)
		}
		t.Fatalf("job after the second merge = %s, merges=%d; runs=%v", job.WorkflowStatus, len(engine.merged), reasons)
	}
	// The generated finalizer still sits behind the Template for steps that
	// say DONE; the merge step's DONE did not go there.
	if last := template.Phases[len(template.Phases)-1]; last.ID != domain.WorkflowPullRequestPhaseID {
		t.Fatalf("last phase = %+v", last)
	}
}

// A Job runs on a copy of its Template; a newer revision is taken over on
// request: keeping the current step when it still exists, or continuing
// at a chosen step, which closes the current one without a verdict and
// drops its open question.
func TestJobAdoptsANewerTemplateRevision(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &appTestEngine{conflicts: 5}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://github.com/derek/shop.git", DefaultRef: "develop", CredentialScope: domain.CredentialScopePublic,
		Services: []domain.AppService{{Name: "web", Run: "npm start", Ports: []int{3000}}}})
	if err != nil {
		t.Fatal(err)
	}
	// Revision 1: test, then the generated merge finalizer, which conflicts.
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Test en merge", Finalize: domain.WorkflowFinalizeMerge, Phases: []domain.WorkflowPhase{{
		ID: "test", Name: "Testen", Executor: domain.WorkflowExecutorExpose, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: "SELF"},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Objective: "Werkend", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	srv.launchQueuedWorkflowPhases("test")
	openQuestion := func(sessionID string) domain.WorkflowQuestion {
		deadline := time.Now().Add(10 * time.Second)
		for time.Now().Before(deadline) {
			for _, candidate := range st.Snapshot().WorkflowQuestions {
				if (sessionID == "" || candidate.SessionID == sessionID) && candidate.Status == "open" {
					return candidate
				}
			}
			time.Sleep(10 * time.Millisecond)
		}
		t.Fatalf("no open question; failures=%+v", srv.sessionPreparations())
		return domain.WorkflowQuestion{}
	}
	advance, err := st.AnswerWorkflowQuestion(openQuestion(created.Session.ID).ID, "derek", "accept", "")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("accept did not queue the merge: %+v, %v", advance, err)
	}
	srv.startQueuedWorkflowLaunch(*advance.NextSession)
	// The generated finalizer retries twice and then asks the person.
	stuck := openQuestion("")
	if stuck.PhaseRunID == "" {
		t.Fatalf("question = %+v", stuck)
	}
	// Revision 2: the person's own merge step and an AI merge step.
	updated, err := st.UpdateWorkflowTemplate(template.ID, domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Test en merge", Phases: []domain.WorkflowPhase{
		{ID: "test", Name: "Testen", Executor: domain.WorkflowExecutorExpose, Accept: domain.WorkflowTransition{Target: "merge"}, Reject: domain.WorkflowTransition{Target: "SELF"}},
		{ID: "merge", Name: "Merge", Executor: domain.WorkflowExecutorAction, Action: &domain.WorkflowAction{Type: domain.WorkflowActionGitMerge}, Accept: domain.WorkflowTransition{Target: "DONE"}, Reject: domain.WorkflowTransition{Target: "ai-merge"}},
		{ID: "ai-merge", Name: "AI merge", Executor: domain.WorkflowExecutorAgent, Instructions: "Merge origin/develop in de Job-branch.", AllowChanges: true, Accept: domain.WorkflowTransition{Target: "merge"}, Reject: domain.WorkflowTransition{Target: "ASK_USER"}},
	}})
	if err != nil || updated.Revision != 2 {
		t.Fatalf("update = r%d, %v", updated.Revision, err)
	}
	if job := st.Snapshot().Jobs[0]; job.TemplateSnapshot == nil || job.TemplateSnapshot.Revision != 1 {
		t.Fatalf("the update changed the running Job's copy: %+v", job.TemplateSnapshot)
	}
	// Keeping the current step works: the generated finalizer exists in
	// both revisions.
	kept, _, launched, err := st.AdoptWorkflowTemplate(created.Job.ID, "derek", "")
	if err != nil || launched || kept.Job.TemplateSnapshot == nil || kept.Job.TemplateSnapshot.Revision != 2 {
		t.Fatalf("adopt keeping the step = %+v launched=%v, %v", kept.Job.TemplateSnapshot, launched, err)
	}
	if _, _, _, err := st.AdoptWorkflowTemplate(created.Job.ID, "derek", "nonsense"); err == nil {
		t.Fatal("an unknown step was accepted")
	}
	if _, _, _, err := st.AdoptWorkflowTemplate(created.Job.ID, "john", "ai-merge"); err == nil {
		t.Fatal("someone else moved the Job")
	}
	// Continuing at the AI merge step: the stuck question is gone, the
	// finalizer run is closed without a verdict, and the new step is queued.
	moved, previousComposition, launched, err := st.AdoptWorkflowTemplate(created.Job.ID, "derek", "ai-merge")
	if err != nil || !launched || moved.Session.ID == "" {
		t.Fatalf("adopt at a step = %+v launched=%v, %v", moved, launched, err)
	}
	_ = previousComposition
	snapshot := st.Snapshot()
	for _, question := range snapshot.WorkflowQuestions {
		if question.ID == stuck.ID && question.Status == "open" {
			t.Fatal("the stuck question is still open")
		}
	}
	var closedRun, newRun domain.PhaseRun
	for _, run := range snapshot.PhaseRuns {
		if run.ID == stuck.PhaseRunID {
			closedRun = run
		}
		if run.SessionID == moved.Session.ID {
			newRun = run
		}
	}
	if closedRun.Status != domain.PhaseRunAccepted || !strings.Contains(closedRun.Summary, "Overgezet naar stap AI merge") {
		t.Fatalf("closed run = %+v", closedRun)
	}
	if newRun.PhaseID != "ai-merge" || newRun.Status != domain.PhaseRunQueued || snapshot.Jobs[0].CurrentPhaseRunID != newRun.ID || snapshot.Jobs[0].WorkflowStatus != domain.WorkflowBusy {
		t.Fatalf("new run = %+v, job = %+v", newRun, snapshot.Jobs[0])
	}
	// The move itself is not feedback for the AI merge step; the conflict
	// the old step ran into is.
	prompt, err := srv.workflowPrompt(moved.Session.ID)
	if err != nil || strings.Contains(prompt, "Overgezet") || !strings.Contains(prompt, "conflicteert met develop") {
		t.Fatalf("prompt = %q, %v", prompt, err)
	}
	// From here the AI merge step's accept returns to the person's merge
	// step of revision 2, which merges cleanly and ends the Job.
	engine.mu.Lock()
	engine.conflicts = 0
	engine.mu.Unlock()
	if _, err := st.MarkWorkflowPhaseRunning(moved.Session.ID); err != nil {
		t.Fatal(err)
	}
	advance, err = st.CompleteWorkflowPhase(moved.Session.ID, "accept", "Opgelost.")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("accept of the AI merge step = %+v, %v", advance, err)
	}
	if _, _, _, nextPhase, _, _, err := st.WorkflowForSession(advance.NextSession.ID); err != nil || nextPhase.ID != "merge" {
		t.Fatalf("next phase = %q, %v", nextPhase.ID, err)
	}
	srv.startQueuedWorkflowLaunch(*advance.NextSession)
	deadline := time.Now().Add(10 * time.Second)
	var job domain.Job
	for job.WorkflowStatus != domain.WorkflowDone && time.Now().Before(deadline) {
		job = st.Snapshot().Jobs[0]
		time.Sleep(10 * time.Millisecond)
	}
	if job.WorkflowStatus != domain.WorkflowDone {
		t.Fatalf("job = %s", job.WorkflowStatus)
	}
}
