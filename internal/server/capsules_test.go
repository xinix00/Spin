package server

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

type testEngine struct {
	started        int
	executed       int
	sealed         int
	materialized   int
	probed         int
	removed        int
	removedRef     string
	materializeErr error
	gitAuth        *capsule.GitAuthentication
	accepted       []capsule.WorkspaceAcceptance
	comparisons    []capsule.WorkspaceComparison
	injected       []capsule.WorkspaceAttachment
}

type blockingMaterializeEngine struct {
	testEngine
	started chan struct{}
	release chan struct{}
	once    sync.Once
}

type blockingStopEngine struct {
	testEngine
	started chan struct{}
	release chan struct{}
	once    sync.Once
}

func (e *blockingMaterializeEngine) Materialize(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact) (domain.CapsuleRuntime, error) {
	e.once.Do(func() { close(e.started) })
	select {
	case <-e.release:
		return e.testEngine.Materialize(ctx, composition, artifacts)
	case <-ctx.Done():
		return domain.CapsuleRuntime{}, ctx.Err()
	}
}

func (e *blockingStopEngine) Stop(ctx context.Context, _ domain.CapsuleRuntime) error {
	e.once.Do(func() { close(e.started) })
	select {
	case <-e.release:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func waitForPreparedSession(t *testing.T, st *store.Store, sessionID string) (domain.Session, domain.Composition) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		snapshot := st.Snapshot()
		for _, session := range snapshot.Sessions {
			if session.ID != sessionID || session.PreparedCompositionID == "" {
				continue
			}
			for _, composition := range snapshot.Compositions {
				if composition.ID == session.PreparedCompositionID && composition.Runtime != nil && composition.Runtime.Status == "ready" {
					return session, composition
				}
			}
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("Session %s was not prepared in the background", sessionID)
	return domain.Session{}, domain.Composition{}
}

func (e *testEngine) Info() domain.CapsuleEngineInfo {
	return domain.CapsuleEngineInfo{Driver: "test", Available: true, FilesystemSnapshots: true}
}

func (e *testEngine) StartRecording(_ context.Context, recording domain.Recording, parents []domain.Artifact) (domain.CapsuleRuntime, error) {
	e.started++
	base := "base:test"
	if len(parents) == 1 {
		base = parents[0].Snapshot.Ref
	}
	return domain.CapsuleRuntime{Driver: "test", ContainerID: "container-" + recording.ID, BaseRef: base, AttachCommand: "attach " + recording.ID, Status: "recording"}, nil
}

func (e *testEngine) Execute(_ context.Context, _ domain.Recording, input string) (capsule.Execution, error) {
	e.executed++
	return capsule.Execution{Output: "ran: " + input, ExitCode: 7}, nil
}

func (e *testEngine) Seal(_ context.Context, recording domain.Recording) (domain.CapsuleSnapshot, error) {
	e.sealed++
	return domain.CapsuleSnapshot{Driver: "test", Ref: "image:" + recording.ID, Digest: "sha256:test", Restorable: true}, nil
}

func (e *testEngine) Cancel(_ context.Context, _ domain.Recording) error { return nil }

func (e *testEngine) Materialize(_ context.Context, composition domain.Composition, _ []domain.Artifact) (domain.CapsuleRuntime, error) {
	e.materialized++
	if e.materializeErr != nil {
		return domain.CapsuleRuntime{}, e.materializeErr
	}
	return domain.CapsuleRuntime{Driver: "test", ContainerID: "use-" + composition.ID, AttachCommand: "attach " + composition.ID, Status: "ready"}, nil
}

func (e *testEngine) MaterializeWithGitAuthentication(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact, authentication *capsule.GitAuthentication) (domain.CapsuleRuntime, error) {
	if authentication != nil {
		copy := *authentication
		e.gitAuth = &copy
	}
	return e.Materialize(ctx, composition, artifacts)
}

func (e *testEngine) Stop(_ context.Context, _ domain.CapsuleRuntime) error { return nil }

func (e *testEngine) RemoveSnapshot(_ context.Context, snapshot domain.CapsuleSnapshot) error {
	e.removed++
	e.removedRef = snapshot.Ref
	return nil
}

func (e *testEngine) AcceptWorkspace(_ context.Context, _ domain.CapsuleRuntime, acceptance capsule.WorkspaceAcceptance) (capsule.WorkspaceAcceptanceResult, error) {
	e.accepted = append(e.accepted, acceptance)
	return capsule.WorkspaceAcceptanceResult{Head: "accepted-head", Committed: acceptance.AllowChanges}, nil
}

func (e *testEngine) InspectWorkspace(_ context.Context, _ domain.CapsuleRuntime) (capsule.WorkspaceChanges, error) {
	return capsule.WorkspaceChanges{Branch: "session-branch", Added: 2, Files: []capsule.WorkspaceFileChange{{Path: "active.go", Status: "M ", Added: 2, Patch: "@@ -1 +1,2 @@\n old\n+new\n"}}}, nil
}

func (e *testEngine) InspectWorkspaceRange(_ context.Context, _ domain.CapsuleRuntime, comparison capsule.WorkspaceComparison) (capsule.WorkspaceChanges, error) {
	e.comparisons = append(e.comparisons, comparison)
	return capsule.WorkspaceChanges{Branch: comparison.HeadRef, Added: 1, Files: []capsule.WorkspaceFileChange{{Path: "result.go", Status: "M ", Added: 1, Patch: "@@ -1 +1,2 @@\n old\n+new\n"}}}, nil
}

func (e *testEngine) InjectWorkspaceAttachments(_ context.Context, _ domain.CapsuleRuntime, attachments []capsule.WorkspaceAttachment) error {
	e.injected = append(e.injected, attachments...)
	return nil
}

func (e *testEngine) ProbeEnabled(_ context.Context, _ domain.CapsuleRuntime, enabled domain.Enablement, request json.RawMessage) (json.RawMessage, error) {
	e.probed++
	var envelope map[string]any
	if err := json.Unmarshal(request, &envelope); err != nil {
		return nil, err
	}
	if envelope["method"] != "initialize" || enabled.Name != "acp" {
		return nil, fmt.Errorf("unexpected probe: enabled=%+v request=%s", enabled, request)
	}
	return json.RawMessage(`{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}`), nil
}

func TestStateUsesArraysForEmptyCollections(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), capsule.Journal{}, ServerOptions{DisableAuthentication: true})
	request := httptest.NewRequest(http.MethodGet, "/api/state", nil)
	response := httptest.NewRecorder()
	srv.Handler().ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("status = %d; body = %s", response.Code, response.Body.String())
	}
	var body map[string]json.RawMessage
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	for _, key := range []string{"artifacts", "recordings", "compositions", "jobs", "job_attachments", "workflow_templates", "phase_runs", "deliverables", "deliverable_comments", "workflow_questions", "sessions", "activations", "turns", "checkpoints", "results", "clients", "mcp_servers", "git_repositories", "git_accounts", "users", "recommendations"} {
		if string(body[key]) != "[]" {
			t.Errorf("%s = %s, want []", key, body[key])
		}
	}
}

