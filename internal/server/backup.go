package server

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"sync"
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

// A backup is a job, not a request. Staging a copy of a multi-gigabyte
// database takes minutes, which no proxy waits for, so POST /api/backup
// starts it and answers at once, GET /api/backup/status follows the copy
// and the check, and the download is a separate request for a file that is
// ready, with its size known. One backup at a time; a ready file is kept
// for half an hour.
type backupJob struct {
	mu        sync.Mutex
	Status    string // running, ready, error
	Stage     string // copy, verify, ready
	Message   string
	Current   int64
	Total     int64
	Size      int64
	Filename  string
	Error     string
	StartedAt time.Time
	ReadyAt   time.Time
	ExpiresAt time.Time
	staged    *persistence.StagedBackup
	cancel    context.CancelFunc
}

type backupJobResponse struct {
	Status    string     `json:"status"`
	Stage     string     `json:"stage,omitempty"`
	Message   string     `json:"message,omitempty"`
	Current   int64      `json:"current,omitempty"`
	Total     int64      `json:"total,omitempty"`
	Size      int64      `json:"size,omitempty"`
	Filename  string     `json:"filename,omitempty"`
	Error     string     `json:"error,omitempty"`
	StartedAt *time.Time `json:"started_at,omitempty"`
	ReadyAt   *time.Time `json:"ready_at,omitempty"`
	ExpiresAt *time.Time `json:"expires_at,omitempty"`
}

const backupKeepReady = 30 * time.Minute

func (job *backupJob) response() backupJobResponse {
	job.mu.Lock()
	defer job.mu.Unlock()
	response := backupJobResponse{Status: job.Status, Stage: job.Stage, Message: job.Message, Current: job.Current, Total: job.Total, Size: job.Size, Filename: job.Filename, Error: job.Error}
	if !job.StartedAt.IsZero() {
		startedAt := job.StartedAt
		response.StartedAt = &startedAt
	}
	if !job.ReadyAt.IsZero() {
		readyAt, expiresAt := job.ReadyAt, job.ExpiresAt
		response.ReadyAt, response.ExpiresAt = &readyAt, &expiresAt
	}
	return response
}

func (job *backupJob) update(apply func(*backupJob)) {
	job.mu.Lock()
	apply(job)
	job.mu.Unlock()
}

// currentBackup returns the running or ready job, dropping one that expired.
func (s *Server) currentBackup() *backupJob {
	s.backupMu.Lock()
	defer s.backupMu.Unlock()
	job := s.backupJob
	if job == nil {
		return nil
	}
	job.mu.Lock()
	expired := job.Status != "running" && !job.ExpiresAt.After(time.Now())
	staged := job.staged
	job.mu.Unlock()
	if expired {
		if staged != nil {
			_ = staged.Close()
		}
		s.backupJob = nil
		return nil
	}
	return job
}

