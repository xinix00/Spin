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

// A message written while the agent is working is the whole point of steering.
// ACP runs one prompt turn per session, so the message has to wait for the
// running turn and then start the next one by itself.
func TestSteeringQueuesAMessageWrittenDuringATurnAndSendsItNext(t *testing.T) {
	process := newScriptedACPProcess()
	_, cancel := context.WithCancel(context.Background())
	active := &activeACP{
		sessionID: "spin-session", agentSessionID: "agent-session", protocolVersion: 1,
		process: process, cancel: cancel, done: make(chan struct{}), pending: map[string]chan acpRPCResponse{},
		permissions: map[string]bool{}, subscribers: map[chan acpBrowserEvent]struct{}{}, history: []acpBrowserEvent{},
	}
	defer active.close()
	go active.readLoop(slog.New(slog.NewTextHandler(io.Discard, nil)))

	type turn struct {
		text string
		id   json.RawMessage
	}
	turns := make(chan turn, 4)
	go func() {
		scanner := bufio.NewScanner(process.inputReader)
		for scanner.Scan() {
			var request acpEnvelope
			if json.Unmarshal(scanner.Bytes(), &request) != nil || request.Method != "session/prompt" {
				continue
			}
			var params struct {
				Prompt []map[string]any `json:"prompt"`
			}
			_ = json.Unmarshal(request.Params, &params)
			text, _ := params.Prompt[0]["text"].(string)
			turns <- turn{text: text, id: append(json.RawMessage(nil), request.ID...)}
		}
	}()
	answer := func(running turn) {
		process.send(map[string]any{"jsonrpc": "2.0", "id": running.id, "result": map[string]string{"stopReason": "end_turn"}})
	}
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
				if event.Type == "error" {
					t.Fatalf("ACP error while waiting for %q: %s", kind, event.Error)
				}
			case <-time.After(3 * time.Second):
				t.Fatalf("timed out waiting for a %q event", kind)
			}
		}
	}
	nextTurn := func() turn {
		t.Helper()
		select {
		case running := <-turns:
			return running
		case <-time.After(3 * time.Second):
			t.Fatal("the agent never received a prompt")
			return turn{}
		}
	}

	if err := active.startPrompt("bouw de switch"); err != nil {
		t.Fatal(err)
	}
	if event := await("user"); event.Text != "bouw de switch" {
		t.Fatalf("first user event = %+v", event)
	}
	first := nextTurn()
	if first.text != "bouw de switch" {
		t.Fatalf("first prompt = %q", first.text)
	}

	// The agent is still working. Steering must be accepted, not refused.
	if err := active.startPrompt("doe het in dark mode"); err != nil {
		t.Fatalf("steering was refused: %v", err)
	}
	if event := await("queued"); event.Text != "doe het in dark mode" || event.Queued != 1 {
		t.Fatalf("queued event = %+v", event)
	}
	if depth := active.queuedCount(); depth != 1 {
		t.Fatalf("queue depth = %d", depth)
	}
	select {
	case running := <-turns:
		t.Fatalf("queued message started a second concurrent turn: %q", running.text)
	case <-time.After(150 * time.Millisecond):
	}

	answer(first)
	if event := await("turn_end"); event.Queued != 1 {
		t.Fatalf("turn_end did not report the waiting message: %+v", event)
	}
	if event := await("user"); event.Text != "doe het in dark mode" {
		t.Fatalf("queued message was not sent next: %+v", event)
	}
	second := nextTurn()
	if second.text != "doe het in dark mode" {
		t.Fatalf("second prompt = %q", second.text)
	}
	if !active.isBusy() {
		t.Fatal("session reported idle while running the queued turn")
	}

	answer(second)
	if event := await("turn_end"); event.Queued != 0 {
		t.Fatalf("final turn_end = %+v", event)
	}
	if active.isBusy() || active.queuedCount() != 0 {
		t.Fatalf("session stayed busy=%t queued=%d after draining", active.isBusy(), active.queuedCount())
	}
}
