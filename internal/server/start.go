package server

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"sync"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// RECORD and EDIT bring a capsule up on a runner. That is seconds when the
// runner already holds the base image and minutes when the image has to come
// out of the archive first, and the edge closes any request after 100 s. So
// the recording is created at once, the rest runs as a job, and the command
// answers with the live recording when it is quick and with progress when it
// is not; the browser follows the job and only sends shell lines once the
// capsule exists.
//
// The recording is durable and the job is not, so the two must never drift
// apart: a recording without a runtime always has a job. Every start is
// resumed when the server comes back (the runner reuses a capsule it already
// created for that recording), CANCEL RECORD stops a running start instead of
// being refused, and a start that fails cancels its recording.

const startAnswerWait = 3 * time.Second

const startResultLifetime = 15 * time.Minute

// startCancelWait bounds how long CANCEL RECORD waits for a stopped start job
// to wind down before it proceeds regardless.
const startCancelWait = 10 * time.Second

type startJob struct {
	mu     sync.Mutex
	status domain.StartStatus
	cancel context.CancelFunc
	done   chan struct{}
}

func (j *startJob) update(change func(*domain.StartStatus)) {
	j.mu.Lock()
	change(&j.status)
	j.status.UpdatedAt = time.Now().UTC()
	j.mu.Unlock()
}

func (j *startJob) snapshot() domain.StartStatus {
	j.mu.Lock()
	defer j.mu.Unlock()
	return j.status
}

// createCapsuleRecording records the request, starts bringing the capsule up
// and waits briefly. When the capsule is live before the wait ends the
// recording comes back with its runtime, as it always did; otherwise the
// recording comes back without one, together with the job to follow.
func (s *Server) createCapsuleRecording(req domain.CreateRecordingRequest) (domain.Recording, *domain.StartStatus, error) {
	if open, err := s.store.OpenRecording(req.Actor); err == nil {
		state := "is still open"
		if open.Runtime == nil || open.Runtime.ContainerID == "" {
			state = "is still starting"
		}
		return domain.Recording{}, nil, fmt.Errorf("your recording %s:%s %s; END RECORD or CANCEL RECORD it first: %w", open.Kind, open.Name, state, store.ErrConflict)
	}
	recording, err := s.store.CreateRecording(req)
	if err != nil {
		return domain.Recording{}, nil, err
	}
	job, err := s.beginStart(recording)
	if err != nil {
		return domain.Recording{}, nil, err
	}

	timer := time.NewTimer(s.startWait)
	defer timer.Stop()
	select {
	case <-job.done:
		status := job.snapshot()
		if status.Status != "done" {
			return domain.Recording{}, nil, fmt.Errorf("%s", status.Error)
		}
		return *status.Recording, nil, nil
	case <-timer.C:
		status := job.snapshot()
		return recording, &status, nil
	}
}

// beginStart registers the job for a recording that has no capsule yet and
// runs it in the background. A job already running for the recording is kept.
func (s *Server) beginStart(recording domain.Recording) (*startJob, error) {
	parents, err := s.recordingParents(recording)
	if err != nil {
		_, _ = s.store.CancelRecording(recording.ID, domain.CancelRecordingRequest{Actor: recording.Actor})
		return nil, err
	}
	now := time.Now().UTC()
	ctx, cancel := context.WithCancel(context.Background())
	job := &startJob{done: make(chan struct{}), cancel: cancel, status: domain.StartStatus{
		RecordingID: recording.ID, Status: "running", Stage: "prepare", Message: "Runner kiezen en basisimage controleren",
		StartedAt: now, UpdatedAt: now,
	}}
	s.startMu.Lock()
	for id, candidate := range s.starts {
		if status := candidate.snapshot(); status.Status != "running" && now.Sub(status.UpdatedAt) > startResultLifetime {
			delete(s.starts, id)
		}
	}
	if existing := s.starts[recording.ID]; existing != nil && existing.snapshot().Status == "running" {
		s.startMu.Unlock()
		cancel()
		return existing, nil
	}
	s.starts[recording.ID] = job
	s.startMu.Unlock()
	go s.runStart(ctx, job, recording, parents)
	return job, nil
}

