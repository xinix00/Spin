package server

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/gorilla/websocket"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

type acpProbeRequest struct {
	Operator string `json:"operator"`
}

type acpProbeResponse struct {
	CompositionID string            `json:"composition_id"`
	Enablement    domain.Enablement `json:"enablement"`
	Handshake     json.RawMessage   `json:"handshake"`
}

type acpRPCError struct {
	Code    int             `json:"code"`
	Message string          `json:"message"`
	Data    json.RawMessage `json:"data,omitempty"`
}

type acpEnvelope struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Method  string          `json:"method,omitempty"`
	Params  json.RawMessage `json:"params,omitempty"`
	Result  json.RawMessage `json:"result,omitempty"`
	Error   *acpRPCError    `json:"error,omitempty"`
}

type acpBrowserMessage struct {
	Type      string `json:"type"`
	Text      string `json:"text,omitempty"`
	RequestID string `json:"request_id,omitempty"`
	OptionID  string `json:"option_id,omitempty"`
}

type acpBrowserEvent struct {
	Type           string          `json:"type"`
	At             time.Time       `json:"at,omitempty"`
	Text           string          `json:"text,omitempty"`
	Update         json.RawMessage `json:"update,omitempty"`
	Params         json.RawMessage `json:"params,omitempty"`
	RequestID      string          `json:"request_id,omitempty"`
	AgentSessionID string          `json:"agent_session_id,omitempty"`
	AgentName      string          `json:"agent_name,omitempty"`
	StopReason     string          `json:"stop_reason,omitempty"`
	Queued         int             `json:"queued,omitempty"`
	Busy           bool            `json:"busy,omitempty"`
	Fatal          bool            `json:"fatal,omitempty"`
	Error          string          `json:"error,omitempty"`
}

type acpRPCResponse struct {
	Result json.RawMessage
	Error  *acpRPCError
}

// acpPromptCapabilities are negotiated with the concrete ACP agent. Text and
// resource links are baseline ACP; richer blocks may only be sent when the
// agent explicitly opts in.
type acpPromptCapabilities struct {
	Image           bool `json:"image"`
	Audio           bool `json:"audio"`
	EmbeddedContext bool `json:"embeddedContext"`
}

type acpPromptAttachment struct {
	ID    string
	Block map[string]any
}

type activeACP struct {
	sessionID       string
	compositionID   string
	operator        string
	agentSessionID  string
	agentName       string
	protocolVersion int
	promptCaps      acpPromptCapabilities
	process         capsule.EnabledProcess
	cancel          context.CancelFunc
	done            chan struct{}
	doneOnce        sync.Once
	writeMu         sync.Mutex
	mu              sync.Mutex
	nextID          int64
	pending         map[string]chan acpRPCResponse
	permissions     map[string]bool
	subscribers     map[chan acpBrowserEvent]struct{}
	history         []acpBrowserEvent
	sentAttachments map[string]bool
	busy            bool
	queued          []queuedPrompt
	failure         error
	onIdle          func() // runs when a turn ends with nothing queued behind it
	// steering is set when the agent accepts _session/steering: a message
	// written during a turn is injected into that turn instead of queued.
	steering bool
	// configOptions is what the agent offered at session/new: models,
	// reasoning efforts, modes.
	configOptions []acpConfigOption
	// modes is the session mode state of session/new, for agents that
	// report modes there instead of as a config option (Claude Code).
	modes *acpSessionModes
	// models is the session model state of session/new, for agents that
	// report models there and switch them with session/set_model (Claude
	// Code) instead of a "model" config option.
	models *acpSessionModels
	// primed is set once this agent session has read the phase's full
	// prompt. An agent session is not durable (a deploy or runner restart
	// makes a new one), and a resumed one must not act on a bare answer or
	// chat line without the instructions and rules the phase was given.
	primed bool
}

func (a *activeACP) isPrimed() bool {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.primed
}

func (a *activeACP) markPrimed() {
	a.mu.Lock()
	a.primed = true
	a.mu.Unlock()
}

// acpConfigOption is one session config option as an ACP agent reports it.
type acpConfigOption struct {
	ID           string          `json:"id"`
	Name         string          `json:"name"`
	Category     string          `json:"category"`
	Type         string          `json:"type"`
	CurrentValue json.RawMessage `json:"currentValue"`
	Options      []struct {
		Value       string `json:"value"`
		Name        string `json:"name"`
		Description string `json:"description"`
	} `json:"options"`
}

// queuedPrompt is a message the operator wrote while the agent was still
// working. ACP runs one prompt turn per session, so steering is queueing: the
// message becomes the next turn instead of interrupting the running one.
type queuedPrompt struct {
	text        string
	attachments []acpPromptAttachment
}

// maxQueuedPrompts bounds how far ahead an operator can steer. Beyond this the
// queue stops being steering and starts being a script written blind.
const maxQueuedPrompts = 16

// acpKeepaliveInterval stays well under the 100 s after which Cloudflare drops
// an idle WebSocket.
const acpKeepaliveInterval = 25 * time.Second

func (s *Server) probeACPHandler(w http.ResponseWriter, r *http.Request) {
	var req acpProbeRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	result, err := s.probeACP(r.Context(), r.PathValue("compositionID"), req.Operator)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, result)
}

