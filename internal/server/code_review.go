package server

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

func (s *Server) createCodeReview(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateCodeReviewRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	revision, err := s.captureCodeReview(r.Context(), r.PathValue("jobID"), strings.TrimSpace(req.SessionID), req.Live, s.requestOperator(r, r.URL.Query().Get("operator")))
	if err != nil {
		writeError(w, err)
		return
	}
	bundle, err := s.store.CodeReviewBundle(revision.ID)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, bundle)
}

func (s *Server) getCodeReview(w http.ResponseWriter, r *http.Request) {
	bundle, err := s.store.CodeReviewBundle(r.PathValue("revisionID"))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, bundle)
}

func (s *Server) createCodeReviewComment(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateCodeReviewCommentRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	revision, err := s.store.CodeReviewRevision(r.PathValue("revisionID"))
	if err != nil {
		writeError(w, err)
		return
	}
	author := s.requestOperator(r, req.Operator)
	current, err := s.captureCodeReview(r.Context(), revision.JobID, revision.SessionID, revision.Live, author)
	if err != nil {
		writeError(w, err)
		return
	}
	if current.ID != revision.ID {
		writeError(w, fmt.Errorf("changes moved while this review was open; reopen the latest revision: %w", store.ErrConflict))
		return
	}
	comment, err := s.store.AddCodeReviewComment(revision.ID, author, req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, comment)
}

func (s *Server) captureCodeReview(ctx context.Context, jobID, sessionID string, live bool, author string) (domain.CodeReviewRevision, error) {
	snapshot := s.store.Snapshot()
	jobIndex := -1
	for index := range snapshot.Jobs {
		if snapshot.Jobs[index].ID == jobID {
			jobIndex = index
			break
		}
	}
	if jobIndex < 0 {
		return domain.CodeReviewRevision{}, store.ErrNotFound
	}
	job := snapshot.Jobs[jobIndex]
	revision := domain.CodeReviewRevision{
		JobID: job.ID, ContextPhaseRunID: job.CurrentPhaseRunID, Scope: "job",
		ScopeKey: "job:" + job.ID, Live: false, CreatedBy: author,
	}
	var changes capsule.WorkspaceChanges
	var err error
	if sessionID == "" {
		changes, err = s.inspectJobChanges(ctx, job.ID, author, "")
	} else {
		var session domain.Session
		for _, candidate := range snapshot.Sessions {
			if candidate.ID == sessionID && candidate.JobID == job.ID {
				session = candidate
				break
			}
		}
		if session.ID == "" {
			return domain.CodeReviewRevision{}, store.ErrNotFound
		}
		var run domain.PhaseRun
		for _, candidate := range snapshot.PhaseRuns {
			if candidate.SessionID == session.ID && candidate.JobID == job.ID {
				run = candidate
				break
			}
		}
		if run.ID == "" {
			return domain.CodeReviewRevision{}, store.ErrNotFound
		}
		revision.SourcePhaseRunID = run.ID
		revision.SessionID = session.ID
		revision.PhaseID = run.PhaseID
		revision.PhaseName = run.PhaseName
		revision.Attempt = run.Attempt
		revision.Scope = "phase"
		revision.ScopeKey = "job:" + job.ID + ":phase:" + run.PhaseID
		revision.Live = live
		if live {
			// The read-only review belongs to the signed-in author, but the
			// running capsule remains bound to the operator who started it.
			changes, err = s.inspectSessionChanges(ctx, session.ID, session.Operator)
		} else {
			changes, err = s.inspectJobChanges(ctx, job.ID, author, session.ID)
		}
	}
	if err != nil {
		return domain.CodeReviewRevision{}, err
	}
	revision.Branch = changes.Branch
	revision.Added = changes.Added
	revision.Deleted = changes.Deleted
	revision.Files = make([]domain.CodeReviewFile, 0, len(changes.Files))
	for _, file := range changes.Files {
		revision.Files = append(revision.Files, domain.CodeReviewFile{
			Path: file.Path, Status: file.Status, Added: file.Added, Deleted: file.Deleted,
			Patch: file.Patch, Binary: file.Binary, Truncated: file.Truncated,
		})
	}
	digestInput, err := json.Marshal(struct {
		Branch  string                  `json:"branch"`
		Added   int                     `json:"added"`
		Deleted int                     `json:"deleted"`
		Files   []domain.CodeReviewFile `json:"files"`
	}{revision.Branch, revision.Added, revision.Deleted, revision.Files})
	if err != nil {
		return domain.CodeReviewRevision{}, fmt.Errorf("encode code review: %w", err)
	}
	digest := sha256.Sum256(digestInput)
	revision.Digest = hex.EncodeToString(digest[:])
	return s.store.SaveCodeReviewRevision(revision)
}
