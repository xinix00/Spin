package server

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"sync"
	"testing"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
	"easyacp/internal/worker"
)

// awayRunnerEngine is the tracked test engine on a runner that can be away.
type awayRunnerEngine struct {
	trackedTestEngine
	away    bool
	stopped int
}

func (e *awayRunnerEngine) Stop(context.Context, domain.CapsuleRuntime) error {
	if e.away {
		return fmt.Errorf("stop: %w", worker.ErrRunnerOffline)
	}
	e.stopped++
	return nil
}

// A capsule whose runner is away is not stopped behind its back: it runs
// on there, on its login, so the login stays with it and what it refreshes
// meanwhile is kept once the runner is back and the stop is done.
func TestCapsuleOfAnAwayRunnerKeepsItsLoginUntilItIsBack(t *testing.T) {
	const path = "/root/.codex/auth.json"
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &awayRunnerEngine{trackedTestEngine: trackedTestEngine{image: map[string][]byte{path: []byte("token-1")}, files: map[string]map[string][]byte{}}}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	layers := buildLayers(t, srv, "derek", gitLayer(), layerSpec{Kind: domain.ArtifactCredential, Name: "codex", Scope: domain.ScopeUser, From: "tool:git", Install: "login"})
	if _, err := st.SetArtifactTrackedPaths(layers[1].ID, []string{path}); err != nil {
		t.Fatal(err)
	}
	key := store.LayerKey(layers[1])
	ctx := context.Background()
	first := useLayers(t, srv, "derek", "credential:codex")

	engine.away = true
	if _, err := srv.stopCapsule(ctx, first.ID, "derek"); !errors.Is(err, worker.ErrRunnerOffline) {
		t.Fatalf("stop with the runner away: err=%v", err)
	}
	held, err := st.Composition(first.ID)
	if err != nil {
		t.Fatal(err)
	}
	if held.Runtime.Status == "stopped" || !held.Runtime.StopPending {
		t.Fatalf("runtime after a stop the runner did not hear = %+v", held.Runtime)
	}
	if _, err := srv.useCapsule(ctx, domain.UseRequest{Operator: "derek", Selector: "credential:codex", Profile: "default"}); !errors.Is(err, store.ErrLoginsBusy) {
		t.Fatalf("a second capsule got the login of one that still runs: err=%v", err)
	}
	// The sweep leaves a stop alone while its runner is away.
	srv.sweepIdleCapsules()
	if engine.stopped != 0 {
		t.Fatal("the sweep stopped a capsule on a runner that is away")
	}

	// The agent refreshes its token while the runner is away; the runner
	// comes back, and the stop is done with the new token kept.
	engine.capsule(first.Runtime.ContainerID)[path] = []byte("token-1b")
	engine.away = false
	srv.runnerAttached(first.Runtime.ClientID)
	stopped, err := st.Composition(first.ID)
	if err != nil {
		t.Fatal(err)
	}
	if stopped.Runtime.Status != "stopped" || stopped.Runtime.StopPending || engine.stopped != 1 {
		t.Fatalf("runtime once the runner is back = %+v (stops %d)", stopped.Runtime, engine.stopped)
	}
	if login, _ := st.Login(first.Logins[key]); string(login.Files[path]) != "token-1b" {
		t.Fatalf("login after the stop holds %q, want the refreshed token", login.Files[path])
	}
	second := useLayers(t, srv, "derek", "credential:codex")
	if got := engine.token(second.Runtime.ContainerID, path); got != "token-1b" {
		t.Fatalf("the next capsule starts with %q", got)
	}
}