func (s *Server) sessionACP(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	connection, err := terminalUpgrader.Upgrade(w, r, nil)
	if err != nil {
		s.logger.Debug("upgrade ACP chat", "error", err)
		return
	}
	defer connection.Close()
	connection.SetReadLimit(1 << 20)
	active, err := s.getOrStartACP(r.PathValue("sessionID"), operator)
	if err != nil {
		_ = connection.WriteJSON(acpBrowserEvent{Type: "error", Error: err.Error(), Fatal: true})
		return
	}

	events, history := active.subscribe()
	defer active.unsubscribe(events)
	agentSessionID, agentName, busy := active.info()
	ready := acpBrowserEvent{Type: "ready", AgentSessionID: agentSessionID, AgentName: agentName, Busy: busy, Queued: active.queuedCount()}
	if err := connection.WriteJSON(ready); err != nil {
		return
	}
	for _, event := range history {
		if err := connection.WriteJSON(event); err != nil {
			return
		}
	}

	clientMessages := make(chan acpBrowserMessage)
	clientErrors := make(chan error, 1)
	go func() {
		defer close(clientMessages)
		for {
			var message acpBrowserMessage
			if err := connection.ReadJSON(&message); err != nil {
				clientErrors <- err
				return
			}
			clientMessages <- message
		}
	}()

	// A turn can be silent for minutes while the agent thinks or runs a tool.
	// Intermediaries close a WebSocket that carries nothing for that long, and
	// every reconnect replays the conversation. Pings keep the socket busy.
	keepalive := time.NewTicker(acpKeepaliveInterval)
	defer keepalive.Stop()
	for {
		select {
		case <-keepalive.C:
			if connection.WriteControl(websocket.PingMessage, nil, time.Now().Add(10*time.Second)) != nil {
				return
			}
		case event, ok := <-events:
			if !ok || connection.WriteJSON(event) != nil {
				return
			}
		case message, ok := <-clientMessages:
			if !ok {
				return
			}
			switch message.Type {
			case "prompt":
				if _, err := s.store.ResumeWorkflowPhaseForChat(active.sessionID, operator); err != nil {
					_ = connection.WriteJSON(acpBrowserEvent{Type: "error", Error: err.Error()})
					continue
				}
				if err := s.startACPPrompt(active, message.Text); err != nil {
					_ = connection.WriteJSON(acpBrowserEvent{Type: "error", Error: err.Error()})
				}
			case "cancel":
				if err := active.cancelPrompt(); err != nil {
					_ = connection.WriteJSON(acpBrowserEvent{Type: "error", Error: err.Error()})
				}
			case "permission":
				if err := active.resolvePermission(message.RequestID, message.OptionID); err != nil {
					_ = connection.WriteJSON(acpBrowserEvent{Type: "error", Error: err.Error()})
				}
			}
		case <-clientErrors:
			return
		case <-active.done:
			if failure := active.err(); failure != nil {
				_ = connection.WriteJSON(acpBrowserEvent{Type: "error", Error: failure.Error(), Fatal: true})
			}
			return
		}
	}
}

func (s *Server) sessionChanges(w http.ResponseWriter, r *http.Request) {
	changes, err := s.inspectSessionChanges(r.Context(), r.PathValue("sessionID"), s.requestOperator(r, r.URL.Query().Get("operator")))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, changes)
}

func (s *Server) jobChanges(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	changes, err := s.inspectJobChanges(r.Context(), r.PathValue("jobID"), operator, strings.TrimSpace(r.URL.Query().Get("session_id")))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, changes)
}

func (s *Server) inspectSessionChanges(ctx context.Context, sessionID, operator string) (capsule.WorkspaceChanges, error) {
	session, composition, err := s.sessionComposition(sessionID, operator)
	if err != nil {
		return capsule.WorkspaceChanges{}, err
	}
	if session.PreparedCompositionID == "" || composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return capsule.WorkspaceChanges{}, fmt.Errorf("session has no running workspace: %w", store.ErrConflict)
	}
	inspector, ok := s.engine.(capsule.WorkspaceInspector)
	if !ok {
		return capsule.WorkspaceChanges{}, fmt.Errorf("capsule engine %s cannot inspect workspaces: %w", s.engine.Info().Driver, store.ErrConflict)
	}
	inspectContext, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	return inspector.InspectWorkspace(inspectContext, *composition.Runtime)
}

func (s *Server) inspectJobChanges(ctx context.Context, jobID, _ string, sessionID string) (capsule.WorkspaceChanges, error) {
	job, compositions, err := s.store.JobWorkspaceHistory(jobID)
	if err != nil {
		return capsule.WorkspaceChanges{}, err
	}
	snapshot := s.store.Snapshot()
	phaseName := ""
	if sessionID != "" {
		sessionIndex := -1
		for index := range snapshot.Sessions {
			if snapshot.Sessions[index].ID == sessionID && snapshot.Sessions[index].JobID == job.ID {
				sessionIndex = index
				break
			}
		}
		if sessionIndex < 0 {
			return capsule.WorkspaceChanges{}, store.ErrNotFound
		}
		for _, run := range snapshot.PhaseRuns {
			if run.SessionID == sessionID && run.JobID == job.ID {
				phaseName = run.PhaseName
				break
			}
		}
	}
	var composition *domain.Composition
	for index := len(compositions) - 1; index >= 0; index-- {
		candidate := &compositions[index]
		if candidate.Runtime != nil && candidate.Runtime.Status != "stopped" && candidate.Runtime.ContainerID != "" && candidate.Git != nil {
			composition = candidate
			break
		}
	}
	if composition == nil {
		for index := len(compositions) - 1; index >= 0; index-- {
			candidate := &compositions[index]
			if candidate.Runtime != nil && candidate.Runtime.Status == "stopped" && candidate.Git != nil {
				composition = candidate
				break
			}
		}
	}
	if composition == nil {
		return capsule.WorkspaceChanges{}, fmt.Errorf("Job has no materialized workspace history for comparison: %w", store.ErrConflict)
	}
	inspector, ok := s.engine.(capsule.WorkspaceRangeInspector)
	if !ok {
		return capsule.WorkspaceChanges{}, fmt.Errorf("capsule engine %s cannot compare Job workspaces: %w", s.engine.Info().Driver, store.ErrConflict)
	}
	authentication := &capsule.GitAuthentication{}
	account, authenticated, accountErr := s.gitAccountForWorkspace(ctx, composition.Git, composition.Operator)
	if accountErr != nil {
		return capsule.WorkspaceChanges{}, accountErr
	}
	if authenticated {
		username := account.Login
		if account.Provider == "gitlab" {
			username = "oauth2"
		}
		authentication.Username = username
		authentication.Password = account.AccessToken
	}
	runtime := composition.Runtime
	if runtime.Status == "stopped" {
		comparisonContext, cancel := context.WithTimeout(ctx, 45*time.Second)
		defer cancel()
		var restored domain.CapsuleRuntime
		if authenticated {
			materializer, supported := s.engine.(capsule.SecretMaterializer)
			if !supported {
				return capsule.WorkspaceChanges{}, fmt.Errorf("capsule engine cannot restore a private Job workspace: %w", store.ErrConflict)
			}
			restored, err = materializer.MaterializeWithGitAuthentication(comparisonContext, *composition, snapshot.Artifacts, authentication)
		} else {
			restored, err = s.engine.Materialize(comparisonContext, *composition, snapshot.Artifacts)
		}
		if err != nil {
			return capsule.WorkspaceChanges{}, fmt.Errorf("restore Job workspace for comparison: %w", err)
		}
		runtime = &restored
		defer func() {
			cleanupContext, cleanupCancel := context.WithTimeout(context.Background(), 15*time.Second)
			defer cleanupCancel()
			if stopErr := s.engine.Stop(cleanupContext, restored); stopErr != nil {
				s.logger.Warn("stop transient Job comparison", "job", job.ID, "error", stopErr)
			}
		}()
	}
	comparison := capsule.WorkspaceComparison{
		BaseRef: job.BaseRef, HeadRef: job.Branch, Authentication: authentication,
	}
	if sessionID != "" {
		comparison.CommitMessageMatch = "Spin-Session: " + sessionID
	}
	inspectContext, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	changes, err := inspector.InspectWorkspaceRange(inspectContext, *runtime, comparison)
	if err != nil {
		return capsule.WorkspaceChanges{}, err
	}
	if sessionID == "" {
		changes.Branch = job.Branch + " ← " + job.BaseRef
	} else if phaseName != "" {
		changes.Branch = phaseName + " · " + sessionID
	} else {
		changes.Branch = sessionID
	}
	return changes, nil
}