// resumeStartingRecordings picks up every recording that was still coming up
// when the server last stopped. The runner keeps the capsule it made for a
// recording and hands it back, so a start interrupted by a deploy carries on
// where it was instead of leaving a recording nothing can finish.
func (s *Server) resumeStartingRecordings() {
	for _, recording := range s.store.StartingRecordings() {
		if _, err := s.beginStart(recording); err != nil {
			s.logger.Warn("resume capsule start", "recording", recording.ID, "error", err)
			continue
		}
		s.logger.Info("resuming capsule start after restart", "recording", recording.ID, "layer", string(recording.Kind)+":"+recording.Name)
	}
}

func (s *Server) runStart(ctx context.Context, job *startJob, recording domain.Recording, parents []domain.Artifact) {
	// Detached from the request on purpose: the request may be long gone.
	ctx = capsule.WithProgress(ctx, func(stage, message string, current, total int64) {
		job.update(func(status *domain.StartStatus) {
			status.Stage, status.Message, status.Current, status.Total = stage, message, current, total
		})
	})
	fail := func(err error) {
		_, _ = s.store.CancelRecording(recording.ID, domain.CancelRecordingRequest{Actor: recording.Actor})
		if errors.Is(err, context.Canceled) || ctx.Err() != nil {
			s.logger.Info("start recording cancelled", "recording", recording.ID)
			job.update(func(status *domain.StartStatus) {
				status.Status, status.Stage, status.Message = "cancelled", "cancelled", "Starten afgebroken"
				status.Current, status.Total = 0, 0
			})
		} else {
			s.logger.Warn("start recording failed", "recording", recording.ID, "error", err)
			job.update(func(status *domain.StartStatus) {
				status.Status, status.Error = "error", err.Error()
			})
		}
		close(job.done)
	}
	runtime, err := s.engine.StartRecording(ctx, recording, parents)
	if err != nil {
		fail(fmt.Errorf("start capsule recording: %w", err))
		return
	}
	recording.Runtime = &runtime
	updated, err := s.store.SetRecordingRuntime(recording.ID, recording.Actor, runtime)
	if err != nil {
		// The recording was cancelled or removed while the capsule came up:
		// the capsule is stale the moment it exists.
		_ = s.engine.Cancel(context.Background(), recording)
		fail(err)
		return
	}
	job.update(func(status *domain.StartStatus) {
		status.Status, status.Stage, status.Message = "done", "done", "Capsule staat"
		status.Current, status.Total = 0, 0
		status.Recording = &updated
	})
	close(job.done)
}

func (s *Server) startInProgress(recordingID string) bool {
	s.startMu.Lock()
	job := s.starts[recordingID]
	s.startMu.Unlock()
	return job != nil && job.snapshot().Status == "running"
}

// cancelStart stops a running start job and waits, bounded, for it to wind
// down. It reports whether a job was running.
func (s *Server) cancelStart(recordingID string) bool {
	s.startMu.Lock()
	job := s.starts[recordingID]
	s.startMu.Unlock()
	if job == nil || job.snapshot().Status != "running" {
		return false
	}
	job.cancel()
	select {
	case <-job.done:
	case <-time.After(s.startCancelWait):
		s.logger.Warn("start job did not stop in time; cancelling the recording regardless", "recording", recordingID)
	}
	return true
}

// getStart serves the start status of a recording to its owner.
func (s *Server) getStart(w http.ResponseWriter, r *http.Request) {
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
	s.startMu.Lock()
	job := s.starts[recordingID]
	s.startMu.Unlock()
	if job == nil {
		writeError(w, fmt.Errorf("no start is known for this recording: %w", store.ErrNotFound))
		return
	}
	writeJSON(w, http.StatusOK, job.snapshot())
}