// startBackup begins staging a backup, or hands back the one under way.
func (s *Server) startBackup() (*backupJob, error) {
	if s.database == nil {
		return nil, fmt.Errorf("SQLite backup is not configured: %w", store.ErrConflict)
	}
	if job := s.currentBackup(); job != nil {
		job.mu.Lock()
		running := job.Status == "running"
		job.mu.Unlock()
		if running {
			return job, nil
		}
		if job.staged != nil {
			_ = job.staged.Close()
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Hour)
	job := &backupJob{Status: "running", Stage: "copy", Message: "Database kopiëren", StartedAt: time.Now().UTC(), cancel: cancel}
	s.backupMu.Lock()
	s.backupJob = job
	s.backupMu.Unlock()
	go s.runBackup(ctx, job)
	return job, nil
}

func (s *Server) runBackup(ctx context.Context, job *backupJob) {
	defer job.cancel()
	fail := func(err error) {
		s.logger.Warn("backup", "error", err)
		job.update(func(job *backupJob) {
			job.Status, job.Error, job.Message = "error", err.Error(), "Backup mislukt"
			job.ExpiresAt = time.Now().Add(backupKeepReady)
		})
	}
	portable, err := s.store.ExportPortableState()
	if err != nil {
		fail(err)
		return
	}
	staged, err := s.database.PrepareBackupProgress(ctx, portable.MasterKey, func(copied, total int) {
		job.update(func(job *backupJob) { job.Current, job.Total = int64(copied), int64(total) })
	})
	if err != nil {
		fail(err)
		return
	}
	job.update(func(job *backupJob) {
		job.Stage, job.Message, job.Current, job.Total = "verify", "Kopie controleren", 0, 0
	})
	stateJSON, err := staged.Database.ReadFile("state")
	if err != nil {
		_ = staged.Close()
		fail(fmt.Errorf("backup copy contains no state: %w", err))
		return
	}
	inspection, err := s.store.InspectPortableState(stateJSON, portable.MasterKey)
	if err != nil {
		_ = staged.Close()
		fail(err)
		return
	}
	if err := verifyBackupObjects(ctx, staged.Database, inspection, func(_, message string, current, total int) {
		job.update(func(job *backupJob) { job.Message, job.Current, job.Total = message, int64(current), int64(total) })
	}); err != nil {
		_ = staged.Close()
		fail(err)
		return
	}
	size, err := staged.Size(ctx)
	if err != nil {
		_ = staged.Close()
		fail(err)
		return
	}
	now := time.Now().UTC()
	job.update(func(job *backupJob) {
		job.Status, job.Stage, job.Message = "ready", "ready", "Backup staat klaar"
		job.Size, job.Filename = size, "spin-backup-"+now.Format("20060102-150405Z")+".db"
		job.ReadyAt, job.ExpiresAt = now, now.Add(backupKeepReady)
		job.staged = staged
		job.Current, job.Total = 0, 0
	})
}

// verifyBackupObjects checks that every attachment and restorable snapshot
// the state names is present in the copy, by its stored size: the copy is
// SQLite's own, so reading gigabytes back would only prove what the page
// checksums already did.
func verifyBackupObjects(ctx context.Context, database *persistence.SQLite, inspection store.PortableStateInspection, report func(string, string, int, int)) error {
	for index, attachment := range inspection.Attachments {
		info, err := database.BlobInfo(ctx, "attachment:"+attachment.ID)
		if err != nil || info.Size != attachment.Size {
			return fmt.Errorf("backup is missing attachment %s: %w", attachment.Name, errors.Join(err, store.ErrConflict))
		}
		report("attachments", fmt.Sprintf("Bijlage %s gecontroleerd", attachment.Name), index+1, len(inspection.Attachments))
	}
	restorable := restorableArtifacts(inspection.Artifacts)
	for index, artifact := range restorable {
		if artifact.SnapshotPrunedAt != nil {
			continue
		}
		if has, err := database.HasSnapshot(ctx, artifact.Snapshot); err != nil || !has {
			return fmt.Errorf("backup Docker snapshot %s:%s (%s) is missing: %w", artifact.Kind, artifact.Name, artifact.Snapshot.Digest, errors.Join(err, store.ErrConflict))
		}
		report("snapshots", fmt.Sprintf("Docker-laag %s:%s gecontroleerd", artifact.Kind, artifact.Name), index+1, len(restorable))
	}
	return nil
}

func (s *Server) startBackupHandler(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	job, err := s.startBackup()
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusAccepted, job.response())
}

func (s *Server) backupStatusHandler(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	job := s.currentBackup()
	if job == nil {
		writeJSON(w, http.StatusOK, backupJobResponse{Status: "none"})
		return
	}
	writeJSON(w, http.StatusOK, job.response())
}

func (s *Server) createBackupTicket(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	job := s.currentBackup()
	if job == nil || job.response().Status != "ready" {
		writeError(w, fmt.Errorf("no backup is ready to download; start one first: %w", store.ErrConflict))
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

// downloadBackupWithTicket streams the ready file. A ticket is single use
// and short-lived because the URL is what a browser download can carry.
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
	job := s.currentBackup()
	if job == nil {
		writeError(w, fmt.Errorf("the backup expired; start a new one: %w", store.ErrConflict))
		return
	}
	job.mu.Lock()
	staged, size, filename, ready := job.staged, job.Size, job.Filename, job.Status == "ready"
	job.mu.Unlock()
	if !ready || staged == nil {
		writeError(w, fmt.Errorf("no backup is ready to download: %w", store.ErrConflict))
		return
	}
	w.Header().Set("Content-Type", "application/vnd.sqlite3")
	w.Header().Set("Content-Disposition", `attachment; filename="`+filename+`"`)
	w.Header().Set("Content-Length", strconv.FormatInt(size, 10))
	w.Header().Set("X-Spin-Backup-Contains-Secrets", "true")
	if err := staged.WriteTo(r.Context(), w); err != nil {
		s.logger.Warn("stream SQLite backup", "error", err)
	}
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