func (s *Server) getOrStartACP(sessionID, operator string) (*activeACP, error) {
	s.acpMu.Lock()
	defer s.acpMu.Unlock()
	if active := s.acpSessions[sessionID]; active != nil {
		select {
		case <-active.done:
			delete(s.acpSessions, sessionID)
		default:
			if active.operator != normalizeOperator(operator) {
				return nil, store.ErrConflict
			}
			return active, nil
		}
	}
	session, composition, err := s.sessionComposition(sessionID, operator)
	if err != nil {
		return nil, err
	}
	if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return nil, fmt.Errorf("session composition is not running: %w", store.ErrConflict)
	}
	mcpServers, err := s.store.MCPServersForOperator(operator, session.MCPServerIDs)
	if err != nil {
		return nil, err
	}
	if session.PhaseRunID != "" {
		workflowServer, workflowErr := s.workflowMCPServer(session.ID)
		if workflowErr != nil {
			return nil, fmt.Errorf("prepare Spin workflow tools: %w", workflowErr)
		}
		mcpServers = append(mcpServers, workflowServer)
	}
	active, err := s.openACP(composition, operator, mcpServers)
	if err != nil {
		return nil, err
	}
	active.mu.Lock()
	active.sessionID = session.ID
	active.onIdle = func() {
		if _, err := s.store.SettleWorkflowChatTurn(session.ID); err != nil {
			s.logger.Warn("settle workflow phase after ACP turn", "session", session.ID, "error", err)
		}
	}
	active.mu.Unlock()
	s.rememberAgentOptions(composition, active)
	s.acpSessions[sessionID] = active
	return active, nil
}

// openACP starts the agent in a running composition's capsule and takes it
// through initialize and session/new. What the agent offers (steering, config
// options) is kept on the result; the caller binds it to a Session or closes
// it again.
func (s *Server) openACP(composition domain.Composition, operator string, mcpServers []domain.MCPServer) (*activeACP, error) {
	enabled, err := acpEnablement(composition)
	if err != nil {
		return nil, err
	}
	streamer, ok := s.engine.(capsule.EnabledEngine)
	if !ok {
		return nil, fmt.Errorf("capsule engine %s cannot stream enabled entrypoints: %w", s.engine.Info().Driver, store.ErrConflict)
	}
	ctx, cancel := context.WithCancel(context.Background())
	process, err := streamer.StartEnabled(ctx, *composition.Runtime, enabled)
	if err != nil {
		cancel()
		return nil, fmt.Errorf("start ACP entrypoint: %w", err)
	}
	active := &activeACP{
		compositionID: composition.ID, operator: normalizeOperator(operator),
		protocolVersion: enabled.ProtocolVersion, process: process, cancel: cancel, done: make(chan struct{}),
		pending: map[string]chan acpRPCResponse{}, permissions: map[string]bool{}, sentAttachments: map[string]bool{},
		subscribers: map[chan acpBrowserEvent]struct{}{}, history: []acpBrowserEvent{},
	}
	if active.protocolVersion == 0 {
		active.protocolVersion = 1
	}
	go active.readLoop(s.logger)
	initializeContext, initializeCancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer initializeCancel()
	initialize, err := active.request(initializeContext, "initialize", map[string]any{
		"protocolVersion": active.protocolVersion,
		"clientCapabilities": map[string]any{
			"fs":       map[string]bool{"readTextFile": false, "writeTextFile": false},
			"terminal": false,
			"auth":     map[string]bool{"terminal": false},
		},
		"clientInfo": map[string]string{"name": "easyacp", "title": "EasyACP Capsule Client", "version": "0.2.0"},
	})
	if err != nil {
		active.close()
		return nil, fmt.Errorf("ACP initialize: %w", err)
	}
	var capabilities struct {
		ProtocolVersion   int `json:"protocolVersion"`
		AgentCapabilities struct {
			PromptCapabilities acpPromptCapabilities `json:"promptCapabilities"`
			MCPCapabilities    struct {
				HTTP bool `json:"http"`
			} `json:"mcpCapabilities"`
		} `json:"agentCapabilities"`
		AgentInfo struct {
			Name  string `json:"name"`
			Title string `json:"title"`
		} `json:"agentInfo"`
		Meta struct {
			Steering struct {
				Supported bool `json:"supported"`
			} `json:"steering"`
		} `json:"_meta"`
	}
	if err := json.Unmarshal(initialize, &capabilities); err != nil || capabilities.ProtocolVersion != active.protocolVersion {
		active.close()
		if err != nil {
			return nil, fmt.Errorf("decode ACP initialize: %w", err)
		}
		return nil, fmt.Errorf("ACP protocol negotiation failed: requested %d, got %d", active.protocolVersion, capabilities.ProtocolVersion)
	}
	active.mu.Lock()
	active.promptCaps = capabilities.AgentCapabilities.PromptCapabilities
	active.steering = capabilities.Meta.Steering.Supported
	active.agentName = capabilities.AgentInfo.Title
	if active.agentName == "" {
		active.agentName = capabilities.AgentInfo.Name
	}
	active.mu.Unlock()
	servers, err := acpMCPServers(mcpServers, capabilities.AgentCapabilities.MCPCapabilities.HTTP)
	if err != nil {
		active.close()
		return nil, err
	}
	newSessionContext, newSessionCancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer newSessionCancel()
	created, err := active.request(newSessionContext, "session/new", acpNewSessionParams(servers))
	if err != nil {
		active.close()
		return nil, fmt.Errorf("ACP session/new: %w", err)
	}
	var newSession struct {
		SessionID     string            `json:"sessionId"`
		ConfigOptions []acpConfigOption `json:"configOptions"`
		Modes         *acpSessionModes  `json:"modes"`
		Models        *acpSessionModels `json:"models"`
	}
	if err := json.Unmarshal(created, &newSession); err != nil || strings.TrimSpace(newSession.SessionID) == "" {
		active.close()
		if err != nil {
			return nil, fmt.Errorf("decode ACP session/new: %w", err)
		}
		return nil, errors.New("ACP session/new returned no sessionId")
	}
	active.mu.Lock()
	active.agentSessionID = newSession.SessionID
	active.configOptions = newSession.ConfigOptions
	active.modes = newSession.Modes
	active.models = newSession.Models
	active.mu.Unlock()
	// The capsule is the sandbox: a throwaway container without the host,
	// without Git credentials, and with everything the agent does discarded
	// unless ACCEPT folds it into a commit. A second sandbox inside it
	// (codex-acp starts in "agent", workspace-write without network) only
	// takes away network, /tmp and parallel builds, so the session runs in
	// the agent's full-access mode when it offers one.
	settings := s.agentSettingsFor(composition)
	modeID, ok := fullAccessMode(newSession.Modes, newSession.ConfigOptions)
	if settings.Mode != "" {
		modeID, ok = settings.Mode, settings.Mode != currentMode(newSession.Modes, newSession.ConfigOptions)
	}
	if ok {
		modeContext, modeCancel := context.WithTimeout(context.Background(), 15*time.Second)
		_, err := active.request(modeContext, "session/set_mode", map[string]any{"sessionId": newSession.SessionID, "modeId": modeID})
		modeCancel()
		if err != nil {
			s.logger.Warn("set ACP session mode", "mode", modeID, "error", err)
		}
	}
	// The layer's chosen model and reasoning effort are the defaults for
	// every Session on it; a Template phase may override them afterwards.
	if err := active.applyConfig(settings.Model, settings.ReasoningEffort); err != nil {
		active.close()
		return nil, fmt.Errorf("apply the layer's agent settings: %w", err)
	}
	return active, nil
}

