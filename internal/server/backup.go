package server

import (
	"context"
	"easyacp/replica"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
)

const maxBackupUploadBytes int64 = 64 << 30

const restoreJobResultLifetime = 6 * time.Hour

const restoreProgressMediaType = "application/x-ndjson"

type restoreResponse struct {
	Status       string `json:"status"`
	Users        int    `json:"users"`
	Jobs         int    `json:"jobs"`
	Templates    int    `json:"templates"`
	Deliverables int    `json:"deliverables"`
	Attachments  int    `json:"attachments"`
	Snapshots    int    `json:"snapshots"`
}

type backupTicket struct {
	UserID    string
	SessionID string
	ExpiresAt time.Time
}

type restoreProgressEvent struct {
	Type    string           `json:"type"`
	Stage   string           `json:"stage,omitempty"`
	Message string           `json:"message,omitempty"`
	Current int              `json:"current,omitempty"`
	Total   int              `json:"total,omitempty"`
	Result  *restoreResponse `json:"result,omitempty"`
	Error   string           `json:"error,omitempty"`
}

type restoreJob struct {
	ID        string
	Status    string
	Stage     string
	Message   string
	Current   int
	Total     int
	Result    *restoreResponse
	Error     string
	ExpiresAt time.Time
}

type restoreJobResponse struct {
	ID        string           `json:"id"`
	Status    string           `json:"status"`
	Stage     string           `json:"stage,omitempty"`
	Message   string           `json:"message,omitempty"`
	Current   int              `json:"current,omitempty"`
	Total     int              `json:"total,omitempty"`
	Result    *restoreResponse `json:"result,omitempty"`
	Error     string           `json:"error,omitempty"`
	ExpiresAt time.Time        `json:"expires_at"`
}

type restoreProgressWriter struct {
	w       http.ResponseWriter
	enabled bool
	started bool
}

func newRestoreProgressWriter(w http.ResponseWriter, r *http.Request) *restoreProgressWriter {
	return &restoreProgressWriter{w: w, enabled: strings.Contains(r.Header.Get("Accept"), restoreProgressMediaType)}
}

func (p *restoreProgressWriter) send(event restoreProgressEvent) {
	if !p.enabled {
		return
	}
	if !p.started {
		p.w.Header().Set("Content-Type", restoreProgressMediaType)
		p.w.Header().Set("Cache-Control", "no-store")
		p.w.Header().Set("X-Accel-Buffering", "no")
		p.started = true
	}
	if err := json.NewEncoder(p.w).Encode(event); err != nil {
		return
	}
	if flusher, ok := p.w.(http.Flusher); ok {
		flusher.Flush()
	}
}

func (p *restoreProgressWriter) progress(stage, message string, current, total int) {
	p.send(restoreProgressEvent{Type: "progress", Stage: stage, Message: message, Current: current, Total: total})
}

func (p *restoreProgressWriter) fail(status int, err error) {
	if p.started {
		p.send(restoreProgressEvent{Type: "error", Error: err.Error()})
		return
	}
	writeJSON(p.w, status, map[string]string{"error": err.Error()})
}

// A backup streams the live database: Spin pauses its writes for the
// duration (the single database connection is held), the file goes out as a
// zip next to the portable master key, and nothing is copied on the volume
// first. A ticket carries the admin's right into the URL a browser download
// can follow.
func (s *Server) downloadBackup(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	s.streamBackup(w, r)
}

func (s *Server) createBackupTicket(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	token, err := randomOAuthValue(32)
	if err != nil {
		writeError(w, err)
		return
	}
	identity, authenticated := identityFromRequest(r)
	ticket := backupTicket{ExpiresAt: time.Now().Add(time.Minute)}
	if authenticated {
		ticket.UserID = identity.User.ID
		ticket.SessionID = identity.Session.ID
	}
	now := time.Now()
	s.backupTicketMu.Lock()
	for key, candidate := range s.backupTickets {
		if !candidate.ExpiresAt.After(now) {
			delete(s.backupTickets, key)
		}
	}
	s.backupTickets[secretHash(token)] = ticket
	s.backupTicketMu.Unlock()
	writeJSON(w, http.StatusCreated, map[string]string{"url": "/api/backup?ticket=" + token})
}