func TestDeleteArtifactRemovesTheBackingSnapshot(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &testEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	run := func(line string) domain.CommandResponse {
		t.Helper()
		response, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line})
		if err != nil {
			t.Fatalf("%s: %v", line, err)
		}
		return response
	}
	run("RECORD tool:disposable --scope=global")
	artifact := run("END RECORD").Artifact
	if artifact == nil {
		t.Fatal("recording returned no artifact")
	}

	request := httptest.NewRequest(http.MethodDelete, "/api/artifacts/"+artifact.ID+"?operator=derek", nil)
	response := httptest.NewRecorder()
	srv.Handler().ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("status = %d; body = %s", response.Code, response.Body.String())
	}
	if engine.removed != 1 || engine.removedRef != artifact.Snapshot.Ref {
		t.Fatalf("snapshot removal = calls:%d ref:%q", engine.removed, engine.removedRef)
	}
	if len(st.Snapshot().Artifacts) != 0 {
		t.Fatalf("artifact metadata survived deletion: %+v", st.Snapshot().Artifacts)
	}
}

func TestMCPAPIIsUserScopedAndRedactsCredentials(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), capsule.Journal{}, ServerOptions{DisableAuthentication: true})
	body := bytes.NewBufferString(`{"operator":"derek","name":"github","transport":"stdio","command":"/usr/local/bin/github-mcp","args":["--stdio"],"env":[{"name":"GITHUB_TOKEN","value":"secret"}]}`)
	request := httptest.NewRequest(http.MethodPost, "/api/mcp-servers", body)
	request.Header.Set("Content-Type", "application/json")
	response := httptest.NewRecorder()
	srv.Handler().ServeHTTP(response, request)
	if response.Code != http.StatusCreated {
		t.Fatalf("status = %d; body = %s", response.Code, response.Body.String())
	}
	var created domain.MCPServer
	if err := json.Unmarshal(response.Body.Bytes(), &created); err != nil {
		t.Fatal(err)
	}
	if len(created.Env) != 1 || created.Env[0].Value != "" || strings.Contains(response.Body.String(), "secret") {
		t.Fatalf("MCP response leaked a credential: %s", response.Body.String())
	}
	private, err := st.MCPServersForOperator("derek", []string{created.ID})
	if err != nil || private[0].Env[0].Value != "secret" {
		t.Fatalf("private MCP config = %+v, error = %v", private, err)
	}

	deleteRequest := httptest.NewRequest(http.MethodDelete, "/api/mcp-servers/"+created.ID+"?operator=john", nil)
	deleteResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(deleteResponse, deleteRequest)
	if deleteResponse.Code != http.StatusConflict {
		t.Fatalf("other-user delete status = %d; body = %s", deleteResponse.Code, deleteResponse.Body.String())
	}
}

