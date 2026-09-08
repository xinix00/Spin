package server

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"net/http"
	"strconv"
	"strings"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
)

// A runner pulls an archived snapshot in 1 MiB pieces over plain HTTP, each
// piece its own short request, resumable at any offset. The runner link
// then carries only signalling, and a wobbly line pauses a download
// instead of failing a launch.

type snapshotChunkReader interface {
	ReadSnapshotChunk(context.Context, domain.CapsuleSnapshot, int64) ([]byte, persistence.BlobInfo, error)
}

func (s *Server) snapshotChunkHandler(w http.ResponseWriter, r *http.Request) {
	if !s.workerRequest(r) {
		writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "runner authorization required"})
		return
	}
	reader, ok := s.snapshotArchive.(snapshotChunkReader)
	if !ok && s.database != nil {
		// The database is the archive whenever one is configured.
		reader, ok = s.database, true
	}
	if !ok {
		writeError(w, fmt.Errorf("snapshot archive cannot serve chunks: %w", store.ErrConflict))
		return
	}
	digest := strings.TrimSpace(r.PathValue("digest"))
	offset, err := strconv.ParseInt(strings.TrimSpace(r.URL.Query().Get("offset")), 10, 64)
	if digest == "" || err != nil || offset < 0 {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "digest and a non-negative offset are required"})
		return
	}
	chunk, info, err := reader.ReadSnapshotChunk(r.Context(), domain.CapsuleSnapshot{Digest: digest}, offset)
	if errors.Is(err, fs.ErrNotExist) {
		writeJSON(w, http.StatusNotFound, map[string]string{"error": "snapshot is not in the archive"})
		return
	}
	if err != nil {
		writeError(w, err)
		return
	}
	w.Header().Set("X-Spin-Size", strconv.FormatInt(info.Size, 10))
	w.Header().Set("X-Spin-Digest", info.Digest)
	if len(chunk) == 0 {
		w.WriteHeader(http.StatusRequestedRangeNotSatisfiable)
		return
	}
	w.Header().Set("Content-Type", "application/octet-stream")
	w.Header().Set("Content-Length", strconv.Itoa(len(chunk)))
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(chunk)
}
