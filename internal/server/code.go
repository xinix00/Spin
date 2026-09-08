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

// The code browser shows a Job's files as Git holds them, read on demand
// from the workspace of its latest running Session; Spin stores none of it.
// Refs a person can pick: the working tree, HEAD, the Job branch and the
// base branch as the workspace fetched them.

func codeRefs(job domain.Job) []string {
	return []string{"workspace", "HEAD", "origin/" + job.Branch, "origin/" + job.BaseRef}
}

// codeWorkspace finds the Job's latest running workspace to read from.
func (s *Server) codeWorkspace(jobID string) (domain.Job, domain.CapsuleRuntime, error) {
	snapshot := s.store.Snapshot()
	var job domain.Job
	for _, candidate := range snapshot.Jobs {
		if candidate.ID == jobID {
			job = candidate
		}
	}
	if job.ID == "" {
		return domain.Job{}, domain.CapsuleRuntime{}, store.ErrNotFound
	}
	var latest *domain.Composition
	for index := range snapshot.Compositions {
		candidate := &snapshot.Compositions[index]
		if candidate.Git == nil || candidate.Runtime == nil || candidate.Runtime.Status != "ready" || candidate.Runtime.ContainerID == "" {
			continue
		}
		for _, session := range snapshot.Sessions {
			if session.ID == candidate.SessionID && session.JobID == job.ID && (latest == nil || candidate.CreatedAt.After(latest.CreatedAt)) {
				latest = candidate
			}
		}
	}
	if latest == nil {
		return job, domain.CapsuleRuntime{}, fmt.Errorf("de Job heeft geen draaiende workspace om code uit te lezen; start of hervat een stap: %w", store.ErrConflict)
	}
	return job, *latest.Runtime, nil
}

func (s *Server) codeTreeHandler(w http.ResponseWriter, r *http.Request) {
	browser, ok := s.engine.(capsule.WorkspaceBrowser)
	if !ok {
		writeError(w, fmt.Errorf("capsule engine %s cannot browse workspaces: %w", s.engine.Info().Driver, store.ErrConflict))
		return
	}
	job, runtime, err := s.codeWorkspace(r.PathValue("jobID"))
	if err != nil {
		writeError(w, err)
		return
	}
	ref := codeRef(job, r.URL.Query().Get("ref"))
	ctx, cancel := context.WithTimeout(r.Context(), 30*time.Second)
	defer cancel()
	tree, err := browser.ListWorkspace(ctx, runtime, ref)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ref": tree.Ref, "refs": codeRefs(job), "entries": tree.Entries})
}

func (s *Server) codeFileHandler(w http.ResponseWriter, r *http.Request) {
	browser, ok := s.engine.(capsule.WorkspaceBrowser)
	if !ok {
		writeError(w, fmt.Errorf("capsule engine %s cannot browse workspaces: %w", s.engine.Info().Driver, store.ErrConflict))
		return
	}
	job, runtime, err := s.codeWorkspace(r.PathValue("jobID"))
	if err != nil {
		writeError(w, err)
		return
	}
	path := strings.TrimSpace(r.URL.Query().Get("path"))
	if path == "" {
		writeError(w, fmt.Errorf("path is required: %w", store.ErrConflict))
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 30*time.Second)
	defer cancel()
	file, err := browser.ReadWorkspaceFile(ctx, runtime, codeRef(job, r.URL.Query().Get("ref")), path)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, file)
}

// codeRef accepts only the refs the Job offers; anything else is the
// working tree.
func codeRef(job domain.Job, requested string) string {
	requested = strings.TrimSpace(requested)
	for _, ref := range codeRefs(job) {
		if ref == requested {
			return ref
		}
	}
	return "workspace"
}