func (s *Server) downloadBackupWithTicket(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	identity, authenticated := identityFromRequest(r)
	key := secretHash(r.URL.Query().Get("ticket"))
	s.backupTicketMu.Lock()
	ticket, ok := s.backupTickets[key]
	delete(s.backupTickets, key)
	s.backupTicketMu.Unlock()
	if !ok || !ticket.ExpiresAt.After(time.Now()) || (authenticated && (ticket.UserID != identity.User.ID || ticket.SessionID != identity.Session.ID)) {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "backup download ticket is invalid or expired"})
		return
	}
	s.streamBackup(w, r)
}

// streamBackup pauses writes and streams the database. One at a time: a
// second request while one streams is told to wait.
func (s *Server) streamBackup(w http.ResponseWriter, r *http.Request) {
	if s.database == nil {
		writeError(w, fmt.Errorf("SQLite backup is not configured: %w", store.ErrConflict))
		return
	}
	if !s.paused.CompareAndSwap(false, true) {
		w.Header().Set("Retry-After", "30")
		writeJSON(w, http.StatusServiceUnavailable, map[string]string{"error": "a backup is already streaming; try again in a moment"})
		return
	}
	defer s.paused.Store(false)
	portable, err := s.store.ExportPortableState()
	if err != nil {
		writeError(w, err)
		return
	}
	filename := "spin-backup-" + time.Now().UTC().Format("20060102-150405Z") + ".zip"
	w.Header().Set("Content-Type", "application/zip")
	w.Header().Set("Content-Disposition", `attachment; filename="`+filename+`"`)
	w.Header().Set("X-Spin-Backup-Contains-Secrets", "true")
	s.logger.Info("backup stream started; writes pause until it ends")
	if err := s.database.StreamBackup(r.Context(), w, portable.MasterKey); err != nil {
		s.logger.Warn("stream SQLite backup", "error", err)
	}
	s.logger.Info("backup stream ended")
}

// pausedForBackup answers a write with 503 while a backup streams, so a
// runner waits instead of timing out on a hanging request.
func (s *Server) pausedForBackup(w http.ResponseWriter) bool {
	if !s.paused.Load() {
		return false
	}
	w.Header().Set("Retry-After", "15")
	writeJSON(w, http.StatusServiceUnavailable, map[string]string{"error": "Spin is pausing writes while a backup streams; retry shortly"})
	return true
}

func (s *Server) restoreBackup(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	if s.database == nil {
		writeError(w, fmt.Errorf("SQLite restore is not configured: %w", store.ErrConflict))
		return
	}
	if s.hasInteractiveActivity() {
		writeJSON(w, http.StatusConflict, map[string]string{"error": "stop active terminals, chats and background Job launches before restoring a backup"})
		return
	}
	if !s.backupMu.TryLock() {
		writeJSON(w, http.StatusConflict, map[string]string{"error": "another backup or restore is already running"})
		return
	}
	defer s.backupMu.Unlock()
	progress := newRestoreProgressWriter(w, r)

	r.Body = http.MaxBytesReader(w, r.Body, maxBackupUploadBytes)
	staged, err := s.database.StageBackup(r.Context(), r.Body, maxBackupUploadBytes)
	if err != nil {
		progress.fail(http.StatusBadRequest, err)
		return
	}
	defer staged.Close()
	s.restoreStagedBackup(w, r, staged, progress)
}

// completeRestoreUpload stages an assembled backup and starts the restore job.
func (s *Server) completeRestoreUpload(w http.ResponseWriter, r *http.Request, upload *chunkedUpload) {
	if s.hasInteractiveActivity() {
		writeJSON(w, http.StatusConflict, map[string]string{"error": "stop active terminals, chats and background Job launches before restoring a backup"})
		return
	}
	jobID, err := randomOAuthValue(24)
	if err != nil {
		writeError(w, err)
		return
	}
	if !s.backupMu.TryLock() {
		writeJSON(w, http.StatusConflict, map[string]string{"error": "another backup or restore is already running"})
		return
	}
	s.removeUpload(upload)
	staged, err := upload.Backup.Stage(upload.Size)
	if err != nil {
		s.backupMu.Unlock()
		_ = upload.Backup.Close()
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": err.Error()})
		return
	}
	job := &restoreJob{
		ID: jobID, Status: "running", Stage: "open", Message: "Restore wordt gestart",
		ExpiresAt: time.Now().Add(restoreJobResultLifetime),
	}
	s.storeRestoreJob(job)
	initial := s.restoreJobResult(job)
	go s.runRestoreJob(job, staged)
	w.Header().Set("Location", "/api/restores/"+job.ID)
	w.Header().Set("Retry-After", "1")
	writeJSON(w, http.StatusAccepted, initial)
}

