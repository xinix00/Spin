package server

import (
	"context"
	"net/http"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
)

// Storage on the server is finite and Spin cannot ask the volume how much is
// left. What it can do is not waste it: an EDIT makes the old version of a
// layer superseded, and once nothing running uses that version its archived
// snapshot goes. The numbers below are shown so a person sees the database
// grow before the volume is full.

// storageUsage is what the archive can report about itself.
type storageUsage interface {
	Usage(context.Context) (persistence.StorageUsage, error)
}

// storageInfo is the storage line of the state and of /healthz.
type storageInfo struct {
	DatabaseBytes int64  `json:"database_bytes"`
	ObjectBytes   int64  `json:"object_bytes"`
	Objects       int    `json:"objects"`
	Prunable      int    `json:"prunable"`
	Error         string `json:"error,omitempty"`
}

func (s *Server) storageInfo(ctx context.Context) storageInfo {
	info := storageInfo{Prunable: len(s.store.PrunableArtifacts())}
	reporter, ok := s.snapshotArchive.(storageUsage)
	if !ok {
		return info
	}
	usage, err := reporter.Usage(ctx)
	if err != nil {
		info.Error = err.Error()
		return info
	}
	info.DatabaseBytes, info.ObjectBytes, info.Objects = usage.DatabaseBytes, usage.ObjectBytes, usage.Objects
	return info
}

// pruneSupersededSnapshots removes the archived snapshots of superseded
// versions that nothing uses any more. It runs at startup and after every
// saved layer, and it is idempotent.
func (s *Server) pruneSupersededSnapshots(ctx context.Context) int {
	if s.snapshotArchive == nil {
		return 0
	}
	pruned := 0
	for _, artifact := range s.store.PrunableArtifacts() {
		if err := s.snapshotArchive.RemoveArchivedSnapshot(ctx, artifact.Snapshot); err != nil {
			s.logger.Warn("prune superseded snapshot", "artifact", artifact.ID, "error", err)
			continue
		}
		if _, err := s.store.MarkSnapshotPruned(artifact.ID); err != nil {
			s.logger.Warn("mark snapshot pruned", "artifact", artifact.ID, "error", err)
			continue
		}
		pruned++
		s.logger.Info("pruned superseded snapshot", "artifact", artifact.ID, "kind", artifact.Kind, "name", artifact.Name, "superseded_by", artifact.SupersededBy)
	}
	return pruned
}

func (s *Server) pruneLater() {
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
		defer cancel()
		s.pruneSupersededSnapshots(ctx)
	}()
}

func (s *Server) storageHandler(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, s.storageInfo(r.Context()))
}

// snapshotShippable reports whether an artifact's snapshot should reach a
// runner: a pruned one is contained in its newer version and is never
// applied on its own.
func snapshotShippable(artifact domain.Artifact) bool {
	return artifact.SnapshotPrunedAt == nil
}

// cleanTemporaryFiles removes staged backup and restore copies a previous
// process left behind; each one is as large as the database.
func (s *Server) cleanTemporaryFiles() {
	if s.database == nil {
		return
	}
	removed, err := s.database.CleanTemporaryFiles()
	if err != nil {
		s.logger.Warn("clean temporary database copies", "error", err)
		return
	}
	if removed > 0 {
		s.logger.Info("removed leftover database copies", "count", removed)
	}
}
