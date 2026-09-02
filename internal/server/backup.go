package server

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
)

const maxBackupUploadBytes int64 = 64 << 30

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
	s.backupMu.Lock()
	defer s.backupMu.Unlock()

	r.Body = http.MaxBytesReader(w, r.Body, maxBackupUploadBytes)
	staged, err := s.database.StageBackup(r.Context(), r.Body, maxBackupUploadBytes)
	if err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": err.Error()})
		return
	}
	defer staged.Close()
	stateJSON, err := staged.Database.ReadFile("state")
	if err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "Spin backup contains no state"})
		return
	}
	inspection, err := s.store.InspectPortableState(stateJSON, staged.MasterKey)
	if err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": err.Error()})
		return
	}
	if err := validateDatabaseObjects(r.Context(), staged.Database, inspection); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": err.Error()})
		return
	}

	rollback, err := s.database.RollbackPoint(r.Context())
	if err != nil {
		writeError(w, err)
		return
	}
	defer rollback.Close()
	if err := s.database.RestoreFrom(r.Context(), staged); err != nil {
		writeError(w, err)
		return
	}
	if err := s.store.RestorePortableState(stateJSON, staged.MasterKey); err != nil {
		rollbackErr := s.database.RestoreFrom(context.Background(), rollback)
		writeError(w, errors.Join(err, rollbackErr))
		return
	}
	s.csrfTokens.clear()
	s.workflowMu.Lock()
	s.workflowTokens = map[string]string{}
	s.workflowMu.Unlock()
	writeJSON(w, http.StatusOK, restoreResponse{
		Status: "restored", Users: inspection.Users, Jobs: inspection.Jobs, Templates: inspection.Templates,
		Deliverables: inspection.Deliverables, Attachments: len(inspection.Attachments), Snapshots: restorableSnapshotCount(inspection.Artifacts),
	})
}

func validateDatabaseObjects(ctx context.Context, database *persistence.SQLite, inspection store.PortableStateInspection) error {
	attachments := database.Files("attachment:", "job-attachment", maxJobAttachmentBytes)
	for _, attachment := range inspection.Attachments {
		data, err := attachments.ReadFile(attachment.ID)
		if err != nil {
			return fmt.Errorf("backup is missing attachment %s: %w", attachment.Name, err)
		}
		if err := verifyAttachmentData(attachment, data); err != nil {
			return err
		}
	}
	for _, artifact := range inspection.Artifacts {
		if !artifact.Snapshot.Restorable {
			continue
		}
		if err := database.RestoreSnapshot(ctx, artifact.Snapshot, io.Discard); err != nil {
			return fmt.Errorf("backup Docker snapshot %s:%s (%s) is missing or corrupt: %w", artifact.Kind, artifact.Name, artifact.Snapshot.Digest, err)
		}
	}
	return nil
}

func restorableSnapshotCount(artifacts []domain.Artifact) int {
	count := 0
	for _, artifact := range artifacts {
		if artifact.Snapshot.Restorable {
			count++
		}
	}
	return count
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