// agentSettingsFor is what the layer that ENABLES acp in a composition chose
// for its agent; nothing when it chose nothing.
func (s *Server) agentSettingsFor(composition domain.Composition) domain.AgentSettings {
	artifactID := s.acpArtifactID(composition)
	if artifactID == "" {
		return domain.AgentSettings{}
	}
	artifact, err := s.store.Artifact(artifactID)
	if err != nil || artifact.AgentSettings == nil {
		return domain.AgentSettings{}
	}
	return *artifact.AgentSettings
}

// currentMode is the mode an agent reports a new session in.
func currentMode(modes *acpSessionModes, options []acpConfigOption) string {
	if modes != nil && modes.CurrentModeID != "" {
		return modes.CurrentModeID
	}
	for _, option := range options {
		if option.ID == "mode" {
			var value string
			_ = json.Unmarshal(option.CurrentValue, &value)
			return value
		}
	}
	return ""
}

// setEnablementCommandHandler sets the entrypoint of a capability a layer
// ENABLES, for a recording that was made without --command.
func (s *Server) setEnablementCommandHandler(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Command string `json:"command"`
	}
	if !decodeJSON(w, r, &req) {
		return
	}
	updated, err := s.store.SetArtifactEnablementCommand(r.PathValue("artifactID"), r.PathValue("name"), req.Command)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, updated)
}

// setAgentSettingsHandler stores the chosen mode, model and reasoning effort
// on the layer that carries the agent, whichever layer of its closure the
// request names.
func (s *Server) setAgentSettingsHandler(w http.ResponseWriter, r *http.Request) {
	var settings domain.AgentSettings
	if !decodeJSON(w, r, &settings) {
		return
	}
	agentLayer, ok := s.store.EnablingLayer(r.PathValue("artifactID"), "acp")
	if !ok {
		writeError(w, fmt.Errorf("layer has no ACP agent in its closure: %w", store.ErrConflict))
		return
	}
	updated, err := s.store.SetArtifactAgentSettings(agentLayer.ID, settings)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, updated)
}

// acpSessionModes is the mode state an agent reports at session/new.
type acpSessionModes struct {
	CurrentModeID  string `json:"currentModeId"`
	AvailableModes []struct {
		ID          string `json:"id"`
		Name        string `json:"name"`
		Description string `json:"description"`
	} `json:"availableModes"`
}

// acpSessionModels is the session model state some agents report at
// session/new instead of a "model" config option.
type acpSessionModels struct {
	CurrentModelID  string `json:"currentModelId"`
	AvailableModels []struct {
		ID          string `json:"modelId"`
		Name        string `json:"name"`
		Description string `json:"description"`
	} `json:"availableModels"`
}

// fullAccessMode finds the agent's full-access mode, from the session mode
// state or the "mode" config option, and reports whether switching to it is
// needed.
func fullAccessMode(modes *acpSessionModes, options []acpConfigOption) (string, bool) {
	current, available := "", []string{}
	if modes != nil {
		current = modes.CurrentModeID
		for _, mode := range modes.AvailableModes {
			available = append(available, mode.ID)
		}
	}
	for _, option := range options {
		if option.ID != "mode" {
			continue
		}
		var value string
		if json.Unmarshal(option.CurrentValue, &value) == nil && current == "" {
			current = value
		}
		for _, candidate := range option.Options {
			available = append(available, candidate.Value)
		}
	}
	// Agents name it differently: codex-acp "agent-full-access", the Claude
	// Code adapter "bypassPermissions".
	for _, id := range available {
		lower := strings.ToLower(id)
		if strings.Contains(lower, "full-access") || strings.Contains(lower, "full_access") || strings.Contains(lower, "bypass") {
			return id, id != current
		}
	}
	return "", false
}

// agentOptions folds the agent's config options into what a layer keeps.
func (a *activeACP) agentOptions() domain.AgentOptions {
	a.mu.Lock()
	defer a.mu.Unlock()
	options := domain.AgentOptions{AgentName: a.agentName, FetchedAt: time.Now().UTC()}
	for _, option := range a.configOptions {
		values := make([]domain.AgentOption, 0, len(option.Options))
		for _, value := range option.Options {
			values = append(values, domain.AgentOption{Value: value.Value, Name: value.Name, Description: value.Description})
		}
		switch option.ID {
		case "model":
			options.Models = values
		case "reasoning_effort":
			options.ReasoningEfforts = values
		case "mode":
			options.Modes = values
		}
	}
	if len(options.Modes) == 0 && a.modes != nil {
		for _, mode := range a.modes.AvailableModes {
			options.Modes = append(options.Modes, domain.AgentOption{Value: mode.ID, Name: mode.Name, Description: mode.Description})
		}
	}
	if len(options.Models) == 0 && a.models != nil {
		for _, model := range a.models.AvailableModels {
			options.Models = append(options.Models, domain.AgentOption{Value: model.ID, Name: model.Name, Description: model.Description})
		}
	}
	return options
}