func (s *Server) restoreStagedBackup(w http.ResponseWriter, r *http.Request, staged *persistence.StagedBackup, progress *restoreProgressWriter) {
	result, status, err := s.performRestore(r.Context(), staged, progress.progress)
	if err != nil {
		progress.fail(status, err)
		return
	}
	if progress.enabled {
		progress.send(restoreProgressEvent{Type: "complete", Stage: "complete", Message: "Restore compleet", Result: &result})
		return
	}
	writeJSON(w, http.StatusOK, result)
}

func (s *Server) performRestore(ctx context.Context, staged *persistence.StagedBackup, progress func(string, string, int, int)) (restoreResponse, int, error) {
	if progress == nil {
		progress = func(string, string, int, int) {}
	}
	progress("open", "Upload compleet · database geopend", 1, 1)
	stateJSON, err := staged.Database.ReadFile("state")
	if err != nil {
		return restoreResponse{}, http.StatusBadRequest, errors.New("Spin backup contains no state")
	}
	inspection, err := s.store.InspectPortableState(stateJSON, staged.MasterKey)
	if err != nil {
		return restoreResponse{}, http.StatusBadRequest, err
	}
	progress("state", "State en credentials ontsleuteld", 1, 1)
	if err := validateDatabaseObjects(ctx, staged.Database, inspection, progress); err != nil {
		return restoreResponse{}, http.StatusBadRequest, err
	}

	progress("rollback", "Veilig rollbackpunt van de huidige database maken", 0, 0)
	rollback, err := s.database.RollbackPoint(ctx)
	if err != nil {
		return restoreResponse{}, http.StatusInternalServerError, err
	}
	defer rollback.Close()
	progress("install", "Gevalideerde database activeren", 0, 0)
	if err := s.database.RestoreFrom(ctx, staged); err != nil {
		return restoreResponse{}, http.StatusInternalServerError, err
	}
	progress("secrets", "Credentials onder de server-key opnieuw versleutelen", 0, 0)
	if err := s.store.RestorePortableState(stateJSON, staged.MasterKey); err != nil {
		rollbackErr := s.database.RestoreFrom(context.Background(), rollback)
		return restoreResponse{}, http.StatusInternalServerError, errors.Join(err, rollbackErr)
	}
	s.csrfTokens.clear()
	s.workflowMu.Lock()
	s.workflowTokens = map[string]string{}
	s.workflowMu.Unlock()
	if s.runnerBroker != nil {
		progress("runners", "Verbonden runners opnieuw laten aanmelden", 0, 0)
		if closed := s.runnerBroker.DisconnectAll("Spin state restored; reconnect to register again"); closed > 0 {
			s.logger.Info("restore closed runner sockets for re-registration", "runners", closed)
		}
	}
	result := restoreResponse{
		Status: "restored", Users: inspection.Users, Jobs: inspection.Jobs, Templates: inspection.Templates,
		Deliverables: inspection.Deliverables, Attachments: len(inspection.Attachments), Snapshots: restorableSnapshotCount(inspection.Artifacts),
	}
	return result, http.StatusOK, nil
}