func TestCommandFlowUsesCapsuleEngine(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &testEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})

	run := func(line string) domain.CommandResponse {
		t.Helper()
		response, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line})
		if err != nil {
			t.Fatalf("%s: %v", line, err)
		}
		return response
	}

	started := run("RECORD tool:codex --scope=global")
	if started.Recording == nil || started.Recording.Runtime == nil || started.Recording.Runtime.Driver != "test" {
		t.Fatalf("recording runtime = %+v", started.Recording)
	}
	executed := run("install codex")
	if executed.Output != "ran: install codex" || executed.ExitCode == nil || *executed.ExitCode != 7 {
		t.Fatalf("execution = %+v", executed)
	}
	artifact := run("END RECORD").Artifact
	if artifact == nil || artifact.Snapshot.Ref == "" || !artifact.Snapshot.Restorable {
		t.Fatalf("artifact = %+v", artifact)
	}
	composition := run("USE tool:codex").Composition
	if composition == nil || composition.Runtime == nil || composition.Runtime.Status != "ready" {
		t.Fatalf("composition = %+v", composition)
	}
	stopped := run("STOP USE").Composition
	if stopped == nil || stopped.Runtime == nil || stopped.Runtime.Status != "stopped" {
		t.Fatalf("stopped composition = %+v", stopped)
	}
	if engine.started != 1 || engine.executed != 1 || engine.sealed != 1 || engine.materialized != 1 {
		t.Fatalf("engine calls = %+v", engine)
	}
}

