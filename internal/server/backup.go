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
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
)

const maxBackupUploadBytes int64 = 64 << 30

// Lean intentionally accepts at most 1 MiB per HTTP request. Keep restore
// chunks at that exact transport boundary; metadata travels in headers.
const restoreUploadChunkBytes int64 = 1 << 20

// restoreUploadParallelism is how many chunks a browser keeps in flight. Every
// chunk costs one round trip through Cloudflare and the HopOS tunnel (about
// 0.1 s), so sequential 1 MiB chunks cap out near 10 MB/s. The tunnel app
// buffers each request body inside a 32 MB slot, which keeps this modest.
const restoreUploadParallelism = 4

const restoreUploadLifetime = 6 * time.Hour

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

type restoreUpload struct {
	ID        string
	UserID    string
	Name      string
	Size      int64
	ExpiresAt time.Time
	File      *persistence.BackupUpload
}

type restoreUploadResponse struct {
	ID        string    `json:"id"`
	Name      string    `json:"name"`
	Size      int64     `json:"size"`
	Offset    int64     `json:"offset"`
	ChunkSize int64     `json:"chunk_size"`
	Parallel  int       `json:"parallel"`
	ExpiresAt time.Time `json:"expires_at"`
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

func (s *Server) downloadBackup(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	s.writeBackup(w, r)
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
	s.writeBackup(w, r)
}

func (s *Server) writeBackup(w http.ResponseWriter, r *http.Request) {
	if s.database == nil {
		writeError(w, fmt.Errorf("SQLite backup is not configured: %w", store.ErrConflict))
		return
	}
	s.backupMu.Lock()
	defer s.backupMu.Unlock()

	portable, err := s.store.ExportPortableState()
	if err != nil {
		writeError(w, err)
		return
	}
	backup, err := s.database.PrepareBackup(r.Context(), portable.MasterKey)
	if err != nil {
		writeError(w, err)
		return
	}
	defer backup.Close()
	stateJSON, err := backup.Database.ReadFile("state")
	if err != nil {
		writeError(w, fmt.Errorf("backup copy contains no state: %w", err))
		return
	}
	inspection, err := s.store.InspectPortableState(stateJSON, portable.MasterKey)
	if err != nil {
		writeError(w, err)
		return
	}
	if err := validateDatabaseObjects(r.Context(), backup.Database, inspection); err != nil {
		writeError(w, err)
		return
	}
	filename := "spin-backup-" + time.Now().UTC().Format("20060102-150405Z") + ".db"
	w.Header().Set("Content-Type", "application/vnd.sqlite3")
	w.Header().Set("Content-Disposition", `attachment; filename="`+filename+`"`)
	w.Header().Set("X-Spin-Backup-Contains-Secrets", "true")
	if err := backup.WriteTo(r.Context(), w); err != nil {
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

func (s *Server) createRestoreUpload(w http.ResponseWriter, r *http.Request) {
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
	var request struct {
		Name string `json:"name"`
		Size int64  `json:"size"`
	}
	decoder := json.NewDecoder(http.MaxBytesReader(w, r.Body, 16<<10))
	if err := decoder.Decode(&request); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "invalid restore upload: " + err.Error()})
		return
	}
	request.Name = strings.TrimSpace(request.Name)
	if request.Name == "" || len(request.Name) > 255 {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "backup filename must contain 1 to 255 characters"})
		return
	}
	if request.Size <= 0 || request.Size > maxBackupUploadBytes {
		writeJSON(w, http.StatusRequestEntityTooLarge, map[string]string{"error": "backup size must be between 1 byte and 64 GiB"})
		return
	}
	file, err := s.database.BeginBackupUpload(maxBackupUploadBytes)
	if err != nil {
		writeError(w, err)
		return
	}
	id, err := randomOAuthValue(24)
	if err != nil {
		_ = file.Close()
		writeError(w, err)
		return
	}
	identity, authenticated := identityFromRequest(r)
	upload := &restoreUpload{ID: id, Name: request.Name, Size: request.Size, ExpiresAt: time.Now().Add(restoreUploadLifetime), File: file}
	if authenticated {
		upload.UserID = identity.User.ID
	}
	expired := s.storeRestoreUpload(upload)
	for _, candidate := range expired {
		_ = candidate.File.Close()
	}
	writeJSON(w, http.StatusCreated, s.restoreUploadResult(upload))
}

func (s *Server) getRestoreUpload(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	upload, ok := s.restoreUploadForRequest(w, r)
	if !ok {
		return
	}
	writeJSON(w, http.StatusOK, s.restoreUploadResult(upload))
}

