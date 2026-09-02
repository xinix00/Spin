package store

import (
	"fmt"
	"slices"
	"strings"
	"time"

	"easyacp/internal/domain"
)

func codeReviewSummary(revision domain.CodeReviewRevision) domain.CodeReviewRevisionSummary {
	return domain.CodeReviewRevisionSummary{
		ID: revision.ID, JobID: revision.JobID, SourcePhaseRunID: revision.SourcePhaseRunID,
		ContextPhaseRunID: revision.ContextPhaseRunID, SessionID: revision.SessionID,
		PhaseID: revision.PhaseID, PhaseName: revision.PhaseName, Attempt: revision.Attempt,
		Scope: revision.Scope, ScopeKey: revision.ScopeKey, Branch: revision.Branch,
		Added: revision.Added, Deleted: revision.Deleted, FileCount: len(revision.Files),
		CreatedBy: revision.CreatedBy, CreatedAt: revision.CreatedAt,
	}
}

func (s *Store) SaveCodeReviewRevision(revision domain.CodeReviewRevision) (domain.CodeReviewRevision, error) {
	revision.CreatedBy = normalizeSubject(revision.CreatedBy)
	revision.JobID = strings.TrimSpace(revision.JobID)
	revision.SessionID = strings.TrimSpace(revision.SessionID)
	revision.Scope = strings.ToLower(strings.TrimSpace(revision.Scope))
	revision.ScopeKey = strings.TrimSpace(revision.ScopeKey)
	revision.Digest = strings.TrimSpace(revision.Digest)
	if revision.CreatedBy == "" || revision.JobID == "" || revision.ScopeKey == "" || revision.Digest == "" || (revision.Scope != "job" && revision.Scope != "phase") {
		return domain.CodeReviewRevision{}, fmt.Errorf("author, Job, scope and digest are required: %w", ErrConflict)
	}
	if revision.Files == nil {
		revision.Files = []domain.CodeReviewFile{}
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, ok := s.state.Jobs[revision.JobID]; !ok {
		return domain.CodeReviewRevision{}, ErrNotFound
	}
	for _, runID := range []string{revision.SourcePhaseRunID, revision.ContextPhaseRunID} {
		if runID == "" {
			continue
		}
		run, ok := s.state.PhaseRuns[runID]
		if !ok || run.JobID != revision.JobID {
			return domain.CodeReviewRevision{}, ErrNotFound
		}
	}
	for _, existing := range s.state.CodeReviewRevisions {
		if existing.ScopeKey == revision.ScopeKey && existing.Digest == revision.Digest && existing.SourcePhaseRunID == revision.SourcePhaseRunID && existing.ContextPhaseRunID == revision.ContextPhaseRunID && existing.Live == revision.Live {
			return existing, nil
		}
	}
	revision.ID = newID("rev")
	revision.CreatedAt = time.Now().UTC()
	s.state.CodeReviewRevisions[revision.ID] = revision
	return revision, s.saveLocked()
}

func (s *Store) CodeReviewRevision(revisionID string) (domain.CodeReviewRevision, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	revision, ok := s.state.CodeReviewRevisions[strings.TrimSpace(revisionID)]
	if !ok {
		return domain.CodeReviewRevision{}, ErrNotFound
	}
	return revision, nil
}

func (s *Store) CodeReviewBundle(revisionID string) (domain.CodeReviewBundle, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	revision, ok := s.state.CodeReviewRevisions[strings.TrimSpace(revisionID)]
	if !ok {
		return domain.CodeReviewBundle{}, ErrNotFound
	}
	history := make([]domain.CodeReviewRevisionSummary, 0)
	comments := make([]domain.CodeReviewComment, 0)
	for _, candidate := range s.state.CodeReviewRevisions {
		if candidate.ScopeKey == revision.ScopeKey {
			history = append(history, codeReviewSummary(candidate))
		}
	}
	for _, comment := range s.state.CodeReviewComments {
		if comment.RevisionID == revision.ID {
			comments = append(comments, comment)
		}
	}
	slices.SortFunc(history, func(a, b domain.CodeReviewRevisionSummary) int {
		if revision.Scope == "phase" && a.Attempt != b.Attempt {
			return a.Attempt - b.Attempt
		}
		return a.CreatedAt.Compare(b.CreatedAt)
	})
	slices.SortFunc(comments, func(a, b domain.CodeReviewComment) int { return a.CreatedAt.Compare(b.CreatedAt) })
	latestID := latestCodeReviewRevisionIDLocked(s.state.CodeReviewRevisions, s.state.PhaseRuns, revision)
	return domain.CodeReviewBundle{Revision: revision, History: history, Comments: comments, LatestRevisionID: latestID, Annotatable: latestID == revision.ID}, nil
}

func latestCodeReviewRevisionIDLocked(revisions map[string]domain.CodeReviewRevision, phaseRuns map[string]domain.PhaseRun, reference domain.CodeReviewRevision) string {
	latestAttempt := 0
	if reference.Scope == "phase" {
		for _, run := range phaseRuns {
			if run.JobID == reference.JobID && run.PhaseID == reference.PhaseID && run.Attempt > latestAttempt {
				latestAttempt = run.Attempt
			}
		}
	}
	var latest domain.CodeReviewRevision
	for _, candidate := range revisions {
		if candidate.ScopeKey != reference.ScopeKey || reference.Scope == "phase" && candidate.Attempt != latestAttempt {
			continue
		}
		if latest.ID == "" || candidate.CreatedAt.After(latest.CreatedAt) {
			latest = candidate
		}
	}
	return latest.ID
}

func (s *Store) AddCodeReviewComment(revisionID, author string, req domain.CreateCodeReviewCommentRequest) (domain.CodeReviewComment, error) {
	author = normalizeSubject(author)
	path := strings.TrimSpace(req.Path)
	side := strings.ToLower(strings.TrimSpace(req.Side))
	selected := strings.TrimSpace(req.SelectedText)
	body := strings.TrimSpace(req.Body)
	if author == "" || path == "" || (side != "old" && side != "new") || req.StartLine < 1 || req.EndLine < req.StartLine || selected == "" || body == "" {
		return domain.CodeReviewComment{}, fmt.Errorf("path, side, line selection and comment are required: %w", ErrConflict)
	}
	if len(selected) > 32*1024 || len(body) > 8*1024 {
		return domain.CodeReviewComment{}, fmt.Errorf("code selection or comment is too large: %w", ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	revision, ok := s.state.CodeReviewRevisions[strings.TrimSpace(revisionID)]
	if !ok {
		return domain.CodeReviewComment{}, ErrNotFound
	}
	if latestCodeReviewRevisionIDLocked(s.state.CodeReviewRevisions, s.state.PhaseRuns, revision) != revision.ID {
		return domain.CodeReviewComment{}, fmt.Errorf("code review revision is historical; open the latest changes: %w", ErrConflict)
	}
	fileExists := false
	for _, file := range revision.Files {
		if file.Path == path {
			fileExists = true
			break
		}
	}
	if !fileExists {
		return domain.CodeReviewComment{}, ErrNotFound
	}
	comment := domain.CodeReviewComment{
		ID: newID("crc"), RevisionID: revision.ID, Path: path, Side: side,
		StartLine: req.StartLine, EndLine: req.EndLine, Selected: selected,
		Body: body, Author: author, CreatedAt: time.Now().UTC(),
	}
	s.state.CodeReviewComments[comment.ID] = comment
	return comment, s.saveLocked()
}

// JobWorkspaceHistory returns the materialized Session history needed for a
// read-only review. Access control remains at the authenticated HTTP boundary;
// unlike deletion, review is intentionally collaborative.
func (s *Store) JobWorkspaceHistory(jobID string) (domain.Job, []domain.Composition, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	job, ok := s.state.Jobs[strings.TrimSpace(jobID)]
	if !ok {
		return domain.Job{}, nil, ErrNotFound
	}
	sessionIDs := map[string]bool{}
	for _, session := range s.state.Sessions {
		if session.JobID == job.ID {
			sessionIDs[session.ID] = true
		}
	}
	compositions := make([]domain.Composition, 0)
	for _, composition := range s.state.Compositions {
		if sessionIDs[composition.SessionID] {
			compositions = append(compositions, composition)
		}
	}
	slices.SortFunc(compositions, func(a, b domain.Composition) int { return a.CreatedAt.Compare(b.CreatedAt) })
	return job, compositions, nil
}
