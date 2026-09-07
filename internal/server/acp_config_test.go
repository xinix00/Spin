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
	active.settings = acpSettingsOf(codexConfigOptions(t), nil, nil)
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

// codexConfigOptions is what codex-acp reports at session/new.
func codexConfigOptions(t *testing.T) []acpConfigOption {
	t.Helper()
	var options []acpConfigOption
	if err := json.Unmarshal([]byte(`[
		{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"agent","options":[{"value":"read-only","name":"Read only"},{"value":"agent","name":"Agent"},{"value":"agent-full-access","name":"Full access"}]},
		{"id":"model","name":"Model","category":"model","type":"select","currentValue":"gpt-5.3-codex","options":[{"value":"gpt-5.3-codex","name":"GPT-5.3 Codex","description":"Default"},{"value":"gpt-5.1-codex-mini","name":"Mini"}]},
		{"id":"reasoning_effort","name":"Reasoning effort","category":"thought_level","type":"select","currentValue":"medium","options":[{"value":"low","name":"Low"},{"value":"high","name":"High"}]}
	]`), &options); err != nil {
		t.Fatal(err)
	}
	return options
}

func sessionModes(current string, ids ...string) *acpSessionModes {
	available := make([]map[string]string, 0, len(ids))
	for _, id := range ids {
		available = append(available, map[string]string{"id": id, "name": id})
	}
	encoded, _ := json.Marshal(map[string]any{"currentModeId": current, "availableModes": available})
	var modes acpSessionModes
	_ = json.Unmarshal(encoded, &modes)
	return &modes
}

