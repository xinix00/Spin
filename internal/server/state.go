package server

import (
	"easyacp/internal/domain"
	"easyacp/internal/orchestrator"
	"github.com/gorilla/websocket"
	"net/http"
	"time"
)

// stateFor is the state one browser sees: the store as visible to the
// signed-in user plus what only lives in memory (launches under way).
func (s *Server) stateFor(r *http.Request) stateResponse {
	identity, _ := identityFromRequest(r)
	snapshot := s.store.Snapshot()
	if !s.authDisabled {
		snapshot = visibleSnapshot(snapshot, identity.User.Username)
	}
	recommendations := orchestrator.Recommend(snapshot)
	if recommendations == nil {
		recommendations = []domain.Recommendation{}
	}
	return stateResponse{Snapshot: snapshot, Recommendations: recommendations, Engine: s.engine.Info(), GitOAuthProviders: s.gitOAuth.publicProviders(r), CurrentUser: publicUser(identity.User), Preparing: s.sessionPreparations(), Storage: s.storageInfo(r.Context()), Version: s.store.Version()}
}

// stateStream pushes the state over a WebSocket: the whole of it on
// connect, again whenever the store changes (saves within a short window
// collapse into one message), and every few seconds while something only
// in memory is moving (a launch, a seal, a fetch). The browser never asks.
func (s *Server) stateStream(w http.ResponseWriter, r *http.Request) {
	connection, err := terminalUpgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	defer connection.Close()
	connection.SetReadLimit(1 << 20)
	ticks, stop := s.store.Watch()
	defer stop()
	closed := make(chan struct{})
	go func() {
		defer close(closed)
		for {
			if _, _, err := connection.ReadMessage(); err != nil {
				return
			}
		}
	}()
	send := func() bool {
		connection.SetWriteDeadline(time.Now().Add(20 * time.Second))
		return connection.WriteJSON(s.stateFor(r)) == nil
	}
	if !send() {
		return
	}
	transient := time.NewTicker(3 * time.Second)
	defer transient.Stop()
	keepalive := time.NewTicker(acpKeepaliveInterval)
	defer keepalive.Stop()
	var pending <-chan time.Time
	for {
		select {
		case <-closed:
			return
		case <-ticks:
			if pending == nil {
				pending = time.After(150 * time.Millisecond)
			}
		case <-pending:
			pending = nil
			if !send() {
				return
			}
		case <-transient.C:
			if s.hasTransientWork() && !send() {
				return
			}
		case <-keepalive.C:
			if err := connection.WriteControl(websocket.PingMessage, nil, time.Now().Add(5*time.Second)); err != nil {
				return
			}
		}
	}
}

// hasTransientWork reports whether something changes without a store save:
// launches, seals, starts, option fetches and app starts in progress.
func (s *Server) hasTransientWork() bool {
	s.jobLaunchMu.Lock()
	launching := len(s.jobLaunching) > 0
	s.jobLaunchMu.Unlock()
	if launching {
		return true
	}
	s.sealMu.Lock()
	sealing := len(s.seals) > 0
	s.sealMu.Unlock()
	s.startMu.Lock()
	starting := len(s.starts) > 0
	s.startMu.Unlock()
	s.appMu.Lock()
	apps := len(s.appStarts) > 0
	s.appMu.Unlock()
	return sealing || starting || apps
}

type stateResponse struct {
	domain.Snapshot
	Recommendations   []domain.Recommendation  `json:"recommendations"`
	Engine            domain.CapsuleEngineInfo `json:"engine"`
	GitOAuthProviders []gitOAuthProviderInfo   `json:"git_oauth_providers"`
	CurrentUser       domain.PublicUser        `json:"current_user"`
	Preparing         []sessionPreparation     `json:"preparing"`
	Storage           storageInfo              `json:"storage"`
	Version           uint64                   `json:"version"`
}

func visibleSnapshot(snapshot domain.Snapshot, operator string) domain.Snapshot {
	operator = normalizeOperator(operator)
	snapshot.Artifacts = filterSlice(snapshot.Artifacts, func(artifact domain.Artifact) bool {
		return artifact.Scope != domain.ScopeUser || artifact.Subject == operator
	})
	snapshot.Recordings = filterSlice(snapshot.Recordings, func(recording domain.Recording) bool {
		return recording.Actor == operator
	})
	snapshot.MCPServers = filterSlice(snapshot.MCPServers, func(server domain.MCPServer) bool {
		return server.Operator == operator
	})
	snapshot.GitAccounts = filterSlice(snapshot.GitAccounts, func(account domain.GitAccount) bool {
		return account.CredentialScope == domain.CredentialScopeGlobal || account.Operator == operator
	})
	return snapshot
}

func filterSlice[T any](items []T, keep func(T) bool) []T {
	filtered := make([]T, 0, len(items))
	for _, item := range items {
		if keep(item) {
			filtered = append(filtered, item)
		}
	}
	return filtered
}
