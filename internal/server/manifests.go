package server

import (
	"encoding/json"
	"net/http"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
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
