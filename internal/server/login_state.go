package server

import (
	"context"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

// A layer can track files: a login an agent rotates, a config it keeps.
// Chosen by a person in the layer's contents. After every turn and at
// stop Spin reads those files back from the capsule and keeps them,
// encrypted in the database per layer and user; before the next agent
// starts they are put in place. Nothing else the agent did in the capsule
// comes along.

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

// restoreLoginState puts the kept files in the capsule before the agent
// starts.
func (s *Server) restoreLoginState(ctx context.Context, composition domain.Composition) {
	tracked, ok := s.engine.(capsule.TrackedFiles)
	if !ok || composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return
	}
	for _, target := range s.trackedTargets(composition) {
		state, ok := s.store.LoginState(target.key)
		if !ok || len(state.Files) == 0 {
			continue
		}
		if err := tracked.WriteTrackedFiles(ctx, *composition.Runtime, state.Files); err != nil {
			s.logger.Warn("restore tracked files", "composition", composition.ID, "layer", target.key, "error", err)
			continue
		}
		s.logger.Info("tracked files restored", "composition", composition.ID, "layer", target.key, "files", len(state.Files), "kept_at", state.UpdatedAt.Format(time.RFC3339))
	}
}

// captureLoginState reads the tracked files back from a running capsule
// and keeps what changed; it also notes what else the capsule changed.
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
	for _, target := range s.trackedTargets(composition) {
		files, err := tracked.ReadTrackedFiles(ctx, *composition.Runtime, target.paths)
		if err != nil {
			s.logger.Warn("read tracked files", "composition", composition.ID, "layer", target.key, "error", err)
			continue
		}
		if len(files) == 0 {
			continue
		}
		changed, err := s.store.SaveLoginState(target.key, files)
		if err != nil {
			s.logger.Warn("keep tracked files", "layer", target.key, "error", err)
			continue
		}
		if changed {
			s.logger.Info("tracked files kept", "layer", target.key, "files", len(files))
		}
	}
}