// A capsule of a Job stays open while the Job goes on, whatever its step
// does; the Job's end closes it.
func TestJobCapsuleStaysOpenUntilTheJobEnds(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &acpTestEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true, InternalURL: "http://spin.internal"})
	created := restartTestJob(t, srv, st)
	srv.resumeQueuedWorkflowPhases()
	composition := waitForSessionCapsule(t, st, created.Session.ID)

	srv.sweepIdleCapsules()
	if current, _ := st.Composition(composition.ID); current.Runtime.Status == "stopped" {
		t.Fatal("the sweep closed the capsule of a Job that goes on")
	}
	if _, err := st.CloseJob(created.Job.ID, "derek"); err != nil {
		t.Fatal(err)
	}
	srv.sweepIdleCapsules()
	if current, _ := st.Composition(composition.ID); current.Runtime.Status != "stopped" {
		t.Fatalf("capsule of a closed Job = %+v", current.Runtime)
	}
}

func restartTestJob(t *testing.T, srv *Server, st *store.Store) domain.CreateJobResponse {
	t.Helper()
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://github.com/derek/shop.git", DefaultRef: "main", CredentialScope: domain.CredentialScopePublic})
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
	return created
}

func waitForSessionCapsule(t *testing.T, st *store.Store, sessionID string) domain.Composition {
	t.Helper()
	var found domain.Composition
	waitUntil(t, "a running capsule for the Session", func() bool {
		for _, composition := range st.RunningCompositions() {
			if composition.SessionID == sessionID {
				found = composition
				return true
			}
		}
		return false
	})
	return found
}

func waitUntil(t *testing.T, what string, condition func() bool) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for !condition() {
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %s", what)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

// runnerStream is an agent process as a runner keeps it: scripted, with a
// stream ID the server can find it back by.
type runnerStream struct {
	*scriptedACPProcess
	id string
}

func (s runnerStream) StreamID() string { return s.id }

// agentScript plays an ACP agent on a process: it answers initialize and
// session/new, and reports the prompts it gets without ending their turn.
func agentScript(process *scriptedACPProcess, prompts chan<- string) {
	scanner := bufio.NewScanner(process.inputReader)
	for scanner.Scan() {
		var envelope acpEnvelope
		if json.Unmarshal(scanner.Bytes(), &envelope) != nil {
			continue
		}
		switch envelope.Method {
		case "initialize":
			process.send(map[string]any{"jsonrpc": "2.0", "id": envelope.ID, "result": map[string]any{"protocolVersion": 1, "agentInfo": map[string]string{"name": "agent"}, "agentCapabilities": map[string]any{"mcpCapabilities": map[string]bool{"http": true}}}})
		case "session/new":
			process.send(map[string]any{"jsonrpc": "2.0", "id": envelope.ID, "result": map[string]any{"sessionId": "agent-session-1"}})
		case "session/prompt":
			process.send(map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{"sessionId": "agent-session-1", "update": map[string]string{"sessionUpdate": "agent_message_chunk"}}})
			prompts <- string(envelope.ID)
		}
	}
}

// runnerEngine starts agents as a runner does and takes them up again.
type runnerEngine struct {
	acpTestEngine
	mu      sync.Mutex
	starts  int
	streams map[string]runnerStream
}

func (e *runnerEngine) StartEnabled(context.Context, domain.CapsuleRuntime, domain.Enablement) (capsule.EnabledProcess, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.starts++
	stream := runnerStream{scriptedACPProcess: newScriptedACPProcess(), id: fmt.Sprintf("str_%d", e.starts)}
	e.streams[stream.id] = stream
	return stream, nil
}

func (e *runnerEngine) AdoptEnabled(_ domain.CapsuleRuntime, streamID string) (capsule.EnabledProcess, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	// The runner's output of the stream now goes to the new server.
	stream := runnerStream{scriptedACPProcess: newScriptedACPProcess(), id: streamID}
	e.streams[streamID] = stream
	return stream, nil
}

func (e *runnerEngine) stream(id string) runnerStream {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.streams[id]
}