func (s *Server) runRestoreJob(job *restoreJob, staged *persistence.StagedBackup) {
	defer s.backupMu.Unlock()
	defer staged.Close()
	result, _, err := s.performRestore(context.Background(), staged, func(stage, message string, current, total int) {
		s.updateRestoreJob(job, func(candidate *restoreJob) {
			candidate.Stage = stage
			candidate.Message = message
			candidate.Current = current
			candidate.Total = total
		})
	})
	if err != nil {
		s.updateRestoreJob(job, func(candidate *restoreJob) {
			candidate.Status = "error"
			candidate.Error = err.Error()
			candidate.ExpiresAt = time.Now().Add(restoreJobResultLifetime)
		})
		s.logger.Warn("background restore failed", "error", err)
		return
	}
	s.updateRestoreJob(job, func(candidate *restoreJob) {
		candidate.Status = "complete"
		candidate.Stage = "complete"
		candidate.Message = "Restore compleet"
		candidate.Current = 1
		candidate.Total = 1
		candidate.Result = &result
		candidate.ExpiresAt = time.Now().Add(restoreJobResultLifetime)
	})
}

func (s *Server) getRestoreJob(w http.ResponseWriter, r *http.Request) {
	id := strings.TrimSpace(r.PathValue("restoreID"))
	now := time.Now()
	s.restoreJobMu.Lock()
	job, ok := s.restoreJobs[id]
	if ok && job.Status != "running" && !job.ExpiresAt.After(now) {
		delete(s.restoreJobs, id)
		ok = false
	}
	var result restoreJobResponse
	if ok {
		result = restoreJobResponseFrom(job)
	}
	s.restoreJobMu.Unlock()
	if !ok {
		writeJSON(w, http.StatusNotFound, map[string]string{"error": "restore status not found or expired"})
		return
	}
	writeJSON(w, http.StatusOK, result)
}

func (s *Server) storeRestoreJob(job *restoreJob) {
	now := time.Now()
	s.restoreJobMu.Lock()
	for id, candidate := range s.restoreJobs {
		if candidate.Status != "running" && !candidate.ExpiresAt.After(now) {
			delete(s.restoreJobs, id)
		}
	}
	s.restoreJobs[job.ID] = job
	s.restoreJobMu.Unlock()
}

func (s *Server) updateRestoreJob(job *restoreJob, update func(*restoreJob)) {
	s.restoreJobMu.Lock()
	if s.restoreJobs[job.ID] == job {
		update(job)
	}
	s.restoreJobMu.Unlock()
}

func (s *Server) restoreJobResult(job *restoreJob) restoreJobResponse {
	s.restoreJobMu.Lock()
	defer s.restoreJobMu.Unlock()
	return restoreJobResponseFrom(job)
}

func restoreJobResponseFrom(job *restoreJob) restoreJobResponse {
	return restoreJobResponse{
		ID: job.ID, Status: job.Status, Stage: job.Stage, Message: job.Message,
		Current: job.Current, Total: job.Total, Result: job.Result, Error: job.Error, ExpiresAt: job.ExpiresAt,
	}
}

func validateDatabaseObjects(ctx context.Context, database *persistence.SQLite, inspection store.PortableStateInspection, report ...func(string, string, int, int)) error {
	progress := func(string, string, int, int) {}
	if len(report) > 0 && report[0] != nil {
		progress = report[0]
	}
	attachments := database.Files("attachment:", "job-attachment", maxJobAttachmentBytes)
	for index, attachment := range inspection.Attachments {
		data, err := attachments.ReadFile(attachment.ID)
		if err != nil {
			return fmt.Errorf("backup is missing attachment %s: %w", attachment.Name, err)
		}
		if err := verifyAttachmentData(attachment, data); err != nil {
			return err
		}
		progress("attachments", fmt.Sprintf("Bijlage %s gecontroleerd", attachment.Name), index+1, len(inspection.Attachments))
	}
	restorable := restorableArtifacts(inspection.Artifacts)
	for index, artifact := range restorable {
		if err := database.RestoreSnapshot(ctx, artifact.Snapshot, io.Discard); err != nil {
			return fmt.Errorf("backup Docker snapshot %s:%s (%s) is missing or corrupt: %w", artifact.Kind, artifact.Name, artifact.Snapshot.Digest, err)
		}
		progress("snapshots", fmt.Sprintf("Docker-laag %s:%s gecontroleerd", artifact.Kind, artifact.Name), index+1, len(restorable))
	}
	return nil
}

func restorableArtifacts(artifacts []domain.Artifact) []domain.Artifact {
	result := make([]domain.Artifact, 0, len(artifacts))
	for _, artifact := range artifacts {
		if artifact.Snapshot.Restorable {
			result = append(result, artifact)
		}
	}
	return result
}