func TestFailedMaterializationDoesNotLeaveADeadComposition(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &testEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	run := func(line string) {
		t.Helper()
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
	}
	run("RECORD tool:codex --scope=global")
	run("END RECORD")
	run("RECORD tool:dotnet --scope=global")
	run("END RECORD")
	engine.materializeErr = fmt.Errorf("independent Docker lineages")

	if _, err := srv.useCapsule(context.Background(), domain.UseRequest{
		Selector: "tool:codex", WithSelectors: []string{"tool:dotnet"}, Operator: "derek",
	}); err == nil {
		t.Fatal("failed materialization unexpectedly succeeded")
	}
	if got := len(st.Snapshot().Compositions); got != 0 {
		t.Fatalf("failed materialization left %d dead composition(s)", got)
	}
}

func TestRecordFromBuildsExplicitToolAndCredentialLayers(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &testEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	run := func(line string) domain.CommandResponse {
		t.Helper()
		response, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line})
		if err != nil {
			t.Fatalf("%s: %v", line, err)
		}
		return response
	}

	run("RECORD tool:node --scope=global")
	run("apk add nodejs npm")
	node := run("END RECORD").Artifact
	if node == nil || node.Slot != "tool:node" {
		t.Fatalf("node layer = %+v", node)
	}

	codexRecording := run("RECORD tool:codex --scope=global --from=tool:node --enable=acp --command=codex-acp").Recording
	if codexRecording == nil || len(codexRecording.ParentArtifactIDs) != 1 || codexRecording.ParentArtifactIDs[0] != node.ID {
		t.Fatalf("codex recording parents = %+v", codexRecording)
	}
	if codexRecording.Runtime == nil || codexRecording.Runtime.BaseRef != node.Snapshot.Ref {
		t.Fatalf("codex runtime = %+v", codexRecording.Runtime)
	}
	run("npm install -g @openai/codex")
	codex := run("END RECORD").Artifact

	credentialRecording := run("RECORD credential:codex --scope=user --from=tool:codex").Recording
	if credentialRecording == nil || len(credentialRecording.ParentArtifactIDs) != 1 || credentialRecording.ParentArtifactIDs[0] != codex.ID {
		t.Fatalf("credential recording = %+v", credentialRecording)
	}
}

func TestACPProbeUsesEnabledEntrypointInMaterializedCapsule(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &testEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	run := func(line string) domain.CommandResponse {
		t.Helper()
		response, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line})
		if err != nil {
			t.Fatalf("%s: %v", line, err)
		}
		return response
	}

	run("RECORD tool:codex --scope=global --enable=acp --command=codex-acp")
	run("install codex-acp")
	run("END RECORD")
	composition := run("USE tool:codex").Composition
	if composition == nil || len(composition.Enabled) != 1 {
		t.Fatalf("composition = %+v", composition)
	}
	probed := run("ACP PROBE " + composition.ID)
	if engine.probed != 1 || !strings.Contains(probed.Output, `"protocolVersion": 1`) {
		t.Fatalf("probe = %+v, calls=%d", probed, engine.probed)
	}
}

