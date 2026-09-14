package server

import (
	"easyacp/internal/domain"
	"net/http"
)

func (s *Server) createMCPServer(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateMCPServerRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	server, err := s.store.CreateMCPServer(req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, server)
}

func (s *Server) deleteMCPServer(w http.ResponseWriter, r *http.Request) {
	server, err := s.store.DeleteMCPServer(r.PathValue("mcpServerID"), s.requestOperator(r, r.URL.Query().Get("operator")))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, server)
}

func (s *Server) createGitRepository(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateGitRepositoryRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	created, err := s.store.CreateGitRepository(req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, created)
}

func (s *Server) updateGitRepository(w http.ResponseWriter, r *http.Request) {
	var req domain.UpdateGitRepositoryRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	repository, err := s.store.UpdateGitRepository(r.PathValue("repositoryID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, repository)
}

func (s *Server) deleteGitRepository(w http.ResponseWriter, r *http.Request) {
	repository, err := s.store.DeleteGitRepository(r.PathValue("repositoryID"), s.requestOperator(r, r.URL.Query().Get("operator")))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, repository)
}

func (s *Server) createGitAccount(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateGitAccountRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	if req.CredentialScope == domain.CredentialScopeGlobal {
		identity, ok := identityFromRequest(r)
		if !ok || identity.User.Role != domain.UserAdmin {
			writeJSON(w, http.StatusForbidden, map[string]string{"error": "admin role required for a global Git account"})
			return
		}
	}
	account, err := s.store.CreateGitAccount(req)
	if err == nil {
		go s.launchQueuedWorkflowPhases("git account added")
	}
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, account)
}

func (s *Server) deleteGitAccount(w http.ResponseWriter, r *http.Request) {
	account, err := s.store.DeleteGitAccount(r.PathValue("accountID"), s.requestOperator(r, r.URL.Query().Get("operator")))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, account)
}