func (s *Server) appendRestoreUpload(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	upload, ok := s.restoreUploadForRequest(w, r)
	if !ok {
		return
	}
	if r.ContentLength <= 0 || r.ContentLength > restoreUploadChunkBytes {
		writeJSON(w, http.StatusRequestEntityTooLarge, map[string]any{"error": "restore chunks must contain at most 1 MiB", "offset": upload.File.Offset()})
		return
	}
	offset, err := strconv.ParseInt(strings.TrimSpace(r.Header.Get("X-Spin-Upload-Offset")), 10, 64)
	if err != nil || offset < 0 {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "X-Spin-Upload-Offset must be a non-negative integer"})
		return
	}
	if offset+r.ContentLength > upload.Size {
		writeJSON(w, http.StatusBadRequest, map[string]any{"error": "restore chunk exceeds the declared backup size", "offset": upload.File.Offset()})
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, restoreUploadChunkBytes)
	next, err := upload.File.WriteAt(r.Context(), offset, r.ContentLength, r.Body)
	if err != nil {
		status := http.StatusBadRequest
		if errors.Is(err, persistence.ErrBackupUploadOffset) {
			status = http.StatusConflict
		}
		writeJSON(w, status, map[string]any{"error": err.Error(), "offset": next, "size": upload.Size})
		return
	}
	s.touchRestoreUpload(upload)
	writeJSON(w, http.StatusOK, s.restoreUploadResult(upload))
}

func (s *Server) deleteRestoreUpload(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	upload, ok := s.restoreUploadForRequest(w, r)
	if !ok {
		return
	}
	s.removeRestoreUpload(upload)
	if err := upload.File.Close(); err != nil {
		writeError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *Server) completeRestoreUpload(w http.ResponseWriter, r *http.Request) {
	if !s.requireBackupAdmin(w, r) {
		return
	}
	upload, ok := s.restoreUploadForRequest(w, r)
	if !ok {
		return
	}
	if upload.File.Offset() != upload.Size {
		writeJSON(w, http.StatusConflict, map[string]any{"error": "restore upload is incomplete", "offset": upload.File.Offset(), "size": upload.Size})
		return
	}
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
	s.removeRestoreUpload(upload)
	staged, err := upload.File.Stage(upload.Size)
	if err != nil {
		s.backupMu.Unlock()
		_ = upload.File.Close()
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

func (s *Server) restoreUploadResult(upload *restoreUpload) restoreUploadResponse {
	s.restoreUploadMu.Lock()
	expiresAt := upload.ExpiresAt
	s.restoreUploadMu.Unlock()
	return restoreUploadResponse{
		ID: upload.ID, Name: upload.Name, Size: upload.Size, Offset: upload.File.Offset(),
		ChunkSize: restoreUploadChunkBytes, Parallel: restoreUploadParallelism, ExpiresAt: expiresAt,
	}
}

func (s *Server) storeRestoreUpload(upload *restoreUpload) []*restoreUpload {
	now := time.Now()
	var expired []*restoreUpload
	s.restoreUploadMu.Lock()
	for id, candidate := range s.restoreUploads {
		if !candidate.ExpiresAt.After(now) {
			delete(s.restoreUploads, id)
			expired = append(expired, candidate)
		}
	}
	s.restoreUploads[upload.ID] = upload
	s.restoreUploadMu.Unlock()
	return expired
}

func (s *Server) restoreUploadForRequest(w http.ResponseWriter, r *http.Request) (*restoreUpload, bool) {
	id := strings.TrimSpace(r.PathValue("uploadID"))
	identity, authenticated := identityFromRequest(r)
	now := time.Now()
	s.restoreUploadMu.Lock()
	upload, ok := s.restoreUploads[id]
	if ok && !upload.ExpiresAt.After(now) {
		delete(s.restoreUploads, id)
		ok = false
	}
	if ok && authenticated && upload.UserID != identity.User.ID {
		ok = false
	}
	if ok {
		upload.ExpiresAt = now.Add(restoreUploadLifetime)
	}
	s.restoreUploadMu.Unlock()
	if !ok {
		if upload != nil && !upload.ExpiresAt.After(now) {
			_ = upload.File.Close()
		}
		writeJSON(w, http.StatusNotFound, map[string]string{"error": "restore upload not found or expired"})
		return nil, false
	}
	return upload, true
}

func (s *Server) touchRestoreUpload(upload *restoreUpload) {
	s.restoreUploadMu.Lock()
	if s.restoreUploads[upload.ID] == upload {
		upload.ExpiresAt = time.Now().Add(restoreUploadLifetime)
	}
	s.restoreUploadMu.Unlock()
}

func (s *Server) removeRestoreUpload(upload *restoreUpload) {
	s.restoreUploadMu.Lock()
	if s.restoreUploads[upload.ID] == upload {
		delete(s.restoreUploads, upload.ID)
	}
	s.restoreUploadMu.Unlock()
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