func TestCreateJobQueuesRootSessionWithoutWaitingAndCanRemoveIt(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &blockingMaterializeEngine{started: make(chan struct{}), release: make(chan struct{})}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "RECORD tool:git --scope=global --enable=git"}); err != nil {
		t.Fatal(err)
	}
	if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "install git"}); err != nil {
		t.Fatal(err)
	}
	if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "END RECORD"}); err != nil {
		t.Fatal(err)
	}
	if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "RECORD tool:demo --scope=global --from=tool:git --enable=acp --command=demo-acp"}); err != nil {
		t.Fatal(err)
	}
	if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "install demo"}); err != nil {
		t.Fatal(err)
	}
	if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "END RECORD"}); err != nil {
		t.Fatal(err)
	}
	if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "RECORD tool:dotnet --scope=global --from=tool:demo"}); err != nil {
		t.Fatal(err)
	}
	if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "install dotnet"}); err != nil {
		t.Fatal(err)
	}
	if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "END RECORD"}); err != nil {
		t.Fatal(err)
	}

	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "rudimentary", RemoteURL: "https://example.com/rudimentary.git"})
	if err != nil {
		t.Fatal(err)
	}
	body := `{"title":"Rudimentary run","objective":"Start de capsule","idempotency_key":"click-burst-1","owner":"john","operator":"derek","git_repository_id":"` + repository.Repository.ID + `","environment_selector":"tool:demo","with_selectors":["tool:dotnet"],"run":true}`
	request := httptest.NewRequest(http.MethodPost, "/api/jobs", bytes.NewBufferString(body))
	request.Header.Set("Content-Type", "application/json")
	response := httptest.NewRecorder()
	responseDone := make(chan struct{})
	go func() {
		srv.Handler().ServeHTTP(response, request)
		close(responseDone)
	}()
	select {
	case <-responseDone:
	case <-time.After(500 * time.Millisecond):
		close(engine.release)
		t.Fatal("Job creation waited for background materialization")
	}
	if response.Code != http.StatusAccepted {
		t.Fatalf("status = %d; body = %s", response.Code, response.Body.String())
	}
	var created domain.CreateJobResponse
	if err := json.Unmarshal(response.Body.Bytes(), &created); err != nil {
		t.Fatal(err)
	}
	if created.RunError != "" || created.Composition != nil || created.Session.PreparedCompositionID != "" || created.Replayed {
		t.Fatalf("Job creation did not return before materialization: %+v", created)
	}
	select {
	case <-engine.started:
	case <-time.After(time.Second):
		t.Fatal("background materialization was not scheduled")
	}
	replayRequest := httptest.NewRequest(http.MethodPost, "/api/jobs", bytes.NewBufferString(body))
	replayRequest.Header.Set("Content-Type", "application/json")
	replayResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(replayResponse, replayRequest)
	var replayed domain.CreateJobResponse
	if replayResponse.Code != http.StatusAccepted || json.Unmarshal(replayResponse.Body.Bytes(), &replayed) != nil || !replayed.Replayed || replayed.Job.ID != created.Job.ID || len(st.Snapshot().Jobs) != 1 {
		t.Fatalf("idempotent click replay status=%d response=%+v jobs=%d", replayResponse.Code, replayed, len(st.Snapshot().Jobs))
	}
	close(engine.release)
	preparedSession, composition := waitForPreparedSession(t, st, created.Session.ID)
	if preparedSession.PreparedCompositionID != composition.ID || preparedSession.EnvironmentSelector != "tool:demo" || preparedSession.Operator != "derek" {
		t.Fatalf("session = %+v", created.Session)
	}
	if len(preparedSession.WithSelectors) != 1 || preparedSession.WithSelectors[0] != "tool:dotnet" || len(composition.RequestedArtifactIDs) != 2 {
		t.Fatalf("session WITH stack = session:%+v composition:%+v", preparedSession, composition)
	}
	if !strings.HasPrefix(created.Job.Branch, "jobs/rudimentary-run-") || !strings.HasSuffix(created.Job.Branch, "/main") || !strings.HasPrefix(preparedSession.GitRef, strings.TrimSuffix(created.Job.Branch, "/main")+"/sessions/") || preparedSession.TargetBranch != created.Job.Branch || composition.Git == nil {
		t.Fatalf("branch topology = job:%+v session:%+v", created.Job, preparedSession)
	}
	if engine.materialized != 1 {
		t.Fatalf("materialized = %d, want 1", engine.materialized)
	}

	childBody := bytes.NewBufferString(`{"operator":"derek","environment_selector":"tool:demo","objective_delta":"Review de primary Session","role":"reviewer","spawned_by_session_id":"` + created.Session.ID + `","run":true}`)
	childRequest := httptest.NewRequest(http.MethodPost, "/api/jobs/"+created.Job.ID+"/sessions", childBody)
	childRequest.Header.Set("Content-Type", "application/json")
	childResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(childResponse, childRequest)
	if childResponse.Code != http.StatusCreated {
		t.Fatalf("child status = %d; body = %s", childResponse.Code, childResponse.Body.String())
	}
	var child domain.CreateJobSessionResponse
	if err := json.Unmarshal(childResponse.Body.Bytes(), &child); err != nil {
		t.Fatal(err)
	}
	if child.RunError != "" || child.Composition == nil || child.Session.SpawnedBySessionID != created.Session.ID || child.Session.Role != "reviewer" || child.Session.TargetBranch != created.Job.Branch {
		t.Fatalf("child session = %+v", child)
	}
	if len(child.Session.WithSelectors) != 1 || child.Session.WithSelectors[0] != "tool:dotnet" {
		t.Fatalf("child did not inherit WITH layers: %+v", child.Session.WithSelectors)
	}
	if engine.materialized != 2 {
		t.Fatalf("materialized = %d, want 2", engine.materialized)
	}
	deleteRequest := httptest.NewRequest(http.MethodDelete, "/api/jobs/"+created.Job.ID+"?operator=john", nil)
	deleteResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(deleteResponse, deleteRequest)
	if deleteResponse.Code != http.StatusOK {
		t.Fatalf("delete status = %d; body = %s", deleteResponse.Code, deleteResponse.Body.String())
	}
	snapshot := st.Snapshot()
	if len(snapshot.Jobs) != 0 || len(snapshot.Sessions) != 0 || len(snapshot.Compositions) != 0 || len(snapshot.GitRepositories) != 1 {
		t.Fatalf("Job cleanup left state behind: jobs=%d sessions=%d compositions=%d repositories=%d", len(snapshot.Jobs), len(snapshot.Sessions), len(snapshot.Compositions), len(snapshot.GitRepositories))
	}
}

