package server

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"sync"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

// A layer can track files: a login an agent rotates, a config it keeps.
// Chosen by a person in the layer's contents. The server holds the one
// copy, encrypted in the database per layer and user; every capsule gets
// it before its agent starts, and after every turn and at stop the files
// are read back. Only a file the capsule itself changed goes to the
// server (a capsule that still has what it was given never overwrites a
// newer copy), and a change reaches every other running capsule of the
// same layer and user at once, so two Sessions on two runners keep one
// token between them.

type trackedTarget struct {
	key   string
	paths []string
}

// trackedTargets are the layers of a composition that track files, keyed
// per user and layer name across versions.
func (s *Server) trackedTargets(composition domain.Composition) []trackedTarget {
	var targets []trackedTarget
	for _, artifactID := range capsule.CompositionLayers(composition) {
		artifact, err := s.store.Artifact(artifactID)
		if err != nil || len(artifact.TrackedPaths) == 0 {
			continue
		}
		targets = append(targets, trackedTarget{key: artifact.Subject + "/" + string(artifact.Kind) + ":" + artifact.Name, paths: artifact.TrackedPaths})
	}
	return targets
}

// deliveredFiles remembers, per composition and layer key, a hash of every
// tracked file as the capsule last got or gave it.
type deliveredFiles struct {
	mu    sync.Mutex
	files map[string]map[string]map[string]string // composition -> key -> path -> hash
}

func fileHash(data []byte) string {
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}

func (d *deliveredFiles) get(compositionID, key string) map[string]string {
	d.mu.Lock()
	defer d.mu.Unlock()
	copied := map[string]string{}
	for path, hash := range d.files[compositionID][key] {
		copied[path] = hash
	}
	return copied
}

func (d *deliveredFiles) set(compositionID, key string, files map[string][]byte) {
	d.mu.Lock()
	defer d.mu.Unlock()
	if d.files == nil {
		d.files = map[string]map[string]map[string]string{}
	}
	if d.files[compositionID] == nil {
		d.files[compositionID] = map[string]map[string]string{}
	}
	if d.files[compositionID][key] == nil {
		d.files[compositionID][key] = map[string]string{}
	}
	for path, data := range files {
		d.files[compositionID][key][path] = fileHash(data)
	}
}

func (d *deliveredFiles) forget(compositionID string) {
	d.mu.Lock()
	defer d.mu.Unlock()
	delete(d.files, compositionID)
}

// restoreLoginState puts the kept files in the capsule before the agent
// starts, and notes what the capsule holds from then on.
func (s *Server) restoreLoginState(ctx context.Context, composition domain.Composition) {
	tracked, ok := s.engine.(capsule.TrackedFiles)
	if !ok || composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return
	}
	s.loginMu.Lock()
	defer s.loginMu.Unlock()
	for _, target := range s.trackedTargets(composition) {
		if state, ok := s.store.LoginState(target.key); ok && len(state.Files) > 0 {
			if err := tracked.WriteTrackedFiles(ctx, *composition.Runtime, state.Files); err != nil {
				s.logger.Warn("restore tracked files", "composition", composition.ID, "layer", target.key, "error", err)
				continue
			}
			s.logger.Info("tracked files restored", "composition", composition.ID, "layer", target.key, "files", len(state.Files), "kept_at", state.UpdatedAt.Format(time.RFC3339))
		}
		// What the capsule holds now, from the layer or just written, is
		// the baseline a later change is measured against.
		files, err := tracked.ReadTrackedFiles(ctx, *composition.Runtime, target.paths)
		if err != nil {
			s.logger.Warn("read tracked files", "composition", composition.ID, "layer", target.key, "error", err)
			continue
		}
		s.delivered.set(composition.ID, target.key, files)
	}
}

// captureLoginState reads the tracked files back from a running capsule:
// what the capsule changed goes to the server and to every other running
// capsule of the same layer and user; what the server has newer than this
// capsule goes into it. It also notes what else the capsule changed.
func (s *Server) captureLoginState(ctx context.Context, composition domain.Composition) {
	if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return
	}
	if inspector, ok := s.engine.(capsule.CapsuleInspector); ok {
		changes, err := inspector.CaptureCapsuleChanges(ctx, *composition.Runtime)
		if err != nil {
			s.logger.Warn("inspect capsule changes", "composition", composition.ID, "error", err)
		} else {
			s.detachManifest("composition:"+composition.ID, &changes)
			if err := s.store.SetCompositionChanges(composition.ID, changes); err != nil {
				s.logger.Warn("record capsule changes", "composition", composition.ID, "error", err)
			}
		}
	}
	tracked, ok := s.engine.(capsule.TrackedFiles)
	if !ok {
		return
	}
	s.loginMu.Lock()
	defer s.loginMu.Unlock()
	for _, target := range s.trackedTargets(composition) {
		files, err := tracked.ReadTrackedFiles(ctx, *composition.Runtime, target.paths)
		if err != nil {
			s.logger.Warn("read tracked files", "composition", composition.ID, "layer", target.key, "error", err)
			continue
		}
		delivered := s.delivered.get(composition.ID, target.key)
		changed := map[string][]byte{}
		for path, data := range files {
			if delivered[path] != fileHash(data) {
				changed[path] = data
			}
		}
		if len(changed) > 0 {
			if _, err := s.store.SaveLoginStateFiles(target.key, changed); err != nil {
				s.logger.Warn("keep tracked files", "layer", target.key, "error", err)
				continue
			}
			s.delivered.set(composition.ID, target.key, changed)
			s.logger.Info("tracked files kept", "composition", composition.ID, "layer", target.key, "files", len(changed))
			s.shareTrackedFiles(ctx, tracked, composition.ID, target.key, changed)
		}
		// The server may hold a newer copy this capsule missed (a share
		// that failed while its runner was away): bring it in now.
		state, ok := s.store.LoginState(target.key)
		if !ok {
			continue
		}
		behind := map[string][]byte{}
		for path, data := range state.Files {
			if _, justChanged := changed[path]; !justChanged && delivered[path] != fileHash(data) {
				behind[path] = data
			}
		}
		if len(behind) > 0 {
			if err := tracked.WriteTrackedFiles(ctx, *composition.Runtime, behind); err != nil {
				s.logger.Warn("bring tracked files up to date", "composition", composition.ID, "layer", target.key, "error", err)
				continue
			}
			s.delivered.set(composition.ID, target.key, behind)
			s.logger.Info("tracked files brought up to date", "composition", composition.ID, "layer", target.key, "files", len(behind))
		}
	}
}

// shareTrackedFiles writes files a capsule changed into every other
// running capsule of the same layer and user.
func (s *Server) shareTrackedFiles(ctx context.Context, tracked capsule.TrackedFiles, sourceID, key string, files map[string][]byte) {
	for _, other := range s.store.Snapshot().Compositions {
		if other.ID == sourceID || other.Runtime == nil || other.Runtime.Status == "stopped" {
			continue
		}
		var shares bool
		for _, target := range s.trackedTargets(other) {
			if target.key == key {
				shares = true
				break
			}
		}
		if !shares {
			continue
		}
		if err := tracked.WriteTrackedFiles(ctx, *other.Runtime, files); err != nil {
			s.logger.Warn("share tracked files", "from", sourceID, "to", other.ID, "layer", key, "error", err)
			continue
		}
		s.delivered.set(other.ID, key, files)
		s.logger.Info("tracked files shared", "from", sourceID, "to", other.ID, "layer", key, "files", len(files))
	}
}
