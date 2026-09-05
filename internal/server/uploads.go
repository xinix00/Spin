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

// One chunked upload carries everything large that crosses Spin's HTTP
// boundary. Lean accepts at most 1 MiB per request, and one long-lived stream
// starves the control plane while it writes, so a browser restoring a backup
// and a runner archiving a snapshot both send short, acknowledged chunks here
// and let persistence assemble them. A lost connection resumes at the
// committed offset instead of starting over.

// uploadChunkBytes is the exact transport boundary; metadata travels in headers.
const uploadChunkBytes int64 = 1 << 20

// uploadParallelism is how many chunks a client keeps in flight. Every chunk
// costs one round trip through the edge and the tunnel (about 0.1 s), so
// sequential 1 MiB chunks cap out near 10 MB/s. The tunnel app buffers each
// request body inside a 32 MB slot, which keeps this modest.
const uploadParallelism = 4

const uploadLifetime = 6 * time.Hour

const maxSnapshotUploadBytes int64 = 64 << 30

type uploadTarget interface {
	WriteAt(ctx context.Context, offset, length int64, source io.Reader) (int64, error)
	Offset() int64
	Close() error
}

type chunkedUpload struct {
	ID        string
	Kind      string
	Name      string
	Size      int64
	ExpiresAt time.Time
	UserID    string // browser owner; empty for a runner upload
	Runner    bool   // created with the worker token and continued only with it
	Target    uploadTarget
	Backup    *persistence.BackupUpload
	Snapshot  *persistence.SnapshotUpload
	Digest    string // snapshot digest, so a seal in progress can show its upload
}

type uploadResponse struct {
	ID        string    `json:"id"`
	Kind      string    `json:"kind"`
	Name      string    `json:"name"`
	Size      int64     `json:"size"`
	Offset    int64     `json:"offset"`
	ChunkSize int64     `json:"chunk_size"`
	Parallel  int       `json:"parallel"`
	ExpiresAt time.Time `json:"expires_at"`
}

type createUploadRequest struct {
	Kind     string                 `json:"kind"`
	Name     string                 `json:"name"`
	Size     int64                  `json:"size"`
	Snapshot domain.CapsuleSnapshot `json:"snapshot"`
}

// workerRequest reports whether the caller is a runner. The middleware marks
// that on the context; with authentication disabled it never runs, so a valid
// worker bearer is honoured directly.
func (s *Server) workerRequest(r *http.Request) bool {
	if value, _ := r.Context().Value(workerContextKey{}).(bool); value {
		return true
	}
	return s.authDisabled && s.validWorkerBearer(r.Header.Get("Authorization"))
}

func (s *Server) createUpload(w http.ResponseWriter, r *http.Request) {
	if s.database == nil {
		writeError(w, fmt.Errorf("SQLite storage is not configured: %w", store.ErrConflict))
		return
	}
	var request createUploadRequest
	decoder := json.NewDecoder(http.MaxBytesReader(w, r.Body, 16<<10))
	if err := decoder.Decode(&request); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "invalid upload: " + err.Error()})
		return
	}
	request.Kind = strings.ToLower(strings.TrimSpace(request.Kind))
	request.Name = strings.TrimSpace(request.Name)
	upload := &chunkedUpload{Kind: request.Kind, Name: request.Name, Size: request.Size, ExpiresAt: time.Now().Add(uploadLifetime)}
	switch request.Kind {
	case "restore":
		if !s.requireBackupAdmin(w, r) {
			return
		}
		if s.hasInteractiveActivity() {
			writeJSON(w, http.StatusConflict, map[string]string{"error": "stop active terminals, chats and background Job launches before restoring a backup"})
			return
		}
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
		upload.Backup, upload.Target = file, file
		if identity, authenticated := identityFromRequest(r); authenticated {
			upload.UserID = identity.User.ID
		}
	case "snapshot":
		if !s.workerRequest(r) {
			writeJSON(w, http.StatusForbidden, map[string]string{"error": "snapshot uploads are sent by runners with the worker token"})
			return
		}
		if request.Size <= 0 || request.Size > maxSnapshotUploadBytes {
			writeJSON(w, http.StatusRequestEntityTooLarge, map[string]string{"error": "snapshot size must be between 1 byte and 64 GiB"})
			return
		}
		if strings.TrimSpace(request.Snapshot.Digest) == "" {
			writeJSON(w, http.StatusBadRequest, map[string]string{"error": "snapshot digest is required"})
			return
		}
		if upload.Name == "" {
			upload.Name = request.Snapshot.Ref
		}
		archive, err := s.database.BeginSnapshotUpload(r.Context(), request.Snapshot, request.Size)
		if err != nil {
			writeError(w, err)
			return
		}
		upload.Snapshot, upload.Target, upload.Runner = archive, archive, true
		upload.Digest = strings.TrimSpace(request.Snapshot.Digest)
	default:
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "upload kind must be restore or snapshot"})
		return
	}
	id, err := randomOAuthValue(24)
	if err != nil {
		_ = upload.Target.Close()
		writeError(w, err)
		return
	}
	upload.ID = id
	for _, expired := range s.storeUpload(upload) {
		_ = expired.Target.Close()
	}
	writeJSON(w, http.StatusCreated, s.uploadResult(upload))
}

func (s *Server) getUpload(w http.ResponseWriter, r *http.Request) {
	upload, ok := s.uploadForRequest(w, r)
	if !ok {
		return
	}
	writeJSON(w, http.StatusOK, s.uploadResult(upload))
}