// The four agents report their settings three ways; all fold into the same
// layer options and the same full-access decision without code per agent.
func TestAgentSettingsNormalizeAcrossAgents(t *testing.T) {
	// Codex: config options with categories.
	codex := (&activeACP{agentName: "Codex", settings: acpSettingsOf(codexConfigOptions(t), nil, nil)}).agentOptions()
	if codex.AgentName != "Codex" || len(codex.Models) != 2 || codex.Models[1].Value != "gpt-5.1-codex-mini" || len(codex.ReasoningEfforts) != 2 || codex.ReasoningEfforts[0].Name != "Low" || len(codex.Modes) != 3 {
		t.Fatalf("codex options = %+v", codex)
	}
	if mode, ok := (&activeACP{settings: acpSettingsOf(codexConfigOptions(t), nil, nil)}).setting(acpCategoryMode); !ok || mode.Method != "session/set_config_option" || mode.Current != "agent" {
		t.Fatalf("codex mode setting = %+v", mode)
	} else if full, wanted := fullAccessMode(mode); full != "agent-full-access" || !wanted {
		t.Fatalf("codex full access = %q %v", full, wanted)
	}
	// Claude Code: modes and models as session state, spec-shaped models.
	var claudeModels acpSessionModels
	_ = json.Unmarshal([]byte(`{"currentModelId":"claude-sonnet-5","availableModels":[{"modelId":"claude-sonnet-5","name":"Sonnet 5"},{"modelId":"claude-opus-5","name":"Opus 5","description":"Most capable"}]}`), &claudeModels)
	claudeSettings := acpSettingsOf(nil, sessionModes("default", "default", "acceptEdits", "bypassPermissions", "plan"), &claudeModels)
	claude := (&activeACP{agentName: "Claude Code", settings: claudeSettings}).agentOptions()
	if len(claude.Models) != 2 || claude.Models[1].Value != "claude-opus-5" || claude.Models[1].Description != "Most capable" || len(claude.Modes) != 4 || len(claude.ReasoningEfforts) != 0 {
		t.Fatalf("claude options = %+v", claude)
	}
	if mode, _ := (&activeACP{settings: claudeSettings}).setting(acpCategoryMode); mode.Method != "session/set_mode" {
		t.Fatalf("claude mode setting = %+v", mode)
	} else if full, wanted := fullAccessMode(mode); full != "bypassPermissions" || !wanted {
		t.Fatalf("claude full access = %q %v", full, wanted)
	}
	// Gemini CLI: modes and models as session state, models as value/title.
	var geminiModels acpSessionModels
	_ = json.Unmarshal([]byte(`{"currentModelId":"auto","availableModels":[{"value":"auto","title":"Auto","description":"Let Gemini CLI decide"},{"value":"gemini-2.5-pro","title":"Gemini 2.5 Pro"}]}`), &geminiModels)
	geminiSettings := acpSettingsOf(nil, sessionModes("default", "default", "autoEdit", "yolo"), &geminiModels)
	gemini := (&activeACP{agentName: "Gemini", settings: geminiSettings}).agentOptions()
	if len(gemini.Models) != 2 || gemini.Models[1].Value != "gemini-2.5-pro" || gemini.Models[1].Name != "Gemini 2.5 Pro" || len(gemini.Modes) != 3 {
		t.Fatalf("gemini options = %+v", gemini)
	}
	if mode, _ := (&activeACP{settings: geminiSettings}).setting(acpCategoryMode); mode.Current != "default" {
		t.Fatalf("gemini mode setting = %+v", mode)
	} else if full, wanted := fullAccessMode(mode); full != "yolo" || !wanted {
		t.Fatalf("gemini full access = %q %v", full, wanted)
	}
	// claude-agent-acp (the current Claude adapter): config options with
	// categories, effort levels, and the full-access mode marked in _meta.
	var claudeAgent []acpConfigOption
	_ = json.Unmarshal([]byte(`[{"id":"mode","category":"mode","currentValue":"default","options":[{"value":"default","name":"Manual","_meta":{"kind":"standard"}},{"value":"auto","name":"Auto","_meta":{"kind":"auto_review"}},{"value":"bypassPermissions","name":"Bypass permissions","_meta":{"kind":"full_access"}}]},{"id":"model","category":"model","currentValue":"claude-fable-5-1[1m]","options":[{"value":"default","name":"Default (recommended)"},{"value":"claude-fable-5-1[1m]","name":"Fable"},{"value":"sonnet","name":"Sonnet"}]},{"id":"effort","category":"thought_level","currentValue":"high","options":[{"value":"default"},{"value":"low"},{"value":"medium"},{"value":"high"},{"value":"xhigh"},{"value":"max"}]}]`), &claudeAgent)
	claudeAgentSettings := acpSettingsOf(claudeAgent, sessionModes("default", "default", "auto", "bypassPermissions"), nil)
	if folded := (&activeACP{settings: claudeAgentSettings}).agentOptions(); len(folded.Models) != 3 || folded.Models[1].Name != "Fable" || len(folded.ReasoningEfforts) != 6 || folded.ReasoningEfforts[4].Value != "xhigh" || len(folded.Modes) != 3 {
		t.Fatalf("claude-agent-acp options = %+v", folded)
	}
	if mode, _ := (&activeACP{settings: claudeAgentSettings}).setting(acpCategoryMode); mode.Method != "session/set_config_option" || mode.FullAccess != "bypassPermissions" {
		t.Fatalf("claude-agent-acp mode = %+v", mode)
	} else if full, wanted := fullAccessMode(mode); full != "bypassPermissions" || !wanted {
		t.Fatalf("claude-agent-acp full access = %q %v", full, wanted)
	}
	if effort, _ := (&activeACP{settings: claudeAgentSettings}).setting(acpCategoryThoughtLevel); effort.ID != "effort" || effort.Current != "high" {
		t.Fatalf("claude-agent-acp effort = %+v", effort)
	}
	// OpenCode: config options for model and mode, no full-access mode.
	var opencode []acpConfigOption
	_ = json.Unmarshal([]byte(`[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"opencode/big-pickle","options":[{"value":"opencode/big-pickle","name":"OpenCode Zen/Big Pickle"},{"value":"minimax/MiniMax-M3","name":"MiniMax-M3"}]},{"id":"mode","name":"Session Mode","category":"mode","type":"select","currentValue":"build","options":[{"value":"build","name":"build"},{"value":"plan","name":"plan"}]}]`), &opencode)
	opencodeSettings := acpSettingsOf(opencode, nil, nil)
	if folded := (&activeACP{settings: opencodeSettings}).agentOptions(); len(folded.Models) != 2 || len(folded.Modes) != 2 || len(folded.ReasoningEfforts) != 0 {
		t.Fatalf("opencode options = %+v", folded)
	}
	if mode, _ := (&activeACP{settings: opencodeSettings}).setting(acpCategoryMode); mode.Current != "build" {
		t.Fatalf("opencode mode setting = %+v", mode)
	} else if _, wanted := fullAccessMode(mode); wanted {
		t.Fatal("opencode was switched to a full-access mode it does not have")
	}
	// A newer codex-acp reports models state next to its config options;
	// the config option wins, because its set_model wants "model[effort]".
	var codexModels acpSessionModels
	_ = json.Unmarshal([]byte(`{"currentModelId":"gpt-5.3-codex[medium]","availableModels":[{"modelId":"gpt-5.3-codex[medium]","name":"GPT-5.3 Codex"}]}`), &codexModels)
	both := acpSettingsOf(codexConfigOptions(t), sessionModes("agent-full-access", "agent", "agent-full-access"), &codexModels)
	if model, _ := (&activeACP{settings: both}).setting(acpCategoryModel); model.Method != "session/set_config_option" || model.ID != "model" || len(model.Values) != 2 {
		t.Fatalf("model with both shapes = %+v", model)
	}
	if mode, _ := (&activeACP{settings: both}).setting(acpCategoryMode); mode.Method != "session/set_config_option" {
		t.Fatalf("mode with both shapes = %+v", mode)
	}
	// An already full-access session is not switched.
	if mode, _ := (&activeACP{settings: acpSettingsOf(nil, sessionModes("agent-full-access", "agent", "agent-full-access"), nil)}).setting(acpCategoryMode); mode.Method != "session/set_mode" {
		t.Fatalf("mode from state = %+v", mode)
	} else if _, wanted := fullAccessMode(mode); wanted {
		t.Fatal("switching was requested although the mode is already full access")
	}
	// A config option without a category is placed by its id.
	var bare []acpConfigOption
	_ = json.Unmarshal([]byte(`[{"id":"thinking_level","options":[{"value":"low"}]},{"id":"llm","options":[{"value":"x"}]}]`), &bare)
	if settings := acpSettingsOf(bare, nil, nil); settings[0].Category != acpCategoryThoughtLevel || settings[1].Category != "llm" {
		t.Fatalf("categories from ids = %+v", settings)
	}
}

