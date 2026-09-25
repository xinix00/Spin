package worker

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"sync"
	"testing"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

// agentProcess is an agent that runs until its stdin closes.
type agentProcess struct {
	closed chan struct{}
	once   sync.Once
}

func (p *agentProcess) Read([]byte) (int, error)    { <-p.closed; return 0, io.EOF }
func (p *agentProcess) Write(b []byte) (int, error) { return len(b), nil }
func (p *agentProcess) Close() error                { p.once.Do(func() { close(p.closed) }); return nil }
func (p *agentProcess) Wait() (capsule.Execution, error) {
	<-p.closed
	return capsule.Execution{}, nil
}

type agentEngine struct {
	capsule.Journal
	mu        sync.Mutex
	processes []*agentProcess
}

func (e *agentEngine) StartEnabled(context.Context, domain.CapsuleRuntime, domain.Enablement) (capsule.EnabledProcess, error) {
	process := &agentProcess{closed: make(chan struct{})}
	e.mu.Lock()
	e.processes = append(e.processes, process)
	e.mu.Unlock()
	return process, nil
}

// A capsule runs one agent: a server that starts a new one (it could not
// take up the one an earlier server left) ends the old one, so two agents
// never share a login. Another capsule's agent is left alone.
func TestANewAgentEndsTheOneBeforeItInTheSameCapsule(t *testing.T) {
	engine := &agentEngine{}
	w := New(Config{ServerURL: "http://spin.invalid", InstanceID: "laptop-1", Engine: engine}, slog.New(slog.NewTextHandler(io.Discard, nil)))
	start := func(id, container string) {
		t.Helper()
		payload, _ := json.Marshal(enabledPayload{Runtime: domain.CapsuleRuntime{ContainerID: container}, Enablement: domain.Enablement{Name: "acp"}})
		w.dispatch(context.Background(), wireMessage{Type: messageRequest, ID: id, Method: methodStartEnabled, Payload: payload})
		deadline := time.Now().Add(5 * time.Second)
		for {
			if _, ok := w.stream(id); ok {
				return
			}
			if time.Now().After(deadline) {
				t.Fatalf("stream %s did not start", id)
			}
			time.Sleep(5 * time.Millisecond)
		}
	}
	start("str_1", "capsule-a")
	start("str_2", "capsule-b")
	start("str_3", "capsule-a")
	select {
	case <-engine.processes[0].closed:
	case <-time.After(5 * time.Second):
		t.Fatal("the older agent of the capsule still runs")
	}
	for index, process := range engine.processes[1:] {
		select {
		case <-process.closed:
			t.Fatalf("agent %d was ended", index+2)
		default:
		}
	}
}

type blockedAgentEngine struct {
	agentEngine
	entered chan string
	release chan struct{}
}

func (e *blockedAgentEngine) StartEnabled(ctx context.Context, runtime domain.CapsuleRuntime, enabled domain.Enablement) (capsule.EnabledProcess, error) {
	e.entered <- runtime.ContainerID
	if runtime.ContainerID == "capsule-a" {
		select {
		case <-e.release:
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
	return e.agentEngine.StartEnabled(ctx, runtime, enabled)
}

func TestConcurrentAgentStartsDoNotShareACapsule(t *testing.T) {
	engine := &blockedAgentEngine{entered: make(chan string, 4), release: make(chan struct{})}
	w := New(Config{Engine: engine}, slog.New(slog.NewTextHandler(io.Discard, nil)))
	start := func(ctx context.Context, id, container string) error {
		payload, _ := json.Marshal(enabledPayload{Runtime: domain.CapsuleRuntime{ContainerID: container}, Enablement: domain.Enablement{Name: "acp"}})
		_, _, err := w.invoke(ctx, wireMessage{ID: id, Method: methodStartEnabled, Payload: payload})
		return err
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	first := make(chan error, 1)
	go func() { first <- start(ctx, "str_1", "capsule-a") }()
	select {
	case <-engine.entered:
	case <-ctx.Done():
		t.Fatal("first agent did not enter StartEnabled")
	}
	// Another capsule can start while the first is still waiting.
	if err := start(ctx, "str_other", "capsule-b"); err != nil {
		t.Fatal(err)
	}
	<-engine.entered
	// A competing start waits for the entire start-and-bind operation,
	// and its deadline must not leave another live agent behind.
	waiting, stopWaiting := context.WithTimeout(ctx, 50*time.Millisecond)
	defer stopWaiting()
	if err := start(waiting, "str_cancelled", "capsule-a"); err != context.DeadlineExceeded {
		t.Fatalf("competing start = %v", err)
	}
	select {
	case id := <-engine.entered:
		t.Fatalf("competing start reached the engine for %s", id)
	default:
	}
	close(engine.release)
	if err := <-first; err != nil {
		t.Fatal(err)
	}
	if err := start(ctx, "str_replacement", "capsule-a"); err != nil {
		t.Fatal(err)
	}
	engine.mu.Lock()
	defer engine.mu.Unlock()
	if len(engine.processes) != 3 {
		t.Fatalf("started %d agents, want 3", len(engine.processes))
	}
	select {
	case <-engine.processes[1].closed:
	default:
		t.Fatal("the replacement left the first agent alive")
	}
	for _, process := range engine.processes {
		_ = process.Close()
	}
}