// Spin restarts while an agent is in the middle of a turn. The runner keeps
// the agent and what it writes; the new server takes the same agent up,
// starts no second one on the same login, and the turn ends as it would
// have: the answer to the earlier server's prompt settles the step.
func TestRestartTakesUpTheAgentInTheMiddleOfItsTurn(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	engine := &runnerEngine{streams: map[string]runnerStream{}}
	before := NewWithOptions(st, logger, engine, ServerOptions{DisableAuthentication: true, InternalURL: "http://spin.internal"})
	created := restartTestJob(t, before, st)
	prompts := make(chan string, 4)
	// The first agent answers as soon as the launch starts it.
	go func() {
		waitUntil(t, "the agent to start", func() bool { return engine.stream("str_1").scriptedACPProcess != nil })
		agentScript(engine.stream("str_1").scriptedACPProcess, prompts)
	}()
	before.resumeQueuedWorkflowPhases()
	var promptID string
	select {
	case promptID = <-prompts:
	case <-time.After(10 * time.Second):
		for _, p := range before.sessionPreparations() {
			if p.Failure != nil {
				t.Fatalf("the launch sent no prompt: %s", p.Failure.Error)
			}
		}
		t.Fatal("the launch sent no prompt")
	}
	composition := waitForSessionCapsule(t, st, created.Session.ID)
	waitUntil(t, "the running turn to be kept", func() bool {
		current, _ := st.Composition(composition.ID)
		return current.Agent != nil && current.Agent.PromptID == promptID
	})
	kept, _ := st.Composition(composition.ID)
	if kept.Agent.StreamID != "str_1" || kept.Agent.AgentSessionID != "agent-session-1" || kept.Agent.SessionID != created.Session.ID || !kept.Agent.Primed {
		t.Fatalf("kept agent = %+v", kept.Agent)
	}
	if st.WorkflowToken(created.Session.ID) == "" {
		t.Fatal("the agent's workflow token is not kept")
	}

	// Spin restarts: a new server on the same state.
	after := NewWithOptions(st, logger, engine, ServerOptions{DisableAuthentication: true, InternalURL: "http://spin.internal"})
	adopted := after.runningACP(created.Session.ID)
	if adopted == nil || !adopted.isBusy() {
		t.Fatalf("the restarted server did not take up the busy agent: %+v", adopted)
	}
	if agentSessionID, _, _ := adopted.info(); agentSessionID != "agent-session-1" {
		t.Fatalf("taken-up agent session = %q", agentSessionID)
	}
	for _, run := range st.Snapshot().PhaseRuns {
		if run.ID == created.Session.PhaseRunID && run.Status != domain.PhaseRunRunning {
			t.Fatalf("the step of the taken-up agent went to %q", run.Status)
		}
	}
	// The turn ends; the runner hands the answer to the new server.
	engine.stream("str_1").send(map[string]any{"jsonrpc": "2.0", "id": json.RawMessage(promptID), "result": map[string]string{"stopReason": "end_turn"}})
	waitUntil(t, "the taken-up turn to end", func() bool { return !adopted.isBusy() })
	waitUntil(t, "the kept turn to be cleared", func() bool {
		current, _ := st.Composition(composition.ID)
		return current.Agent != nil && current.Agent.PromptID == ""
	})
	engine.mu.Lock()
	starts := engine.starts
	engine.mu.Unlock()
	if starts != 1 {
		t.Fatalf("started %d agents; a restart must not start a second one", starts)
	}
}