// hasConfigOption reports whether the agent offered a config option with id.
func (a *activeACP) hasConfigOption(id string) bool {
	for _, option := range a.configOptions {
		if option.ID == id {
			return true
		}
	}
	return false
}

// applyConfig sets the phase's model and reasoning effort on the agent's
// session. Empty values keep the agent's own default. The model goes the
// way the agent offered it: a "model" config option, or session/set_model
// for an agent that reported its models as session state. A reasoning
// effort is only sent when the agent offered one, or nothing at all is
// known about what it offers.
func (a *activeACP) applyConfig(model, reasoningEffort string) error {
	a.mu.Lock()
	sessionID, models, advertised := a.agentSessionID, a.models, len(a.configOptions) > 0 || a.models != nil
	a.mu.Unlock()
	set := func(method string, params map[string]any, what, value string) error {
		params["sessionId"] = sessionID
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		_, err := a.request(ctx, method, params)
		cancel()
		if err != nil {
			return fmt.Errorf("agent refused %s %q: %w", what, value, err)
		}
		return nil
	}
	if model = strings.TrimSpace(model); model != "" {
		if !a.hasConfigOption("model") && models != nil {
			if err := set("session/set_model", map[string]any{"modelId": model}, "model", model); err != nil {
				return err
			}
		} else if err := set("session/set_config_option", map[string]any{"configId": "model", "value": model}, "model", model); err != nil {
			return err
		}
	}
	if reasoningEffort = strings.TrimSpace(reasoningEffort); reasoningEffort != "" && (a.hasConfigOption("reasoning_effort") || !advertised) {
		return set("session/set_config_option", map[string]any{"configId": "reasoning_effort", "value": reasoningEffort}, "reasoning_effort", reasoningEffort)
	}
	return nil
}

// acpArtifactID finds the layer in a composition that ENABLES acp.
func (s *Server) acpArtifactID(composition domain.Composition) string {
	for _, artifactID := range composition.SlotBindings {
		artifact, err := s.store.Artifact(artifactID)
		if err != nil {
			continue
		}
		for _, enabled := range artifact.Enables {
			if enabled.Name == "acp" {
				return artifact.ID
			}
		}
	}
	return ""
}

// rememberAgentOptions keeps what an agent just offered on its layer, so the
// Template editor can pick from it without starting the agent again.
func (s *Server) rememberAgentOptions(composition domain.Composition, active *activeACP) {
	options := active.agentOptions()
	if len(options.Models) == 0 && len(options.ReasoningEfforts) == 0 && len(options.Modes) == 0 {
		return
	}
	if artifactID := s.acpArtifactID(composition); artifactID != "" {
		if _, err := s.store.SetArtifactAgentOptions(artifactID, options); err != nil {
			s.logger.Warn("remember agent options", "artifact", artifactID, "error", err)
		}
	}
}

// fetchAgentOptions starts the layer's agent once, in a throwaway capsule,
// to learn which models and reasoning efforts it offers, and keeps that on
// the layer.
func (s *Server) fetchAgentOptions(ctx context.Context, artifactID, operator string) (domain.AgentOptions, error) {
	artifact, err := s.store.Artifact(artifactID)
	if err != nil {
		return domain.AgentOptions{}, err
	}
	agentLayer, ok := s.store.EnablingLayer(artifact.ID, "acp")
	if !ok {
		return domain.AgentOptions{}, fmt.Errorf("layer %s:%s has no ACP agent in its closure: %w", artifact.Kind, artifact.Name, store.ErrConflict)
	}
	// Probe the agent the way the operator runs it: under their credential
	// layer when they have one, so the agent is logged in and answers.
	entry, err := s.store.IdentityLayerFor(artifact.ID, operator)
	if err != nil {
		return domain.AgentOptions{}, err
	}
	composition, err := s.useCapsule(ctx, domain.UseRequest{Selector: string(entry.Kind) + ":" + entry.Name, Profile: entry.Profile, Operator: operator})
	if err != nil {
		return domain.AgentOptions{}, err
	}
	defer func() {
		stopContext, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()
		if _, err := s.stopCapsule(stopContext, composition.ID, operator); err != nil {
			s.logger.Warn("stop capsule after fetching agent options", "composition", composition.ID, "error", err)
		}
	}()
	active, err := s.openACP(composition, operator, nil)
	if err != nil {
		return domain.AgentOptions{}, err
	}
	options := active.agentOptions()
	active.close()
	// The options belong to the layer that carries the agent, whichever
	// layer the probe was started from.
	if _, err := s.store.SetArtifactAgentOptions(agentLayer.ID, options); err != nil {
		return domain.AgentOptions{}, err
	}
	return options, nil
}

// fetchAgentOptionsHandler answers at once and fetches in the background; the
// layer's agent_options show the result (or the failure) on the next state.
func (s *Server) fetchAgentOptionsHandler(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	artifactID := r.PathValue("artifactID")
	if _, err := s.store.Artifact(artifactID); err != nil {
		writeError(w, err)
		return
	}
	target := artifactID
	if agentLayer, ok := s.store.EnablingLayer(artifactID, "acp"); ok {
		target = agentLayer.ID
	}
	// The card shows "fetching" from the state until the result lands; a
	// browser refresh in between must not reset it.
	_ = s.store.MarkArtifactAgentOptionsFetching(target, true)
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
		defer cancel()
		if _, err := s.fetchAgentOptions(ctx, artifactID, operator); err != nil {
			s.logger.Warn("fetch agent options", "artifact", artifactID, "error", err)
			_, _ = s.store.SetArtifactAgentOptions(target, domain.AgentOptions{Error: err.Error(), FetchedAt: time.Now().UTC()})
		}
	}()
	writeJSON(w, http.StatusAccepted, map[string]string{"status": "fetching", "artifact_id": artifactID})
}

// acpNewSessionParams gives the agent two distinct writable areas inside its
// materialized capsule: /workspace for the repository and /root for ordinary
// user-level tool state. The latter is the container user's HOME, not the host
// root directory. ACP agents that implement additionalDirectories (including
// codex-acp) add it to their workspace sandbox without weakening the sandbox
// outside this already isolated Session container.
func acpNewSessionParams(servers []map[string]any) map[string]any {
	return map[string]any{
		"cwd":                   "/workspace",
		"additionalDirectories": []string{"/root"},
		"mcpServers":            servers,
	}
}

