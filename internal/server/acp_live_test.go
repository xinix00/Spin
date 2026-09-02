package server

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"os"
	"strings"
	"testing"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

// This opt-in test exercises a real recorded ACP agent without coupling the
// regular test suite to Docker or an external model account.
func TestACPPromptLive(t *testing.T) {
	containerID := strings.TrimSpace(os.Getenv("SPIN_ACP_CONTAINER"))
	if containerID == "" {
		t.Skip("set SPIN_ACP_CONTAINER to a running composition with ENABLED acp")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()
	engine, err := capsule.NewDocker(ctx, capsule.DockerConfig{})
	if err != nil {
		t.Fatal(err)
	}
	process, err := engine.StartEnabled(ctx, domain.CapsuleRuntime{Driver: "docker", ContainerID: containerID, Status: "ready"}, domain.Enablement{Name: "acp", Command: "codex-acp", Transport: "stdio", ProtocolVersion: 1})
	if err != nil {
		t.Fatal(err)
	}
	active := &activeACP{
		sessionID: "live", process: process, cancel: cancel, done: make(chan struct{}), protocolVersion: 1,
		pending: map[string]chan acpRPCResponse{}, permissions: map[string]bool{}, subscribers: map[chan acpBrowserEvent]struct{}{}, history: []acpBrowserEvent{},
	}
	defer active.close()
	go active.readLoop(slog.New(slog.NewTextHandler(io.Discard, nil)))
	initialized, err := active.request(ctx, "initialize", map[string]any{
		"protocolVersion": 1, "clientCapabilities": map[string]any{},
		"clientInfo": map[string]string{"name": "easyacp-live-test", "version": "0.2.0"},
	})
	if err != nil {
		t.Fatal(err)
	}
	var initializeResult struct {
		ProtocolVersion int `json:"protocolVersion"`
	}
	if err := json.Unmarshal(initialized, &initializeResult); err != nil || initializeResult.ProtocolVersion != 1 {
		t.Fatalf("initialize = %s; error = %v", initialized, err)
	}
	created, err := active.request(ctx, "session/new", acpNewSessionParams(nil))
	if err != nil {
		t.Fatal(err)
	}
	var session struct {
		SessionID string `json:"sessionId"`
	}
	if err := json.Unmarshal(created, &session); err != nil || session.SessionID == "" {
		t.Fatalf("session/new = %s; error = %v", created, err)
	}
	active.mu.Lock()
	active.agentSessionID = session.SessionID
	active.mu.Unlock()
	events, _ := active.subscribe()
	defer active.unsubscribe(events)
	if err := active.startPrompt("Reply with exactly: SPIN_ACP_OK"); err != nil {
		t.Fatal(err)
	}
	var output strings.Builder
	for {
		select {
		case event := <-events:
			switch event.Type {
			case "update":
				var update struct {
					SessionUpdate string `json:"sessionUpdate"`
					Content       struct {
						Type string `json:"type"`
						Text string `json:"text"`
					} `json:"content"`
				}
				_ = json.Unmarshal(event.Update, &update)
				if update.SessionUpdate == "agent_message_chunk" && update.Content.Type == "text" {
					output.WriteString(update.Content.Text)
				}
			case "turn_end":
				if !strings.Contains(output.String(), "SPIN_ACP_OK") {
					t.Fatalf("agent output = %q", output.String())
				}
				return
			case "error":
				t.Fatal(event.Error)
			}
		case <-ctx.Done():
			t.Fatalf("ACP prompt timeout; output = %q", output.String())
		}
	}
}
