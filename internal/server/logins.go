package server

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// A layer can track files: a login an agent rotates, a config it keeps.
// Chosen by a person in the layer's contents. The files live on the server
// as logins of the layer (encrypted in the database). A credential layer
// is a hand-out system: a person logs in as often as there should be
// capsules at once, and every running capsule gets one login that no other
// running capsule holds, so a token refreshed in one capsule never
// invalidates another. Any other layer has one login every capsule shares.
// A capsule gets its login when it starts; after every turn and at stop
// the files are read back into that login. Nothing travels between running
// capsules.

type trackedTarget struct {
	key       string
	label     string
	paths     []string
	exclusive bool // a credential layer: one login per running capsule
}

// trackedTargets are the layers of a composition that track files, keyed
// per user and layer name across versions.
func (s *Server) trackedTargets(composition domain.Composition) []trackedTarget {
	// A stack holds every version of a layer, the newer right above the
	// older; both carry the tracked paths. One layer is one target, with the
	// newest version's paths, or a saved login would be made twice.
	var targets []trackedTarget
	index := map[string]int{}
	for _, artifactID := range capsule.CompositionLayers(composition) {
		artifact, err := s.store.Artifact(artifactID)
		if err != nil || len(artifact.TrackedPaths) == 0 {
			continue
		}
		target := trackedTarget{key: store.LayerKey(artifact), label: string(artifact.Kind) + ":" + artifact.Name, paths: artifact.TrackedPaths, exclusive: artifact.Kind == domain.ArtifactCredential}
		if at, seen := index[target.key]; seen {
			targets[at] = target
			continue
		}
		index[target.key] = len(targets)
		targets = append(targets, target)
	}
	return targets
}

// loginsAvailable says whether every layer of the composition can hand
// the capsule a login right now; the cheap look before a capsule is built.
func (s *Server) loginsAvailable(composition domain.Composition) error {
	if composition.ForLogin {
		return nil
	}
	for _, target := range s.trackedTargets(composition) {
		if !s.store.LoginsFree(target.key, target.exclusive) {
			return s.loginsBusy(target)
		}
	}
	return nil
}

func (s *Server) loginsBusy(target trackedTarget) error {
	logins := s.store.LoginsFor(target.key)
	return fmt.Errorf("every login of %s is in use (%d of %d); stop a capsule or add a login: %w", target.label, len(logins), len(logins), store.ErrLoginsBusy)
}

// handOutLogins gives the running capsule a login of every layer that
// tracks files and puts that login's files in it. A layer without a login
// yet gets its first from what the capsule holds: the layer's own files.
// A capsule started to log in once more gets nothing and keeps the
// layer's files.
func (s *Server) handOutLogins(ctx context.Context, composition domain.Composition) error {
	tracked, ok := s.engine.(capsule.TrackedFiles)
	if !ok || composition.ForLogin || composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return nil
	}
	for _, target := range s.trackedTargets(composition) {
		login, err := s.store.HandOutLogin(composition.ID, target.key, target.exclusive)
		switch {
		case errors.Is(err, store.ErrLoginsBusy):
			return s.loginsBusy(target)
		case errors.Is(err, store.ErrNotFound):
			files, readErr := tracked.ReadTrackedFiles(ctx, *composition.Runtime, target.paths)
			if readErr != nil {
				return fmt.Errorf("read the files of %s: %w", target.label, readErr)
			}
			if len(files) == 0 {
				s.logger.Warn("layer tracks files the capsule does not hold", "composition", composition.ID, "layer", target.key)
				continue
			}
			if login, err = s.store.CreateLogin(composition.ID, target.key, files); err != nil {
				return fmt.Errorf("keep the first login of %s: %w", target.label, err)
			}
			s.logger.Info("login made from the layer's own files", "composition", composition.ID, "layer", target.key, "login", login.Number, "files", len(files))
			continue
		case err != nil:
			return fmt.Errorf("hand out a login of %s: %w", target.label, err)
		}
		if err := tracked.WriteTrackedFiles(ctx, *composition.Runtime, login.Files); err != nil {
			return fmt.Errorf("place login %d of %s: %w", login.Number, target.label, err)
		}
		s.logger.Info("login handed out", "composition", composition.ID, "layer", target.key, "login", login.Number, "files", len(login.Files), "kept_at", login.UpdatedAt.Format(time.RFC3339))
	}
	return nil
}

// captureLoginState is the full look at a capsule at the end of a turn and
// at stop: its tracked files, and what else it changed.
func (s *Server) captureLoginState(ctx context.Context, composition domain.Composition) {
	if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return
	}
	s.captureCapsuleChanges(ctx, composition)
	s.keepLogins(ctx, composition)
}

