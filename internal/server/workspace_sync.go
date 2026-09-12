package server

import (
	"context"
	"sync"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

// Work in progress is pushed: after every agent turn the Session's dirty
// files become a WIP commit and the Session branch goes to the remote. A
// Session can then continue on any runner, and a lost Docker volume loses
// nothing. ACCEPT folds the WIP commits into the one commit that lands on
// the Job branch and removes the Session branch from the remote.

const workspaceSyncMinInterval = 20 * time.Second

type workspaceSyncs struct {
	mu   sync.Mutex
	last map[string]time.Time
}

// gitAuthenticationFor is the operator's Git identity for a push from a
// composition's workspace, resolved late; without an account only the
// author name and email are set.
func (s *Server) gitAuthenticationFor(ctx context.Context, composition domain.Composition) (*capsule.GitAuthentication, error) {
	return s.gitAuthenticationForWorkspace(ctx, composition.Git, composition.Operator)
}

// gitAuthenticationForWorkspace is the credential and author for one
// repository of a composition; repositories of one Job may differ in scope.
func (s *Server) gitAuthenticationForWorkspace(ctx context.Context, workspace *domain.GitWorkspace, operator string) (*capsule.GitAuthentication, error) {
	authentication := &capsule.GitAuthentication{}
	account, authenticated, err := s.gitAccountForWorkspace(ctx, workspace, operator)
	if err != nil {
		return nil, err
	}
	if authenticated {
		username := account.Login
		if account.Provider == "gitlab" {
			username = "oauth2"
		}
		authentication.Username, authentication.Password = username, account.AccessToken
		authentication.AuthorName, authentication.AuthorEmail = account.Name, account.Email
	}
	if workspace != nil {
		if authentication.AuthorName == "" {
			authentication.AuthorName = workspace.AuthorName
		}
		if authentication.AuthorEmail == "" {
			authentication.AuthorEmail = workspace.AuthorEmail
		}
	}
	return authentication, nil
}

// syncWorkspace pushes a Session's work in progress when its phase may
// change code and its capsule runs. Calls closer together than the minimum
// interval are dropped: the next turn pushes again anyway.
func (s *Server) syncWorkspace(sessionID string) {
	s.syncWorkspaceWithin(sessionID, workspaceSyncMinInterval)
}

// syncWorkspaceWithin syncs unless a sync ran less than minInterval ago;
// zero syncs now, for a capsule that is about to go.
func (s *Server) syncWorkspaceWithin(sessionID string, minInterval time.Duration) {
	s.workspaceSyncs.mu.Lock()
	if s.workspaceSyncs.last == nil {
		s.workspaceSyncs.last = map[string]time.Time{}
	}
	if last, ok := s.workspaceSyncs.last[sessionID]; ok && time.Since(last) < minInterval {
		s.workspaceSyncs.mu.Unlock()
		return
	}
	s.workspaceSyncs.last[sessionID] = time.Now()
	s.workspaceSyncs.mu.Unlock()

	syncer, ok := s.engine.(capsule.WorkspaceSyncer)
	if !ok {
		return
	}
	var session domain.Session
	for _, candidate := range s.store.Snapshot().Sessions {
		if candidate.ID == sessionID {
			session = candidate
		}
	}
	if session.ID == "" || session.GitRef == "" {
		return
	}
	if session.PhaseRunID != "" {
		_, _, _, phase, _, _, err := s.store.WorkflowForSession(sessionID)
		if err != nil || !phase.AllowChanges {
			return
		}
	}
	_, composition, err := s.sessionComposition(sessionID, session.Operator)
	if err != nil || composition.Runtime == nil || composition.Runtime.Status != "ready" || composition.Git == nil {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Minute)
	defer cancel()
	// Every repository the Session changes goes to its own Session branch;
	// the head kept on the Session is the main repository's.
	for index, workspace := range composition.ChangedWorkspaces() {
		authentication, err := s.gitAuthenticationForWorkspace(ctx, &workspace, composition.Operator)
		if err != nil {
			s.logger.Warn("sync workspace: git account", "session", sessionID, "repository", workspace.RepositoryName, "error", err)
			return
		}
		result, err := syncer.SyncWorkspace(ctx, *composition.Runtime, capsule.WorkspaceSync{Path: workspace.Path, SessionRef: session.GitRef, Authentication: authentication})
		if err != nil {
			s.logger.Warn("sync workspace", "session", sessionID, "repository", workspace.RepositoryName, "error", err)
			return
		}
		if index == 0 && (result.Pushed || session.SyncedHead != result.Head) {
			if _, err := s.store.SetSessionSync(sessionID, result.Head); err != nil {
				s.logger.Warn("record workspace sync", "session", sessionID, "error", err)
			}
		}
		if result.Pushed {
			s.logger.Info("pushed work in progress", "session", sessionID, "repository", workspace.RepositoryName, "ref", session.GitRef, "head", result.Head, "committed", result.Committed)
		}
	}
}
