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

// setTrackedPathsHandler stores which files of a layer Spin keeps between
// Sessions.
func (s *Server) setTrackedPathsHandler(w http.ResponseWriter, r *http.Request) {
	var request struct {
		Paths []string `json:"paths"`
	}
	if !decodeJSON(w, r, &request) {
		return
	}
	updated, err := s.store.SetArtifactTrackedPaths(r.PathValue("artifactID"), request.Paths)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, updated)
}

// artifactTreeHandler names everything that goes with a layer: every
// version of it and every layer built on any version, other users'
// included, so a confirmation can say so before the removal.
func (s *Server) artifactTreeHandler(w http.ResponseWriter, r *http.Request) {
	artifact, err := s.store.Artifact(r.PathValue("artifactID"))
	if err != nil {
		writeError(w, err)
		return
	}
	type member struct {
		ID      string `json:"id"`
		Layer   string `json:"layer"`
		Subject string `json:"subject,omitempty"`
		Version bool   `json:"version"`
	}
	members := []member{}
	for _, candidate := range s.store.ArtifactTree(artifact.ID) {
		if candidate.ID == artifact.ID {
			continue
		}
		sameLayer := candidate.Kind == artifact.Kind && candidate.Name == artifact.Name && candidate.Subject == artifact.Subject
		members = append(members, member{ID: candidate.ID, Layer: string(candidate.Kind) + ":" + candidate.Name, Subject: candidate.Subject, Version: sameLayer})
	}
	writeJSON(w, http.StatusOK, map[string]any{"members": members})
}