// A reply buffered while Spin was down must have a waiter before reading
// starts. Pending permission questions must also remain answerable.
func TestAdoptedAgentReceivesBufferedReplyAndRestoresPermission(t *testing.T) {
	process := &runnerStream{scriptedACPProcess: newScriptedACPProcess(), id: "str_kept"}
	record := domain.AgentProcess{
		SessionID: "ses_kept", StreamID: process.id, AgentSessionID: "agent-kept", PromptID: "7",
		PendingPermissions: map[string]json.RawMessage{"12": json.RawMessage(`{"options":[{"optionId":"allow","kind":"allow_once","name":"Allow"}]}`)},
	}
	active := adoptedACP("cmp_kept", record, process)
	defer active.close()
	if !active.isBusy() || active.pending["7"] == nil {
		t.Fatal("adoption exposed a process without registering its running turn")
	}
	events, history := active.subscribe()
	defer active.unsubscribe(events)
	found := false
	for _, event := range history {
		if event.Type == "permission" && event.RequestID == "12" {
			found = true
		}
	}
	if !found {
		t.Fatal("the pending permission question disappeared across restart")
	}
	go active.readLoop(slog.New(slog.NewTextHandler(io.Discard, nil)))
	answer := make(chan acpEnvelope, 1)
	go func() {
		scanner := bufio.NewScanner(process.inputReader)
		if scanner.Scan() {
			var envelope acpEnvelope
			_ = json.Unmarshal(scanner.Bytes(), &envelope)
			answer <- envelope
		}
	}()
	if err := active.resolvePermission("12", "allow"); err != nil {
		t.Fatal(err)
	}
	select {
	case envelope := <-answer:
		if string(envelope.ID) != "12" || envelope.Error != nil {
			t.Fatalf("permission response = %+v", envelope)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("permission response did not reach the surviving agent")
	}
	kept, ok := active.agentRecord()
	if !ok || len(kept.PendingPermissions) != 0 {
		t.Fatalf("answered question remains persisted: %+v", kept)
	}
	process.send(map[string]any{"jsonrpc": "2.0", "id": 7, "result": map[string]string{"stopReason": "end_turn"}})
	waitUntil(t, "buffered reply settles adopted turn", func() bool { return !active.isBusy() })
}

func TestDeletingJobRequiresItsOfflineCapsuleToActuallyStop(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &awayRunnerEngine{trackedTestEngine: trackedTestEngine{image: map[string][]byte{}, files: map[string]map[string][]byte{}}}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	created := restartTestJob(t, srv, st)
	composition, err := srv.useCapsule(context.Background(), domain.UseRequest{Selector: "session:" + created.Session.ID, Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	engine.away = true
	if err := srv.stopJobRuntimes(context.Background(), created.Job, "derek", true); !errors.Is(err, worker.ErrRunnerOffline) {
		t.Fatalf("delete cleanup with offline runner = %v", err)
	}
	if _, err := st.DeleteJob(created.Job.ID, "derek"); !errors.Is(err, store.ErrConflict) {
		t.Fatalf("deleted a Job whose capsule still runs: %v", err)
	}
	kept, _ := st.Composition(composition.ID)
	if kept.Runtime == nil || kept.Runtime.Status == "stopped" || !kept.Runtime.StopPending {
		t.Fatalf("lost pending stop after deletion: %+v", kept.Runtime)
	}
	engine.away = false
	srv.runnerAttached(kept.Runtime.ClientID)
	if _, err := st.DeleteJob(created.Job.ID, "derek"); err != nil {
		t.Fatalf("delete after confirmed stop: %v", err)
	}
}

func TestLateReplyDoesNotBlockAdoptedAgentReader(t *testing.T) {
	process := newScriptedACPProcess()
	active := adoptedACP("cmp_kept", domain.AgentProcess{AgentSessionID: "kept"}, process)
	defer active.close()
	// cancelPrompt can satisfy this channel while the runner's real
	// response is still in flight.
	response := make(chan acpRPCResponse, 1)
	response <- acpRPCResponse{Result: json.RawMessage(`{"stopReason":"cancelled"}`)}
	active.pending["7"] = response
	go active.readLoop(slog.New(slog.NewTextHandler(io.Discard, nil)))
	go func() {
		process.send(map[string]any{"id": 7, "result": map[string]string{"stopReason": "end_turn"}})
		process.send(map[string]any{"method": "session/update", "params": map[string]any{"sessionId": "kept", "update": map[string]string{"sessionUpdate": "agent_message_chunk"}}})
	}()
	waitUntil(t, "reader continues after a late reply", func() bool {
		active.mu.Lock()
		defer active.mu.Unlock()
		return active.received > 0
	})
}
