package server

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
)

// A manifest's listing can run to tens of thousands of paths; it lives as
// a file next to the state, keyed by the layer or the composition, and the
// state keeps the summary.

func (s *Server) manifestFiles() *persistence.FileStore {
	if s.database == nil {
		return nil
	}
	return s.database.Files("manifest:", "layer-manifest", 64<<20)
}

// detachManifest moves the listing out of the contents into the file store.
func (s *Server) detachManifest(key string, contents *domain.LayerContents) {
	if contents == nil || len(contents.Entries) == 0 {
		return
	}
	entries := contents.Entries
	contents.Entries = nil
	files := s.manifestFiles()
	if files == nil {
		return
	}
	data, err := json.Marshal(entries)
	if err != nil {
		return
	}
	if err := files.WriteFile(key, data); err != nil {
		s.logger.Warn("keep manifest", "key", key, "error", err)
	}
}

func (s *Server) manifestEntries(key string) []domain.ContentEntry {
	files := s.manifestFiles()
	if files == nil {
		return nil
	}
	data, err := files.ReadFile(key)
	if err != nil {
		return nil
	}
	var entries []domain.ContentEntry
	_ = json.Unmarshal(data, &entries)
	return entries
}

type manifestResponse struct {
	Contents *domain.LayerContents `json:"contents"`
	Entries  []domain.ContentEntry `json:"entries"`
}

// artifactContentsHandler shows what a layer holds.
func (s *Server) artifactContentsHandler(w http.ResponseWriter, r *http.Request) {
	artifact, err := s.store.Artifact(r.PathValue("artifactID"))
	if err != nil {
		writeError(w, err)
		return
	}
	entries := s.manifestEntries("artifact:" + artifact.ID)
	if entries == nil {
		entries = []domain.ContentEntry{}
	}
	writeJSON(w, http.StatusOK, manifestResponse{Contents: artifact.Snapshot.Contents, Entries: entries})
}

// compositionChangesHandler shows what a capsule changed outside its workspace.
func (s *Server) compositionChangesHandler(w http.ResponseWriter, r *http.Request) {
	composition, err := s.store.Composition(r.PathValue("compositionID"))
	if err != nil {
		writeError(w, err)
		return
	}
	entries := s.manifestEntries("composition:" + composition.ID)
	if entries == nil {
		entries = []domain.ContentEntry{}
	}
	writeJSON(w, http.StatusOK, manifestResponse{Contents: composition.CapsuleChanges, Entries: entries})
}

// Layers sealed before manifests existed get theirs read out of the image:
// on request, and in the background for every layer that still lacks one.

type artifactLayerInspector interface {
	InspectLayer(ctx context.Context, artifact domain.Artifact) (domain.LayerContents, error)
}

func (s *Server) inspectLayer(ctx context.Context, artifact domain.Artifact) (domain.LayerContents, error) {
	switch engine := s.engine.(type) {
	case artifactLayerInspector:
		return engine.InspectLayer(ctx, artifact)
	case capsule.LayerInspector:
		return engine.InspectLayer(ctx, artifact.Snapshot)
	}
	return domain.LayerContents{}, fmt.Errorf("capsule engine %s cannot inspect layers: %w", s.engine.Info().Driver, store.ErrConflict)
}

// recordLayerContents reads and keeps the manifest of one layer.
func (s *Server) recordLayerContents(ctx context.Context, artifact domain.Artifact) (domain.Artifact, error) {
	contents, err := s.inspectLayer(ctx, artifact)
	if err != nil {
		return domain.Artifact{}, err
	}
	s.detachManifest("artifact:"+artifact.ID, &contents)
	return s.store.SetArtifactContents(artifact.ID, contents)
}

func (s *Server) inspectArtifactContentsHandler(w http.ResponseWriter, r *http.Request) {
	artifact, err := s.store.Artifact(r.PathValue("artifactID"))
	if err != nil {
		writeError(w, err)
		return
	}
	if !artifact.Snapshot.Restorable {
		writeError(w, fmt.Errorf("layer has no restorable image: %w", store.ErrConflict))
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 10*time.Minute)
	defer cancel()
	updated, err := s.recordLayerContents(ctx, artifact)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, updated)
}

// backfillContents gives every current layer without a manifest one, one
// layer at a time, whenever a runner comes up.
func (s *Server) backfillContents() {
	if !s.backfillMu.TryLock() {
		return
	}
	defer s.backfillMu.Unlock()
	for _, artifact := range s.store.Snapshot().Artifacts {
		if !artifact.Snapshot.Restorable || artifact.Snapshot.Contents != nil || artifact.SnapshotPrunedAt != nil || artifact.SupersededBy != "" {
			continue
		}
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
		_, err := s.recordLayerContents(ctx, artifact)
		cancel()
		if err != nil {
			s.logger.Warn("read layer manifest", "artifact", artifact.ID, "error", err)
			continue
		}
		s.logger.Info("layer manifest read", "artifact", artifact.ID, "kind", artifact.Kind, "name", artifact.Name)
	}
}