// Claude Code reports its models as session state and switches them with
// session/set_model; it offers no reasoning effort, so none is sent.
func TestApplyConfigUsesSetModelForSessionModels(t *testing.T) {
	var models acpSessionModels
	if err := json.Unmarshal([]byte(`{"currentModelId":"claude-sonnet-5","availableModels":[{"modelId":"claude-sonnet-5","name":"Sonnet 5"},{"modelId":"claude-opus-5","name":"Opus 5","description":"Most capable"}]}`), &models); err != nil {
		t.Fatal(err)
	}
	process := newScriptedACPProcess()
	active := newTestActiveACP(process)
	active.settings = acpSettingsOf(nil, nil, &models)
	defer active.close()
	requests := acpRequests(t, process)
	done := make(chan error, 1)
	go func() { done <- active.applyConfig("claude-opus-5", "high") }()
	set := nextACPRequest(t, requests, "session/set_model")
	var params struct {
		SessionID string `json:"sessionId"`
		ModelID   string `json:"modelId"`
	}
	_ = json.Unmarshal(set.Params, &params)
	if params.SessionID != "agent-session" || params.ModelID != "claude-opus-5" {
		t.Fatalf("set_model params = %s", set.Params)
	}
	process.send(map[string]any{"jsonrpc": "2.0", "id": set.ID, "result": map[string]any{}})
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
		t.Fatalf("a reasoning effort the agent never offered was sent: %s", extra.Params)
	case <-time.After(100 * time.Millisecond):
	}
}

func TestACPErrorCarriesAgentData(t *testing.T) {
	plain := (&acpRPCError{Code: -32603, Message: "Internal error"}).asError().Error()
	withText := (&acpRPCError{Code: -32603, Message: "Internal error", Data: json.RawMessage(`"model gpt-6-astra is not available"`)}).asError().Error()
	withObject := (&acpRPCError{Code: -32603, Message: "Internal error", Data: json.RawMessage(`{"details":"Query closed"}`)}).asError().Error()
	if plain != "Internal error (-32603)" || withText != "Internal error (-32603): model gpt-6-astra is not available" || withObject != `Internal error (-32603): {"details":"Query closed"}` {
		t.Fatalf("errors = %q / %q / %q", plain, withText, withObject)
	}
}
