package server

import (
	"context"
	"fmt"
	"net/http"
	"strings"
	"sync"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// END RECORD is a job, not a request. Committing the container is quick;
// exporting a gigabyte and sending it to the archive in 1 MiB chunks is not,
// and the edge closes any HTTP request that lasts longer than 100 seconds. So
// the work runs detached, the command answers with the artifact when it is
// quick and with progress when it is not, and the browser follows the rest.

// sealAnswerWait is how long END RECORD waits before answering with progress.
const sealAnswerWait = 15 * time.Second

// sealResultLifetime keeps a finished seal's status around for late pollers.
const sealResultLifetime = 15 * time.Minute

type sealJob struct {
	mu     sync.Mutex
	status domain.SealStatus
	digest string // known once the snapshot is committed; keys the upload progress
	done   chan struct{}
}

func (j *sealJob) update(change func(*domain.SealStatus)) {
	j.mu.Lock()
	change(&j.status)
	j.status.UpdatedAt = time.Now().UTC()
	j.mu.Unlock()
}

func (j *sealJob) finish(change func(*domain.SealStatus)) {
	j.update(change)
	close(j.done)
}

func (j *sealJob) snapshot() (domain.SealStatus, string) {
	j.mu.Lock()
	defer j.mu.Unlock()
	return j.status, j.digest
}

// startSeal validates the request the way END RECORD always did, then runs the
// seal in the background. A seal already running for the recording is reused,
// so a retried END RECORD joins it instead of committing twice.
func (s *Server) startSeal(recordingID string, req domain.EndRecordingRequest) (*sealJob, error) {
	if s.terminalBusy(recordingID) {
		return nil, fmt.Errorf("an interactive command is still running; wait for it or send Ctrl-C: %w", store.ErrConflict)
	}
	recording, err := s.store.Recording(recordingID)
	if err != nil {
		return nil, err
	}
	open, err := s.store.OpenRecording(req.Actor)
	if err != nil || open.ID != recording.ID {
		return nil, store.ErrConflict
	}
	now := time.Now().UTC()
	s.sealMu.Lock()
	for id, candidate := range s.seals {
		status, _ := candidate.snapshot()
		if status.Status != "running" && now.Sub(status.UpdatedAt) > sealResultLifetime {
			delete(s.seals, id)
		}
	}
	if existing := s.seals[recordingID]; existing != nil {
		if status, _ := existing.snapshot(); status.Status == "running" {
			s.sealMu.Unlock()
			return existing, nil
		}
	}
	job := &sealJob{done: make(chan struct{}), status: domain.SealStatus{
		RecordingID: recordingID, Status: "running", Stage: "commit", Message: "Capsule wordt gecommit op de runner",
		StartedAt: now, UpdatedAt: now,
	}}
	s.seals[recordingID] = job
	s.sealMu.Unlock()
	go s.runSeal(job, recording, req)
	return job, nil
}

func (s *Server) runSeal(job *sealJob, recording domain.Recording, req domain.EndRecordingRequest) {
	// Detached from the request on purpose: the request may be long gone.
	ctx := context.Background()
	fail := func(err error) {
		s.logger.Warn("seal recording failed", "recording", recording.ID, "error", err)
		job.finish(func(status *domain.SealStatus) {
			status.Status = "error"
			status.Error = err.Error()
		})
	}
	snapshot, err := s.engine.Seal(ctx, recording)
	if err != nil {
		fail(fmt.Errorf("seal capsule recording: %w", err))
		return
	}
	job.update(func(status *domain.SealStatus) {
		status.Stage = "archive"
		status.Message = "Runner exporteert de image en uploadt naar het archief"
	})
	job.mu.Lock()
	job.digest = snapshot.Digest
	job.mu.Unlock()
	if err := s.archiveSealedSnapshot(ctx, snapshot); err != nil {
		fail(fmt.Errorf("archive capsule snapshot: %w", err))
		return
	}
	job.update(func(status *domain.SealStatus) {
		status.Stage = "finish"
		status.Message = "Artifact wordt vastgelegd"
		status.Current, status.Total = 0, 0
	})
	req.Snapshot = snapshot
	artifact, err := s.store.EndRecording(recording.ID, req)
	if err != nil {
		if s.snapshotArchive != nil {
			_ = s.snapshotArchive.RemoveArchivedSnapshot(ctx, snapshot)
		}
		fail(err)
		return
	}
	job.finish(func(status *domain.SealStatus) {
		status.Status = "done"
		status.Stage = "done"
		status.Message = "Opgeslagen"
		status.Artifact = &artifact
	})
}

// sealStatus reads a job and, while the archive upload runs, fills in how far
// the runner's chunks have come.
func (s *Server) sealStatus(job *sealJob) domain.SealStatus {
	status, digest := job.snapshot()
	if status.Status == "running" && status.Stage == "archive" && digest != "" {
		if current, total, ok := s.uploadProgressForDigest(digest); ok {
			status.Current, status.Total = current, total
			status.Message = "Runner uploadt de image naar het archief"
		}
	}
	return status
}

// awaitSeal waits up to wait for the job and reports whether it finished.
func (s *Server) awaitSeal(job *sealJob, wait time.Duration) (domain.SealStatus, bool) {
	timer := time.NewTimer(wait)
	defer timer.Stop()
	select {
	case <-job.done:
		return s.sealStatus(job), true
	case <-timer.C:
		return s.sealStatus(job), false
	}
}

func (s *Server) sealInProgress(recordingID string) bool {
	s.sealMu.Lock()
	job := s.seals[recordingID]
	s.sealMu.Unlock()
	if job == nil {
		return false
	}
	status, _ := job.snapshot()
	return status.Status == "running"
}

// getSeal serves the seal status of a recording to its owner.
func (s *Server) getSeal(w http.ResponseWriter, r *http.Request) {
	recordingID := strings.TrimSpace(r.PathValue("recordingID"))
	recording, err := s.store.Recording(recordingID)
	if err != nil {
		writeError(w, err)
		return
	}
	if operator := s.requestOperator(r, ""); !s.authDisabled && normalizeOperator(recording.Actor) != normalizeOperator(operator) {
		writeError(w, store.ErrNotFound)
		return
	}
	s.sealMu.Lock()
	job := s.seals[recordingID]
	s.sealMu.Unlock()
	if job == nil {
		writeError(w, fmt.Errorf("no seal is known for this recording: %w", store.ErrNotFound))
		return
	}
	writeJSON(w, http.StatusOK, s.sealStatus(job))
}
