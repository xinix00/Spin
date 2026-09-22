package server

import (
	"bufio"
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"strings"
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
	idle := make(chan struct{}, 4)
	active.onIdle = func() { idle <- struct{}{} }
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
		// A real agent says something before it ends its turn; a turn that
		// stays silent is treated as a failure.
		process.send(map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{
			"sessionId": "agent-session",
			"update":    map[string]any{"sessionUpdate": "agent_message_chunk", "content": map[string]any{"type": "text", "text": "ok"}},
		}})
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
	// The session is not idle between two turns the operator queued.
	select {
	case <-idle:
		t.Fatal("session settled as idle while a queued message was still waiting")
	default:
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
	select {
	case <-idle:
	case <-time.After(2 * time.Second):
		t.Fatal("session never settled as idle after the queue drained")
	}
}

// A turn that ends without the agent saying anything, and a turn the agent
// answers with an error, both report a reason: the step goes back in the
// queue with that reason instead of staying quietly "running".
func TestFailedTurnReportsItsReason(t *testing.T) {
	for _, tc := range []struct {
		name  string
		reply func(id json.RawMessage) map[string]any
		want  string
	}{
		{
			name: "silent turn",
			reply: func(id json.RawMessage) map[string]any {
				return map[string]any{"jsonrpc": "2.0", "id": id, "result": map[string]string{"stopReason": "end_turn"}}
			},
			want: "zonder iets te zeggen",
		},
		{
			name: "agent error",
			reply: func(id json.RawMessage) map[string]any {
				return map[string]any{"jsonrpc": "2.0", "id": id, "error": map[string]any{"code": -32603, "message": "Failed to authenticate: OAuth session expired"}}
			},
			want: "OAuth session expired",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			process := newScriptedACPProcess()
			_, cancel := context.WithCancel(context.Background())
			active := &activeACP{
				sessionID: "spin-session", agentSessionID: "agent-session", protocolVersion: 1,
				process: process, cancel: cancel, done: make(chan struct{}), pending: map[string]chan acpRPCResponse{},
				permissions: map[string]bool{}, subscribers: map[chan acpBrowserEvent]struct{}{}, history: []acpBrowserEvent{},
			}
			defer active.close()
			failures := make(chan string, 4)
			active.onTurnFailed = func(reason string) { failures <- reason }
			go active.readLoop(slog.New(slog.NewTextHandler(io.Discard, nil)))
			go func() {
				scanner := bufio.NewScanner(process.inputReader)
				for scanner.Scan() {
					var request struct {
						Method string          `json:"method"`
						ID     json.RawMessage `json:"id"`
					}
					if json.Unmarshal(scanner.Bytes(), &request) != nil || request.Method != "session/prompt" {
						continue
					}
					process.send(tc.reply(request.ID))
				}
			}()
			if err := active.startPrompt("ga verder"); err != nil {
				t.Fatal(err)
			}
			select {
			case reason := <-failures:
				if !strings.Contains(reason, tc.want) {
					t.Fatalf("reason = %q, want it to mention %q", reason, tc.want)
				}
			case <-time.After(3 * time.Second):
				t.Fatal("a failed turn reported nothing")
			}
		})
	}
}