func (s *Server) sessionComposition(sessionID, operator string) (domain.Session, domain.Composition, error) {
	operator = normalizeOperator(operator)
	snapshot := s.store.Snapshot()
	var session domain.Session
	for _, candidate := range snapshot.Sessions {
		if candidate.ID == sessionID {
			session = candidate
			break
		}
	}
	if session.ID == "" {
		return domain.Session{}, domain.Composition{}, store.ErrNotFound
	}
	if normalizeOperator(session.Operator) != operator {
		return domain.Session{}, domain.Composition{}, store.ErrConflict
	}
	for _, composition := range snapshot.Compositions {
		if composition.ID == session.PreparedCompositionID {
			if normalizeOperator(composition.Operator) != operator {
				return domain.Session{}, domain.Composition{}, store.ErrConflict
			}
			return session, composition, nil
		}
	}
	return domain.Session{}, domain.Composition{}, fmt.Errorf("session has no prepared composition: %w", store.ErrConflict)
}

func acpEnablement(composition domain.Composition) (domain.Enablement, error) {
	for _, enabled := range composition.Enabled {
		if enabled.Name == "acp" {
			return enabled, nil
		}
	}
	return domain.Enablement{}, fmt.Errorf("composition does not ENABLE acp: %w", store.ErrConflict)
}

func acpMCPServers(servers []domain.MCPServer, supportsHTTP bool) ([]map[string]any, error) {
	out := make([]map[string]any, 0, len(servers))
	for _, server := range servers {
		switch server.Transport {
		case domain.MCPTransportStdio:
			out = append(out, map[string]any{"name": server.Name, "command": server.Command, "args": server.Args, "env": server.Env})
		case domain.MCPTransportHTTP:
			if !supportsHTTP {
				return nil, fmt.Errorf("ACP agent does not advertise HTTP MCP support required by %s: %w", server.Name, store.ErrConflict)
			}
			out = append(out, map[string]any{"type": "http", "name": server.Name, "url": server.URL, "headers": server.Headers})
		default:
			return nil, fmt.Errorf("unsupported MCP transport %q: %w", server.Transport, store.ErrConflict)
		}
	}
	return out, nil
}

func (a *activeACP) readLoop(logger *slog.Logger) {
	scanner := bufio.NewScanner(a.process)
	scanner.Buffer(make([]byte, 32<<10), 16<<20)
	for scanner.Scan() {
		line := append([]byte{}, scanner.Bytes()...)
		var envelope acpEnvelope
		if err := json.Unmarshal(line, &envelope); err != nil {
			a.fail(fmt.Errorf("ACP wrote non-JSON stdout: %q", truncate(string(line), 240)))
			return
		}
		if envelope.Method != "" {
			a.receiveMethod(envelope)
			continue
		}
		key := string(envelope.ID)
		a.mu.Lock()
		response := a.pending[key]
		a.mu.Unlock()
		if response != nil {
			response <- acpRPCResponse{Result: envelope.Result, Error: envelope.Error}
		}
	}
	if err := scanner.Err(); err != nil {
		a.fail(fmt.Errorf("read ACP stream: %w", err))
		return
	}
	execution, waitErr := a.process.Wait()
	if waitErr != nil {
		a.fail(waitErr)
		return
	}
	logger.Debug("ACP process ended", "session", a.sessionID, "exit", execution.ExitCode)
	a.fail(fmt.Errorf("ACP process exited %d: %s", execution.ExitCode, strings.TrimSpace(execution.Output)))
}

func (a *activeACP) receiveMethod(envelope acpEnvelope) {
	switch envelope.Method {
	case "session/update":
		var params struct {
			SessionID string          `json:"sessionId"`
			Update    json.RawMessage `json:"update"`
		}
		a.mu.Lock()
		agentSessionID := a.agentSessionID
		a.mu.Unlock()
		if json.Unmarshal(envelope.Params, &params) == nil && (agentSessionID == "" || params.SessionID == agentSessionID) {
			a.broadcast(acpBrowserEvent{Type: "update", Update: params.Update}, true)
		}
	case "session/request_permission":
		if len(envelope.ID) == 0 {
			return
		}
		key := string(envelope.ID)
		a.mu.Lock()
		a.permissions[key] = true
		a.mu.Unlock()
		a.broadcast(acpBrowserEvent{Type: "permission", RequestID: key, Params: envelope.Params}, true)
	default:
		if len(envelope.ID) != 0 {
			_ = a.write(acpEnvelope{JSONRPC: "2.0", ID: envelope.ID, Error: &acpRPCError{Code: -32601, Message: "client method not supported"}})
		}
	}
}

func (a *activeACP) request(ctx context.Context, method string, params any) (json.RawMessage, error) {
	a.mu.Lock()
	a.nextID++
	id := a.nextID
	key := fmt.Sprintf("%d", id)
	response := make(chan acpRPCResponse, 1)
	a.pending[key] = response
	a.mu.Unlock()
	defer func() {
		a.mu.Lock()
		delete(a.pending, key)
		a.mu.Unlock()
	}()
	encodedParams, err := json.Marshal(params)
	if err != nil {
		return nil, err
	}
	if err := a.write(acpEnvelope{JSONRPC: "2.0", ID: json.RawMessage(key), Method: method, Params: encodedParams}); err != nil {
		return nil, err
	}
	select {
	case received := <-response:
		if received.Error != nil {
			return nil, fmt.Errorf("%s (%d)", received.Error.Message, received.Error.Code)
		}
		return received.Result, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-a.done:
		return nil, a.err()
	}
}

func (a *activeACP) notify(method string, params any) error {
	encodedParams, err := json.Marshal(params)
	if err != nil {
		return err
	}
	return a.write(acpEnvelope{JSONRPC: "2.0", Method: method, Params: encodedParams})
}

func (a *activeACP) write(envelope acpEnvelope) error {
	encoded, err := json.Marshal(envelope)
	if err != nil {
		return err
	}
	a.writeMu.Lock()
	defer a.writeMu.Unlock()
	_, err = a.process.Write(append(encoded, '\n'))
	return err
}

func (a *activeACP) startPrompt(text string) error {
	return a.startPromptWithAttachments(text, nil)
}