func TestAuthenticatedGitCheckoutUsesSecretEngineBoundary(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &testEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	for _, line := range []string{
		"RECORD tool:git --scope=global --enable=git", "install git", "END RECORD",
		"RECORD tool:demo --scope=global --from=tool:git --enable=acp --command=demo-acp", "install demo", "END RECORD",
	} {
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
	}
	_, err = st.CreateGitAccount(domain.CreateGitAccountRequest{
		Operator: "derek", Provider: "github", Login: "derek", Name: "Derek", Email: "derek@example.com", AccessToken: "top-secret-token",
	})
	if err != nil {
		t.Fatal(err)
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{
		Operator: "derek", Name: "private", RemoteURL: "https://github.com/example/private.git", CredentialScope: domain.CredentialScopeUser,
	})
	if err != nil {
		t.Fatal(err)
	}
	body := bytes.NewBufferString(`{"title":"Private","objective":"Checkout","operator":"derek","git_repository_id":"` + repository.Repository.ID + `","environment_selector":"tool:demo","run":true}`)
	request := httptest.NewRequest(http.MethodPost, "/api/jobs", body)
	request.Header.Set("Content-Type", "application/json")
	response := httptest.NewRecorder()
	srv.Handler().ServeHTTP(response, request)
	if response.Code != http.StatusAccepted {
		t.Fatalf("status = %d; body = %s", response.Code, response.Body.String())
	}
	var created domain.CreateJobResponse
	if err := json.Unmarshal(response.Body.Bytes(), &created); err != nil {
		t.Fatal(err)
	}
	waitForPreparedSession(t, st, created.Session.ID)
	if engine.gitAuth == nil || engine.gitAuth.Username != "derek" || engine.gitAuth.Password != "top-secret-token" || engine.gitAuth.AuthorEmail != "derek@example.com" {
		t.Fatalf("checkout authentication = %+v", engine.gitAuth)
	}
	if strings.Contains(response.Body.String(), "top-secret-token") {
		t.Fatal("job response leaked the Git token")
	}
}
