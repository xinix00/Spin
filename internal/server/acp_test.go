package server

import (
	"bufio"
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
	"github.com/gorilla/websocket"
)

type scriptedACPProcess struct {
	inputReader  *io.PipeReader
	inputWriter  *io.PipeWriter
	outputReader *io.PipeReader
	outputWriter *io.PipeWriter
	done         chan struct{}
	closeOnce    sync.Once
}

func newScriptedACPProcess() *scriptedACPProcess {
	inputReader, inputWriter := io.Pipe()
	outputReader, outputWriter := io.Pipe()
	return &scriptedACPProcess{inputReader: inputReader, inputWriter: inputWriter, outputReader: outputReader, outputWriter: outputWriter, done: make(chan struct{})}
}

func (p *scriptedACPProcess) Read(buffer []byte) (int, error) {
	return p.outputReader.Read(buffer)
}

func (p *scriptedACPProcess) Write(buffer []byte) (int, error) {
	return p.inputWriter.Write(buffer)
}

func (p *scriptedACPProcess) Close() error {
	p.closeOnce.Do(func() {
		_ = p.inputWriter.Close()
		_ = p.inputReader.Close()
		_ = p.outputWriter.Close()
		_ = p.outputReader.Close()
		close(p.done)
	})
	return nil
}

func (p *scriptedACPProcess) Wait() (capsule.Execution, error) {
	<-p.done
	return capsule.Execution{}, nil
}

func (p *scriptedACPProcess) send(value any) {
	encoded, _ := json.Marshal(value)
	_, _ = p.outputWriter.Write(append(encoded, '\n'))
}

func TestActiveACPStreamsPromptUpdates(t *testing.T) {
	process := newScriptedACPProcess()
	_, cancel := context.WithCancel(context.Background())
	active := &activeACP{
		sessionID: "spin-session", agentSessionID: "agent-session", protocolVersion: 1,
		process: process, cancel: cancel, done: make(chan struct{}), pending: map[string]chan acpRPCResponse{},
		permissions: map[string]bool{}, subscribers: map[chan acpBrowserEvent]struct{}{}, history: []acpBrowserEvent{},
	}
	defer active.close()
	go active.readLoop(slog.New(slog.NewTextHandler(io.Discard, nil)))
	prompts := make(chan []map[string]any, 1)
	go func() {
		scanner := bufio.NewScanner(process.inputReader)
		for scanner.Scan() {
			var request acpEnvelope
			_ = json.Unmarshal(scanner.Bytes(), &request)
			if request.Method != "session/prompt" {
				continue
			}
			var params struct {
				Prompt []map[string]any `json:"prompt"`
			}
			_ = json.Unmarshal(request.Params, &params)
			prompts <- params.Prompt
			process.send(map[string]any{
				"jsonrpc": "2.0", "method": "session/update",
				"params": map[string]any{"sessionId": "agent-session", "update": map[string]any{"sessionUpdate": "agent_message_chunk", "content": map[string]string{"type": "text", "text": "hello"}}},
			})
			process.send(map[string]any{"jsonrpc": "2.0", "id": json.RawMessage(request.ID), "result": map[string]string{"stopReason": "end_turn"}})
		}
	}()

	events, _ := active.subscribe()
	defer active.unsubscribe(events)
	if err := active.startPromptWithAttachments("hi", []acpPromptAttachment{{ID: "att_image", Block: map[string]any{"type": "image", "mimeType": "image/png", "data": "aGVsbG8="}}}); err != nil {
		t.Fatal(err)
	}
	timeout := time.After(2 * time.Second)
	var sawUser, sawUpdate bool
	for {
		select {
		case event := <-events:
			switch event.Type {
			case "user":
				sawUser = event.Text == "hi"
			case "update":
				sawUpdate = strings.Contains(string(event.Update), "hello")
			case "turn_end":
				if !sawUser || !sawUpdate || event.StopReason != "end_turn" {
					t.Fatalf("events: user=%t update=%t end=%+v", sawUser, sawUpdate, event)
				}
				prompt := <-prompts
				if len(prompt) != 2 || prompt[0]["type"] != "text" || prompt[1]["type"] != "image" {
					t.Fatalf("ACP prompt blocks = %#v", prompt)
				}
				return
			}
		case <-timeout:
			t.Fatal("timed out waiting for ACP turn")
		}
	}
}