// captureCapsuleChanges notes what a capsule changed outside its workspace.
func (s *Server) captureCapsuleChanges(ctx context.Context, composition domain.Composition) {
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
}

// keepLogins reads the tracked files back from a running capsule into the
// logins it holds; the database is written only when a file changed.
func (s *Server) keepLogins(ctx context.Context, composition domain.Composition) {
	tracked, ok := s.engine.(capsule.TrackedFiles)
	if !ok || len(composition.Logins) == 0 {
		return
	}
	for _, target := range s.trackedTargets(composition) {
		loginID, held := composition.Logins[target.key]
		if !held {
			continue
		}
		files, err := tracked.ReadTrackedFiles(ctx, *composition.Runtime, target.paths)
		if err != nil {
			s.logger.Warn("read tracked files", "composition", composition.ID, "layer", target.key, "error", err)
			continue
		}
		changed, err := s.store.SaveLoginFiles(loginID, files)
		if err != nil {
			s.logger.Warn("keep login", "composition", composition.ID, "layer", target.key, "error", err)
			continue
		}
		if changed {
			s.logger.Info("login kept", "composition", composition.ID, "layer", target.key, "files", len(files))
		}
	}
}

// saveNewLogin turns what a person logged in as, in a capsule started for
// it, into a new login of every credential layer that tracks files; the
// capsule holds those logins from then on.
func (s *Server) saveNewLogin(ctx context.Context, compositionID, operator string) ([]domain.Login, error) {
	composition, err := s.store.Composition(compositionID)
	if err != nil {
		return nil, err
	}
	if composition.Operator != normalizeOperator(operator) {
		return nil, store.ErrConflict
	}
	if !composition.ForLogin {
		return nil, fmt.Errorf("this capsule was not started to log in; start one with Nieuwe login: %w", store.ErrConflict)
	}
	if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return nil, fmt.Errorf("the capsule is not running: %w", store.ErrConflict)
	}
	tracked, ok := s.engine.(capsule.TrackedFiles)
	if !ok {
		return nil, fmt.Errorf("capsule engine cannot read tracked files: %w", store.ErrConflict)
	}
	var logins []domain.Login
	for _, target := range s.trackedTargets(composition) {
		if !target.exclusive {
			continue
		}
		if _, held := composition.Logins[target.key]; held {
			continue
		}
		files, err := tracked.ReadTrackedFiles(ctx, *composition.Runtime, target.paths)
		if err != nil {
			return nil, fmt.Errorf("read the files of %s: %w", target.label, err)
		}
		login, err := s.store.CreateLogin(composition.ID, target.key, files)
		if err != nil {
			return nil, fmt.Errorf("%s: %w", target.label, err)
		}
		s.logger.Info("login saved", "composition", composition.ID, "layer", target.key, "login", login.Number, "files", len(files))
		logins = append(logins, login)
	}
	if len(logins) == 0 {
		return nil, fmt.Errorf("no credential layer with tracked files in this capsule, or its login is already saved: %w", store.ErrConflict)
	}
	return logins, nil
}

// saveLoginHandler saves a new login from a capsule started for it, and
// closes that capsule: it did what it was for, and the login it now holds
// is free for the next capsule right away.
func (s *Server) saveLoginHandler(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, "")
	logins, err := s.saveNewLogin(r.Context(), r.PathValue("compositionID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	if _, err := s.stopCapsule(r.Context(), r.PathValue("compositionID"), operator); err != nil {
		s.logger.Warn("close the capsule after saving a login", "composition", r.PathValue("compositionID"), "error", err)
	}
	summaries := make([]domain.LoginSummary, 0, len(logins))
	for _, login := range logins {
		summaries = append(summaries, domain.LoginSummary{ID: login.ID, Key: login.Key, Number: login.Number, Files: len(login.Files), CreatedAt: login.CreatedAt, UpdatedAt: login.UpdatedAt})
	}
	writeJSON(w, http.StatusCreated, summaries)
}

// deleteLoginHandler removes a login nobody holds; the layer's owner or an
// admin may.
func (s *Server) deleteLoginHandler(w http.ResponseWriter, r *http.Request) {
	login, ok := s.store.Login(r.PathValue("loginID"))
	if !ok {
		writeError(w, store.ErrNotFound)
		return
	}
	subject := strings.SplitN(login.Key, "/", 2)[0]
	if identity, authenticated := identityFromRequest(r); authenticated && identity.User.Role != "admin" && identity.User.Username != subject {
		writeError(w, fmt.Errorf("only %s or an admin removes this login: %w", subject, store.ErrConflict))
		return
	}
	if _, err := s.store.DeleteLogin(login.ID); err != nil {
		writeError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}