func (a *activeACP) startPromptWithAttachments(text string, attachments []acpPromptAttachment) error {
	text = strings.TrimSpace(text)
	if text == "" {
		return errors.New("prompt is empty")
	}
	a.mu.Lock()
	if a.busy && a.steering {
		a.mu.Unlock()
		go a.steer(queuedPrompt{text: text, attachments: attachments})
		return nil
	}
	if a.busy {
		if len(a.queued) >= maxQueuedPrompts {
			a.mu.Unlock()
			return errors.New("too many messages are already waiting for the running turn")
		}
		a.queued = append(a.queued, queuedPrompt{text: text, attachments: attachments})
		depth := len(a.queued)
		a.mu.Unlock()
		a.broadcast(acpBrowserEvent{Type: "queued", Text: text, Queued: depth}, true)
		return nil
	}
	a.busy = true
	a.mu.Unlock()
	a.runPrompts(queuedPrompt{text: text, attachments: attachments})
	return nil
}

// steer hands a message to the running turn through the agent's steering
// extension (_session/steering, as codex-acp offers it). The agent reads it
// mid-turn; when the turn had just ended the agent starts a new one from it.
// An agent that refuses gets the message queued the ordinary way.
func (a *activeACP) steer(message queuedPrompt) {
	prompt, newAttachmentIDs := a.buildPrompt(message)
	a.broadcast(acpBrowserEvent{Type: "user", Text: message.text}, true)
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	result, err := a.request(ctx, "_session/steering", map[string]any{"sessionId": a.agentSessionID, "prompt": prompt})
	var outcome struct {
		Outcome string `json:"outcome"`
	}
	if err == nil {
		_ = json.Unmarshal(result, &outcome)
	}
	switch {
	case err == nil && outcome.Outcome == "injected":
		a.broadcast(acpBrowserEvent{Type: "steered", Text: "Ingestuurd in de lopende beurt"}, true)
	case err == nil && outcome.Outcome == "startedNewTurn":
		a.broadcast(acpBrowserEvent{Type: "steered", Text: "De beurt was net klaar; de agent is er een nieuwe mee begonnen"}, true)
	default:
		reason := outcome.Outcome
		if err != nil {
			reason = err.Error()
		}
		a.mu.Lock()
		for _, attachmentID := range newAttachmentIDs {
			delete(a.sentAttachments, attachmentID)
		}
		a.queued = append(a.queued, message)
		depth := len(a.queued)
		a.mu.Unlock()
		a.broadcast(acpBrowserEvent{Type: "queued", Text: fmt.Sprintf("Insturen lukte niet (%s); het bericht wacht op de volgende beurt", reason), Queued: depth}, true)
	}
}

// runPrompts sends one turn and then takes whatever the operator queued while
// it ran, until the queue is empty. Busy stays set across that hand-over, so a
// steered conversation never briefly reads as idle between two of its own
// turns. A failed turn drops the queue: an agent that just returned an error is
// not in a state to receive what was meant to follow.
func (a *activeACP) runPrompts(first queuedPrompt) {
	go func() {
		current, stillWaiting := first, 0
		for {
			prompt, newAttachmentIDs := a.buildPrompt(current)
			a.broadcast(acpBrowserEvent{Type: "user", Text: current.text, Queued: stillWaiting}, true)
			result, err := a.request(context.Background(), "session/prompt", map[string]any{
				"sessionId": a.agentSessionID,
				"prompt":    prompt,
			})
			if err != nil {
				a.mu.Lock()
				for _, attachmentID := range newAttachmentIDs {
					delete(a.sentAttachments, attachmentID)
				}
				a.mu.Unlock()
			}
			next, remaining, more := a.finishTurn(err != nil)
			if err != nil {
				message := "ACP prompt: " + err.Error()
				if remaining > 0 {
					message += fmt.Sprintf(" · %d wachtende bericht(en) verwijderd", remaining)
				}
				a.broadcast(acpBrowserEvent{Type: "error", Error: message}, true)
				a.settleIdle()
				return
			}
			var completed struct {
				StopReason string `json:"stopReason"`
			}
			_ = json.Unmarshal(result, &completed)
			if !more {
				// Settle before the browser hears the turn ended, so its next
				// refresh already shows the decision as pending again.
				a.settleIdle()
			}
			a.broadcast(acpBrowserEvent{Type: "turn_end", StopReason: completed.StopReason, Queued: remaining}, true)
			if !more {
				return
			}
			current, stillWaiting = next, remaining-1
		}
	}()
}

func (a *activeACP) settleIdle() {
	a.mu.Lock()
	onIdle := a.onIdle
	a.mu.Unlock()
	if onIdle != nil {
		onIdle()
	}
}

func (a *activeACP) buildPrompt(message queuedPrompt) ([]map[string]any, []string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.sentAttachments == nil {
		a.sentAttachments = map[string]bool{}
	}
	prompt := []map[string]any{{"type": "text", "text": message.text}}
	newAttachmentIDs := make([]string, 0, len(message.attachments))
	for _, attachment := range message.attachments {
		attachment.ID = strings.TrimSpace(attachment.ID)
		if attachment.ID == "" || len(attachment.Block) == 0 || a.sentAttachments[attachment.ID] {
			continue
		}
		prompt = append(prompt, attachment.Block)
		a.sentAttachments[attachment.ID] = true
		newAttachmentIDs = append(newAttachmentIDs, attachment.ID)
	}
	return prompt, newAttachmentIDs
}

// finishTurn hands back the next queued message. The count it reports is the
// queue as the turn ended, so it includes the message about to start: the
// browser reads it as "more is coming" and never shows an idle gap between two
// turns the operator queued. Busy is cleared only when nothing is waiting.
// With discard set the queue is dropped and its former size is returned.
func (a *activeACP) finishTurn(discard bool) (queuedPrompt, int, bool) {
	a.mu.Lock()
	defer a.mu.Unlock()
	waiting := len(a.queued)
	if discard {
		a.queued = nil
		a.busy = false
		return queuedPrompt{}, waiting, false
	}
	if waiting == 0 {
		a.busy = false
		return queuedPrompt{}, 0, false
	}
	next := a.queued[0]
	a.queued = a.queued[1:]
	return next, waiting, true
}

func (a *activeACP) queuedCount() int {
	a.mu.Lock()
	defer a.mu.Unlock()
	return len(a.queued)
}

func (a *activeACP) promptCapabilities() acpPromptCapabilities {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.promptCaps
}

func (a *activeACP) sentAttachmentSnapshot() map[string]bool {
	a.mu.Lock()
	defer a.mu.Unlock()
	sent := make(map[string]bool, len(a.sentAttachments))
	for attachmentID, value := range a.sentAttachments {
		sent[attachmentID] = value
	}
	return sent
}