func TestACPMCPServerHandoff(t *testing.T) {
	servers := []domain.MCPServer{
		{Name: "local", Transport: domain.MCPTransportStdio, Command: "/usr/bin/local-mcp", Args: []string{"--stdio"}, Env: []domain.MCPSecret{{Name: "TOKEN", Value: "secret"}}},
		{Name: "remote", Transport: domain.MCPTransportHTTP, URL: "https://mcp.example.test", Headers: []domain.MCPSecret{{Name: "Authorization", Value: "Bearer secret"}}},
	}
	if _, err := acpMCPServers(servers, false); err == nil {
		t.Fatal("HTTP MCP should require the advertised capability")
	}
	encoded, err := acpMCPServers(servers, true)
	if err != nil {
		t.Fatal(err)
	}
	if len(encoded) != 2 || encoded[0]["command"] != "/usr/bin/local-mcp" || encoded[1]["type"] != "http" {
		t.Fatalf("handoff = %+v", encoded)
	}
}

func TestACPNewSessionMakesCapsuleHomeWritable(t *testing.T) {
	servers := []map[string]any{{"name": "workflow", "command": "spin-mcp"}}
	params := acpNewSessionParams(servers)
	if params["cwd"] != "/workspace" {
		t.Fatalf("cwd = %#v", params["cwd"])
	}
	directories, ok := params["additionalDirectories"].([]string)
	if !ok || len(directories) != 1 || directories[0] != "/root" {
		t.Fatalf("additionalDirectories = %#v", params["additionalDirectories"])
	}
	if handedOff, ok := params["mcpServers"].([]map[string]any); !ok || len(handedOff) != 1 || handedOff[0]["name"] != "workflow" {
		t.Fatalf("mcpServers = %#v", params["mcpServers"])
	}
}

func TestACPWebSocketCarriesACompleteTurn(t *testing.T) {
	process := newScriptedACPProcess()
	_, cancel := context.WithCancel(context.Background())
	active := &activeACP{
		sessionID: "ses_web", operator: "derek", agentSessionID: "agent-web", agentName: "Test Agent", protocolVersion: 1,
		process: process, cancel: cancel, done: make(chan struct{}), pending: map[string]chan acpRPCResponse{},
		permissions: map[string]bool{}, subscribers: map[chan acpBrowserEvent]struct{}{}, history: []acpBrowserEvent{},
	}
	defer active.close()
	go active.readLoop(slog.New(slog.NewTextHandler(io.Discard, nil)))
	go func() {
		scanner := bufio.NewScanner(process.inputReader)
		for scanner.Scan() {
			var request acpEnvelope
			_ = json.Unmarshal(scanner.Bytes(), &request)
			if request.Method == "session/prompt" {
				process.send(map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{"sessionId": "agent-web", "update": map[string]any{"sessionUpdate": "agent_message_chunk", "content": map[string]string{"type": "text", "text": "over websocket"}}}})
				process.send(map[string]any{"jsonrpc": "2.0", "id": json.RawMessage(request.ID), "result": map[string]string{"stopReason": "end_turn"}})
			}
		}
	}()
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), capsule.Journal{}, ServerOptions{DisableAuthentication: true})
	srv.acpSessions[active.sessionID] = active
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Skipf("local sockets unavailable: %v", err)
	}
	httpServer := httptest.NewUnstartedServer(srv.Handler())
	httpServer.Listener = listener
	httpServer.Start()
	defer httpServer.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(httpServer.URL, "http")+"/api/sessions/ses_web/acp?operator=derek", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	var event acpBrowserEvent
	if err := connection.ReadJSON(&event); err != nil || event.Type != "ready" || event.AgentSessionID != "agent-web" {
		t.Fatalf("ready = %+v; error = %v", event, err)
	}
	if err := connection.WriteJSON(acpBrowserMessage{Type: "prompt", Text: "hello"}); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(2 * time.Second)
	_ = connection.SetReadDeadline(deadline)
	var sawUser, sawUpdate bool
	for {
		if err := connection.ReadJSON(&event); err != nil {
			t.Fatal(err)
		}
		switch event.Type {
		case "user":
			sawUser = event.Text == "hello"
		case "update":
			sawUpdate = strings.Contains(string(event.Update), "over websocket")
		case "turn_end":
			if !sawUser || !sawUpdate || event.StopReason != "end_turn" {
				t.Fatalf("websocket events: user=%t update=%t end=%+v", sawUser, sawUpdate, event)
			}
			return
		}
	}
}
