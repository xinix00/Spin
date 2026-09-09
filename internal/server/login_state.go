package server

import (
	"context"
	"strings"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

// An agent's login lives in a credential layer, and an agent rotates its
// OAuth tokens while it works. The rotated token stays in the capsule that
// ran, and the next Session starts from the layer's old one, which the
// provider then refuses. So Spin keeps the login state: after every turn
// and at stop it reads back the files the credential layer's recording
// wrote under HOME, encrypted in the database per layer and user, and puts
// them in place before the next agent starts. Nothing else the agent did
// in the capsule comes along.

type loginTarget struct {
	key        string
	credential domain.CapsuleSnapshot
}

// loginTargets are the credential layers of a composition, keyed per user
// and layer name across versions.
func (s *Server) loginTargets(composition domain.Composition) []loginTarget {
	var targets []loginTarget
	for slot, artifactID := range composition.SlotBindings {
		if !strings.HasPrefix(slot, "credential:") {
			continue
		}
		artifact, err := s.store.Artifact(artifactID)
		if err != nil || !artifact.Snapshot.Restorable || artifact.Snapshot.Driver != "docker" {
			continue
		}
		targets = append(targets, loginTarget{key: artifact.Subject + "/" + string(artifact.Kind) + ":" + artifact.Name, credential: artifact.Snapshot})
	}
	return targets
}

// restoreLoginState puts the kept logins in the capsule before the agent
// starts.
func (s *Server) restoreLoginState(ctx context.Context, composition domain.Composition) {
	login, ok := s.engine.(capsule.LoginState)
	if !ok || composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return
	}
	for _, target := range s.loginTargets(composition) {
		state, ok := s.store.LoginState(target.key)
		if !ok || len(state.Files) == 0 {
			continue
		}
		if err := login.WriteHomeFiles(ctx, *composition.Runtime, state.Files); err != nil {
			s.logger.Warn("restore login state", "composition", composition.ID, "login", target.key, "error", err)
			continue
		}
		s.logger.Info("login state restored", "composition", composition.ID, "login", target.key, "files", len(state.Files), "kept_at", state.UpdatedAt.Format(time.RFC3339))
	}
}

// captureLoginState reads the logins back from a running capsule and keeps
// what changed; it also notes what else the capsule changed, by kind.
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
	login, ok := s.engine.(capsule.LoginState)
	if !ok {
		return
	}
	for _, target := range s.loginTargets(composition) {
		files, err := login.CaptureLoginState(ctx, *composition.Runtime, target.credential)
		if err != nil {
			s.logger.Warn("capture login state", "composition", composition.ID, "login", target.key, "error", err)
			continue
		}
		if len(files) == 0 {
			continue
		}
		changed, err := s.store.SaveLoginState(target.key, files)
		if err != nil {
			s.logger.Warn("keep login state", "login", target.key, "error", err)
			continue
		}
		if changed {
			s.logger.Info("login state kept", "login", target.key, "files", len(files))
		}
	}
}