func restorableSnapshotCount(artifacts []domain.Artifact) int {
	return len(restorableArtifacts(artifacts))
}

func (s *Server) requireBackupAdmin(w http.ResponseWriter, r *http.Request) bool {
	if s.authDisabled {
		return true
	}
	identity, ok := identityFromRequest(r)
	if !ok || identity.User.Role != domain.UserAdmin {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "admin role required"})
		return false
	}
	return true
}

func (s *Server) hasInteractiveActivity() bool {
	s.terminalMu.Lock()
	terminals := len(s.terminals)
	s.terminalMu.Unlock()
	s.acpMu.Lock()
	chats := len(s.acpSessions)
	s.acpMu.Unlock()
	s.jobLaunchMu.Lock()
	launches := len(s.jobLaunching)
	s.jobLaunchMu.Unlock()
	return terminals > 0 || chats > 0 || launches > 0
}

// The replica keeps generations for a while: points in time the database
// can be put back to, through the same validated restore as a backup zip.

type replicaRestorer interface {
	Points(ctx context.Context) ([]replica.Point, error)
	Fetch(ctx context.Context, generation string, at time.Time, destination string) error
}

func (s *Server) listReplicaPoints(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	restorer, ok := s.replica.(replicaRestorer)
	if !ok {
		writeJSON(w, http.StatusOK, map[string]any{"points": []replica.Point{}})
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 60*time.Second)
	defer cancel()
	points, err := restorer.Points(ctx)
	if err != nil {
		writeError(w, err)
		return
	}
	if points == nil {
		points = []replica.Point{}
	}
	writeJSON(w, http.StatusOK, map[string]any{"points": points})
}

// restoreReplicaPoint fetches the database as it was at a point, next to
// the live one, and runs the restore job on it.
func (s *Server) restoreReplicaPoint(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	restorer, ok := s.replica.(replicaRestorer)
	if !ok || s.database == nil {
		writeError(w, fmt.Errorf("this Spin has no replica to restore from: %w", store.ErrConflict))
		return
	}
	var request struct {
		Generation string    `json:"generation"`
		At         time.Time `json:"at"`
	}
	if err := json.NewDecoder(r.Body).Decode(&request); err != nil || strings.TrimSpace(request.Generation) == "" {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "generation is required"})
		return
	}
	if s.hasInteractiveActivity() {
		writeJSON(w, http.StatusConflict, map[string]string{"error": "stop active terminals, chats and background Job launches before restoring"})
		return
	}
	jobID, err := randomOAuthValue(24)
	if err != nil {
		writeError(w, err)
		return
	}
	if !s.backupMu.TryLock() {
		writeJSON(w, http.StatusConflict, map[string]string{"error": "another backup or restore is already running"})
		return
	}
	job := &restoreJob{
		ID: jobID, Status: "running", Stage: "download", Message: "Herstelpunt uit de replica halen",
		ExpiresAt: time.Now().Add(restoreJobResultLifetime),
	}
	s.storeRestoreJob(job)
	initial := s.restoreJobResult(job)
	generation, at, masterKey := strings.TrimSpace(request.Generation), request.At, s.store.PortableMasterKey()
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Hour)
		defer cancel()
		destination := s.database.TemporaryPath("generation")
		fail := func(err error) {
			s.backupMu.Unlock()
			s.updateRestoreJob(job, func(candidate *restoreJob) {
				candidate.Status, candidate.Error = "error", err.Error()
				candidate.ExpiresAt = time.Now().Add(restoreJobResultLifetime)
			})
			s.logger.Warn("restore replica generation", "generation", generation, "error", err)
		}
		if err := restorer.Fetch(ctx, generation, at, destination); err != nil {
			fail(err)
			return
		}
		staged, err := s.database.StageDatabaseFile(destination, masterKey)
		if err != nil {
			fail(err)
			return
		}
		s.runRestoreJob(job, staged)
	}()
	w.Header().Set("Location", "/api/restores/"+job.ID)
	w.Header().Set("Retry-After", "1")
	writeJSON(w, http.StatusAccepted, initial)
}