func (a *activeACP) cancelPrompt() error {
	if !a.isBusy() {
		return errors.New("there is no running prompt")
	}
	a.mu.Lock()
	dropped := len(a.queued)
	a.queued = nil
	pending := make([]string, 0, len(a.permissions))
	for id := range a.permissions {
		pending = append(pending, id)
	}
	a.mu.Unlock()
	if dropped > 0 {
		a.broadcast(acpBrowserEvent{Type: "queued", Queued: 0, Text: fmt.Sprintf("%d wachtende bericht(en) geannuleerd", dropped)}, true)
	}
	for _, id := range pending {
		_ = a.resolvePermissionOutcome(id, map[string]string{"outcome": "cancelled"})
	}
	return a.notify("session/cancel", map[string]string{"sessionId": a.agentSessionID})
}

func (a *activeACP) resolvePermission(requestID, optionID string) error {
	requestID = strings.TrimSpace(requestID)
	optionID = strings.TrimSpace(optionID)
	if requestID == "" || optionID == "" || !json.Valid([]byte(requestID)) {
		return errors.New("permission request and option are required")
	}
	return a.resolvePermissionOutcome(requestID, map[string]string{"outcome": "selected", "optionId": optionID})
}

func (a *activeACP) resolvePermissionOutcome(requestID string, outcome map[string]string) error {
	a.mu.Lock()
	if !a.permissions[requestID] {
		a.mu.Unlock()
		return errors.New("permission request is no longer pending")
	}
	delete(a.permissions, requestID)
	a.mu.Unlock()
	result, err := json.Marshal(map[string]any{"outcome": outcome})
	if err != nil {
		return err
	}
	return a.write(acpEnvelope{JSONRPC: "2.0", ID: json.RawMessage(requestID), Result: result})
}

func (a *activeACP) subscribe() (chan acpBrowserEvent, []acpBrowserEvent) {
	a.mu.Lock()
	defer a.mu.Unlock()
	events := make(chan acpBrowserEvent, 256)
	a.subscribers[events] = struct{}{}
	return events, append([]acpBrowserEvent{}, a.history...)
}

func (a *activeACP) unsubscribe(events chan acpBrowserEvent) {
	a.mu.Lock()
	delete(a.subscribers, events)
	a.mu.Unlock()
}

func (a *activeACP) broadcast(event acpBrowserEvent, remember bool) {
	if event.At.IsZero() {
		event.At = time.Now().UTC()
	}
	a.mu.Lock()
	if remember {
		a.history = append(a.history, event)
		if len(a.history) > 4000 {
			a.history = append([]acpBrowserEvent{}, a.history[len(a.history)-4000:]...)
		}
	}
	for subscriber := range a.subscribers {
		select {
		case subscriber <- event:
		default:
		}
	}
	a.mu.Unlock()
}

func (a *activeACP) isBusy() bool {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.busy
}

func (a *activeACP) info() (string, string, bool) {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.agentSessionID, a.agentName, a.busy
}

func (a *activeACP) err() error {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.failure
}

func (a *activeACP) fail(err error) {
	a.doneOnce.Do(func() {
		a.mu.Lock()
		a.failure = err
		a.mu.Unlock()
		close(a.done)
	})
}

func (a *activeACP) close() {
	a.cancel()
	_ = a.process.Close()
	select {
	case <-a.done:
	case <-time.After(3 * time.Second):
		a.fail(errors.New("ACP process stopped"))
	}
}

func (s *Server) stopACPComposition(compositionID string) {
	s.acpMu.Lock()
	active := make([]*activeACP, 0)
	for sessionID, candidate := range s.acpSessions {
		if candidate.compositionID == compositionID {
			active = append(active, candidate)
			delete(s.acpSessions, sessionID)
		}
	}
	s.acpMu.Unlock()
	for _, candidate := range active {
		candidate.close()
	}
}

func (s *Server) probeACP(ctx context.Context, compositionID, operator string) (acpProbeResponse, error) {
	composition, err := s.store.Composition(compositionID)
	if err != nil {
		return acpProbeResponse{}, err
	}
	if composition.Operator != normalizeOperator(operator) {
		return acpProbeResponse{}, store.ErrConflict
	}
	if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return acpProbeResponse{}, fmt.Errorf("composition is not running: %w", store.ErrConflict)
	}
	enabled, err := acpEnablement(composition)
	if err != nil {
		return acpProbeResponse{}, err
	}
	prober, ok := s.engine.(capsule.EnabledProber)
	if !ok {
		return acpProbeResponse{}, fmt.Errorf("capsule engine %s cannot launch enabled entrypoints: %w", s.engine.Info().Driver, store.ErrConflict)
	}
	protocolVersion := enabled.ProtocolVersion
	if protocolVersion == 0 {
		protocolVersion = 1
	}
	request, err := json.Marshal(map[string]any{
		"jsonrpc": "2.0", "id": 0, "method": "initialize",
		"params": map[string]any{
			"protocolVersion": protocolVersion, "clientCapabilities": map[string]any{},
			"clientInfo": map[string]string{"name": "easyacp", "title": "EasyACP Capsule Client", "version": "0.2.0"},
		},
	})
	if err != nil {
		return acpProbeResponse{}, err
	}
	probeContext, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	handshake, err := prober.ProbeEnabled(probeContext, *composition.Runtime, enabled, request)
	if err != nil {
		return acpProbeResponse{}, fmt.Errorf("ACP initialize: %w", err)
	}
	var response struct {
		JSONRPC string `json:"jsonrpc"`
		Result  *struct {
			ProtocolVersion int `json:"protocolVersion"`
		} `json:"result"`
		Error *acpRPCError `json:"error"`
	}
	if err := json.Unmarshal(handshake, &response); err != nil {
		return acpProbeResponse{}, fmt.Errorf("decode ACP initialize response: %w", err)
	}
	if response.Error != nil {
		return acpProbeResponse{}, fmt.Errorf("ACP initialize error %d: %s", response.Error.Code, response.Error.Message)
	}
	if response.JSONRPC != "2.0" || response.Result == nil || response.Result.ProtocolVersion != protocolVersion {
		return acpProbeResponse{}, fmt.Errorf("ACP protocol negotiation failed: requested %d, response %s", protocolVersion, string(handshake))
	}
	return acpProbeResponse{CompositionID: composition.ID, Enablement: enabled, Handshake: handshake}, nil
}

func truncate(value string, limit int) string {
	if len(value) <= limit {
		return value
	}
	return value[:limit] + "…"
}
