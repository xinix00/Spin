package server

import (
	"bufio"
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"testing"
	"time"
)

// acpRequests reads every request the fake agent receives, by method.
func acpRequests(t *testing.T, process *scriptedACPProcess) chan acpEnvelope {
	t.Helper()
	requests := make(chan acpEnvelope, 16)
	go func() {
		scanner := bufio.NewScanner(process.inputReader)
		for scanner.Scan() {
			var request acpEnvelope
			if json.Unmarshal(scanner.Bytes(), &request) == nil && request.Method != "" {
				request.ID = append(json.RawMessage(nil), request.ID...)
				requests <- request
			}
		}
	}()
	return requests
}

func nextACPRequest(t *testing.T, requests chan acpEnvelope, method string) acpEnvelope {
	t.Helper()
	for {
		select {
		case request := <-requests:
			if request.Method == method {
				return request
			}
		case <-time.After(3 * time.Second):
			t.Fatalf("the agent never received %s", method)
		}
	}
}

func newTestActiveACP(process *scriptedACPProcess) *activeACP {
	_, cancel := context.WithCancel(context.Background())
	active := &activeACP{
		sessionID: "spin-session", agentSessionID: "agent-session", protocolVersion: 1,
		process: process, cancel: cancel, done: make(chan struct{}), pending: map[string]chan acpRPCResponse{},
		permissions: map[string]bool{}, subscribers: map[chan acpBrowserEvent]struct{}{}, history: []acpBrowserEvent{},
	}
	go active.readLoop(slog.New(slog.NewTextHandler(io.Discard, nil)))
	return active
}

// An agent that offers steering gets a message written during a turn injected
// into that turn, the way codex-acp's _session/steering works; nothing waits.
func TestSteeringInjectsIntoTheRunningTurnWhenTheAgentOffersIt(t *testing.T) {
	process := newScriptedACPProcess()
	active := newTestActiveACP(process)
	defer active.close()
	active.steering = true
	requests := acpRequests(t, process)
	events, _ := active.subscribe()
	defer active.unsubscribe(events)
	await := func(kind string) acpBrowserEvent {
		t.Helper()
		for {
			select {
			case event := <-events:
				if event.Type == kind {
					return event
				}
			case <-time.After(3 * time.Second):
				t.Fatalf("timed out waiting for a %q event", kind)
			}
		}
	}

	if err := active.startPrompt("bouw de switch"); err != nil {
		t.Fatal(err)
	}
	first := nextACPRequest(t, requests, "session/prompt")
	await("user")
	if err := active.startPrompt("doe het in dark mode"); err != nil {
		t.Fatal(err)
	}
	await("user")
	steer := nextACPRequest(t, requests, "_session/steering")
	var params struct {
		SessionID string           `json:"sessionId"`
		Prompt    []map[string]any `json:"prompt"`
	}
	_ = json.Unmarshal(steer.Params, &params)
	if params.SessionID != "agent-session" || len(params.Prompt) != 1 || params.Prompt[0]["text"] != "doe het in dark mode" {
		t.Fatalf("steering params = %s", steer.Params)
	}
	process.send(map[string]any{"jsonrpc": "2.0", "id": steer.ID, "result": map[string]string{"outcome": "injected"}})
	await("steered")
	if depth := active.queuedCount(); depth != 0 {
		t.Fatalf("injected message was also queued: depth %d", depth)
	}
	process.send(map[string]any{"jsonrpc": "2.0", "id": first.ID, "result": map[string]string{"stopReason": "end_turn"}})
	if event := await("turn_end"); event.Queued != 0 {
		t.Fatalf("turn_end after steering = %+v", event)
	}
}

// A phase's model and reasoning effort reach the agent as session config
// options before its first prompt; an empty value is not sent.
func TestApplyConfigSetsModelAndReasoningEffort(t *testing.T) {
	process := newScriptedACPProcess()
	active := newTestActiveACP(process)
	defer active.close()
	requests := acpRequests(t, process)
	done := make(chan error, 1)
	go func() { done <- active.applyConfig("gpt-5.3-codex", "") }()
	set := nextACPRequest(t, requests, "session/set_config_option")
	var params struct {
		SessionID string `json:"sessionId"`
		ConfigID  string `json:"configId"`
		Value     string `json:"value"`
	}
	_ = json.Unmarshal(set.Params, &params)
	if params.SessionID != "agent-session" || params.ConfigID != "model" || params.Value != "gpt-5.3-codex" {
		t.Fatalf("set_config_option params = %s", set.Params)
	}
	process.send(map[string]any{"jsonrpc": "2.0", "id": set.ID, "result": map[string]any{"configOptions": []any{}}})
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("applyConfig did not return")
	}
	select {
	case extra := <-requests:
		t.Fatalf("an empty reasoning effort was sent: %s", extra.Params)
	case <-time.After(100 * time.Millisecond):
	}
}

// What session/new offered folds into the layer's stored options.
func TestAgentOptionsFoldSessionConfigOptions(t *testing.T) {
	var options []acpConfigOption
	if err := json.Unmarshal([]byte(`[
		{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"agent","options":[{"value":"read-only","name":"Read only"},{"value":"agent","name":"Agent"}]},
		{"id":"model","name":"Model","category":"model","type":"select","currentValue":"gpt-5.3-codex","options":[{"value":"gpt-5.3-codex","name":"GPT-5.3 Codex","description":"Default"},{"value":"gpt-5.1-codex-mini","name":"Mini"}]},
		{"id":"reasoning_effort","name":"Reasoning effort","category":"thought_level","type":"select","currentValue":"medium","options":[{"value":"low","name":"Low"},{"value":"high","name":"High"}]}
	]`), &options); err != nil {
		t.Fatal(err)
	}
	active := &activeACP{agentName: "Codex", configOptions: options}
	folded := active.agentOptions()
	if folded.AgentName != "Codex" || len(folded.Models) != 2 || folded.Models[1].Value != "gpt-5.1-codex-mini" || len(folded.ReasoningEfforts) != 2 || folded.ReasoningEfforts[0].Name != "Low" || len(folded.Modes) != 2 {
		t.Fatalf("folded options = %+v", folded)
	}
}
