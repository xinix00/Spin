package server

import (
	"context"
	"fmt"
	"net/http"
	"strings"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// Explore browses a repository from Connections → Git, read on demand
// through a runner that keeps a shallow clone per repository; Spin stores
// none of it. The person picks a repository and a branch.
func (s *Server) exploreRepository(ctx context.Context, repositoryID, operator string) (domain.GitRepository, *capsule.GitAuthentication, error) {
	var repository domain.GitRepository
	for _, candidate := range s.store.Snapshot().GitRepositories {
		if candidate.ID == repositoryID {
			repository = candidate
		}
	}
	if repository.ID == "" {
		return domain.GitRepository{}, nil, store.ErrNotFound
	}
	workspace := &domain.GitWorkspace{RepositoryID: repository.ID, RepositoryName: repository.Name, RemoteURL: repository.RemoteURL, Provider: repository.Provider, CredentialScope: repository.CredentialScope}
	authentication := &capsule.GitAuthentication{}
	account, authenticated, err := s.gitAccountForWorkspace(ctx, workspace, operator)
	if err != nil {
		return repository, nil, err
	}
	if authenticated {
		username := account.Login
		if account.Provider == "gitlab" {
			username = "oauth2"
		}
		authentication = &capsule.GitAuthentication{Username: username, Password: account.AccessToken}
	}
	return repository, authentication, nil
}

func (s *Server) exploreHandler(w http.ResponseWriter, r *http.Request) {
	browser, ok := s.engine.(capsule.RepositoryBrowser)
	if !ok {
		writeError(w, fmt.Errorf("capsule engine %s cannot browse repositories; a runner is needed: %w", s.engine.Info().Driver, store.ErrConflict))
		return
	}
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	ctx, cancel := context.WithTimeout(r.Context(), 60*time.Second)
	defer cancel()
	repository, authentication, err := s.exploreRepository(ctx, r.PathValue("repositoryID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	mode := r.PathValue("mode")
	ref := strings.TrimSpace(r.URL.Query().Get("ref"))
	if ref == "" {
		ref = repository.DefaultRef
	}
	result, err := browser.BrowseRepository(ctx, capsule.RepositoryBrowse{
		RemoteURL: repository.RemoteURL, CacheKey: repository.ID, Mode: mode, Ref: ref,
		Path: strings.TrimSpace(r.URL.Query().Get("path")), Authentication: authentication,
	})
	if err != nil {
		writeError(w, err)
		return
	}
	switch mode {
	case "refs":
		refs := result.Refs
		if refs == nil {
			refs = []capsule.RepositoryRef{}
		}
		writeJSON(w, http.StatusOK, map[string]any{"refs": refs, "default_ref": repository.DefaultRef})
	case "tree":
		if result.Tree == nil {
			result.Tree = &capsule.WorkspaceTree{Ref: ref, Entries: []capsule.WorkspaceEntry{}}
		}
		writeJSON(w, http.StatusOK, map[string]any{"ref": result.Tree.Ref, "entries": result.Tree.Entries})
	default:
		writeJSON(w, http.StatusOK, result.File)
	}
}