func (s *Server) appendUpload(w http.ResponseWriter, r *http.Request) {
	upload, ok := s.uploadForRequest(w, r)
	if !ok {
		return
	}
	if r.ContentLength <= 0 || r.ContentLength > uploadChunkBytes {
		writeJSON(w, http.StatusRequestEntityTooLarge, map[string]any{"error": "upload chunks must contain at most 1 MiB", "offset": upload.Target.Offset()})
		return
	}
	offset, err := strconv.ParseInt(strings.TrimSpace(r.Header.Get("X-Spin-Upload-Offset")), 10, 64)
	if err != nil || offset < 0 {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "X-Spin-Upload-Offset must be a non-negative integer"})
		return
	}
	if offset+r.ContentLength > upload.Size {
		writeJSON(w, http.StatusBadRequest, map[string]any{"error": "chunk exceeds the declared upload size", "offset": upload.Target.Offset()})
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, uploadChunkBytes)
	next, err := upload.Target.WriteAt(r.Context(), offset, r.ContentLength, r.Body)
	if err != nil {
		status := http.StatusBadRequest
		if errors.Is(err, persistence.ErrUploadOffset) {
			status = http.StatusConflict
		}
		writeJSON(w, status, map[string]any{"error": err.Error(), "offset": next, "size": upload.Size})
		return
	}
	s.touchUpload(upload)
	writeJSON(w, http.StatusOK, s.uploadResult(upload))
}

func (s *Server) deleteUpload(w http.ResponseWriter, r *http.Request) {
	upload, ok := s.uploadForRequest(w, r)
	if !ok {
		return
	}
	s.removeUpload(upload)
	if err := upload.Target.Close(); err != nil {
		writeError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *Server) completeUpload(w http.ResponseWriter, r *http.Request) {
	upload, ok := s.uploadForRequest(w, r)
	if !ok {
		return
	}
	if upload.Target.Offset() != upload.Size {
		writeJSON(w, http.StatusConflict, map[string]any{"error": "upload is incomplete", "offset": upload.Target.Offset(), "size": upload.Size})
		return
	}
	switch upload.Kind {
	case "restore":
		s.completeRestoreUpload(w, r, upload)
	case "snapshot":
		info, err := upload.Snapshot.Complete(r.Context())
		if err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]any{"error": err.Error(), "offset": upload.Target.Offset(), "size": upload.Size})
			return
		}
		s.removeUpload(upload)
		writeJSON(w, http.StatusOK, map[string]any{"ref": info.Ref, "digest": info.Digest, "size": info.Size})
	default:
		writeError(w, fmt.Errorf("upload kind %q cannot be completed: %w", upload.Kind, store.ErrConflict))
	}
}

func (s *Server) uploadResult(upload *chunkedUpload) uploadResponse {
	s.uploadMu.Lock()
	expiresAt := upload.ExpiresAt
	s.uploadMu.Unlock()
	return uploadResponse{
		ID: upload.ID, Kind: upload.Kind, Name: upload.Name, Size: upload.Size, Offset: upload.Target.Offset(),
		ChunkSize: uploadChunkBytes, Parallel: uploadParallelism, ExpiresAt: expiresAt,
	}
}

func (s *Server) storeUpload(upload *chunkedUpload) []*chunkedUpload {
	now := time.Now()
	var expired []*chunkedUpload
	s.uploadMu.Lock()
	for id, candidate := range s.uploads {
		if !candidate.ExpiresAt.After(now) {
			delete(s.uploads, id)
			expired = append(expired, candidate)
		}
	}
	s.uploads[upload.ID] = upload
	s.uploadMu.Unlock()
	return expired
}

// uploadForRequest finds the upload and checks that the caller may continue
// it: a runner upload only with the worker token, a browser upload only by the
// backup admin who started it.
func (s *Server) uploadForRequest(w http.ResponseWriter, r *http.Request) (*chunkedUpload, bool) {
	id := strings.TrimSpace(r.PathValue("uploadID"))
	worker := s.workerRequest(r)
	now := time.Now()
	s.uploadMu.Lock()
	upload, ok := s.uploads[id]
	if ok && !upload.ExpiresAt.After(now) {
		delete(s.uploads, id)
		ok = false
	}
	if ok && upload.Runner != worker {
		ok = false
	}
	if ok {
		upload.ExpiresAt = now.Add(uploadLifetime)
	}
	s.uploadMu.Unlock()
	if !ok {
		if upload != nil && !upload.ExpiresAt.After(now) {
			_ = upload.Target.Close()
		}
		writeJSON(w, http.StatusNotFound, map[string]string{"error": "upload not found or expired"})
		return nil, false
	}
	if !worker {
		if !s.requireBackupAdmin(w, r) {
			return nil, false
		}
		if identity, authenticated := identityFromRequest(r); authenticated && upload.UserID != identity.User.ID {
			writeJSON(w, http.StatusNotFound, map[string]string{"error": "upload not found or expired"})
			return nil, false
		}
	}
	return upload, true
}

func (s *Server) touchUpload(upload *chunkedUpload) {
	s.uploadMu.Lock()
	if s.uploads[upload.ID] == upload {
		upload.ExpiresAt = time.Now().Add(uploadLifetime)
	}
	s.uploadMu.Unlock()
}

func (s *Server) removeUpload(upload *chunkedUpload) {
	s.uploadMu.Lock()
	if s.uploads[upload.ID] == upload {
		delete(s.uploads, upload.ID)
	}
	s.uploadMu.Unlock()
}

// uploadProgressForDigest reports how far the runner's archive upload for a
// snapshot has come, for a seal that wants to show progress.
func (s *Server) uploadProgressForDigest(digest string) (int64, int64, bool) {
	s.uploadMu.Lock()
	defer s.uploadMu.Unlock()
	for _, upload := range s.uploads {
		if upload.Kind == "snapshot" && upload.Digest == digest {
			return upload.Target.Offset(), upload.Size, true
		}
	}
	return 0, 0, false
}
