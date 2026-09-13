package server

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"slices"
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
	excludes  []string
	exclusive bool // a credential layer: one login per running capsule
}

// selection is what to read from the capsule for this target.
func (t trackedTarget) selection() capsule.TrackedSelection {
	return capsule.TrackedSelection{Paths: t.paths, Excludes: t.excludes}
}

// folders are the tracked folders of the target, kept whole in a login.
func (t trackedTarget) folders() []string {
	var folders []string
	for _, path := range t.paths {
		if domain.TrackedFolder(path) {
			folders = append(folders, path)
		}
	}
	return folders
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
		target := trackedTarget{key: store.LayerKey(artifact), label: string(artifact.Kind) + ":" + artifact.Name, paths: artifact.TrackedPaths, excludes: artifact.TrackedExcludes, exclusive: artifact.Kind == domain.ArtifactCredential}
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
		if !s.store.LoginsFree(target.key, target.exclusive, composition.Operator) {
			return s.loginsBusy(target)
		}
	}
	return nil
}

// loginsBusy says every login of the layer is held, and by what, so the
// person knows which capsule to stop or that a login is to be added.
func (s *Server) loginsBusy(target trackedTarget) error {
	snapshot := s.store.Snapshot()
	var holders []string
	total := 0
	for _, login := range snapshot.Logins {
		if login.Key != target.key {
			continue
		}
		total++
		if login.CompositionID == "" {
			continue
		}
		holders = append(holders, fmt.Sprintf("login %d by %s", login.Number, s.describeHolder(snapshot, login.CompositionID)))
	}
	detail := "stop a capsule or add a login"
	if len(holders) > 0 {
		detail = strings.Join(holders, ", ") + "; stop that capsule or add a login"
	}
	return fmt.Errorf("every login of %s is in use (%d of %d): %s: %w", target.label, len(holders), total, detail, store.ErrLoginsBusy)
}

// describeHolder names the capsule that holds a login the way a person
// finds it: the Job and its step, a login capsule, or whose capsule.
func (s *Server) describeHolder(snapshot domain.Snapshot, compositionID string) string {
	for _, composition := range snapshot.Compositions {
		if composition.ID != compositionID {
			continue
		}
		if composition.ForLogin {
			return "a capsule started to log in (" + composition.Operator + ")"
		}
		for _, session := range snapshot.Sessions {
			if session.ID != composition.SessionID {
				continue
			}
			for _, job := range snapshot.Jobs {
				if job.ID == session.JobID {
					step := session.Role
					if step == "" {
						step = "session"
					}
					return fmt.Sprintf("Job %q (%s)", job.Title, step)
				}
			}
			return "a Session of " + composition.Operator
		}
		return "a capsule of " + composition.Operator
	}
	return "a capsule that is gone"
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
			files, readErr := tracked.ReadTrackedFiles(ctx, *composition.Runtime, target.selection())
			if readErr != nil {
				return fmt.Errorf("read the files of %s: %w", target.label, readErr)
			}
			if len(files) == 0 {
				s.logger.Warn("layer tracks files the capsule does not hold", "composition", composition.ID, "layer", target.key)
				continue
			}
			if login, err = s.store.CreateLogin(composition.ID, target.key, files, ""); err != nil {
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

// keepLoginsWhenACPEnds reads a capsule's tracked files into its logins
// the moment its agent process ends, however it ends: a chat closed, a
// crash, a stop. A token the agent refreshed during its last turn is then
// kept even when no turn end followed.
func (s *Server) keepLoginsWhenACPEnds(active *activeACP, compositionID string) {
	<-active.done
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	composition, err := s.store.Composition(compositionID)
	if err != nil || composition.Runtime == nil || composition.Runtime.Status == "stopped" || len(composition.Logins) == 0 || !s.engineConnected(composition.Runtime.ClientID) {
		return
	}
	s.keepLogins(ctx, composition)
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
		files, err := tracked.ReadTrackedFiles(ctx, *composition.Runtime, target.selection())
		if err != nil {
			s.logger.Warn("read tracked files", "composition", composition.ID, "layer", target.key, "error", err)
			continue
		}
		changed, err := s.store.SaveLoginFiles(loginID, files, target.folders())
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
		files, err := tracked.ReadTrackedFiles(ctx, *composition.Runtime, target.selection())
		if err != nil {
			return nil, fmt.Errorf("read the files of %s: %w", target.label, err)
		}
		owner := ""
		if composition.ForLoginPrivate {
			owner = composition.Operator
		}
		login, err := s.store.CreateLogin(composition.ID, target.key, files, owner)
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
		summaries = append(summaries, domain.LoginSummary{ID: login.ID, Key: login.Key, Number: login.Number, Owner: login.Owner, Files: len(login.Files), CreatedAt: login.CreatedAt, UpdatedAt: login.UpdatedAt})
	}
	writeJSON(w, http.StatusCreated, summaries)
}

// mayManageLogins says whether the request's user owns the layer of the
// key (its subject, or whoever recorded a shared layer) or is an admin.
func (s *Server) mayManageLogins(r *http.Request, key string) bool {
	identity, authenticated := identityFromRequest(r)
	if !authenticated || identity.User.Role == "admin" {
		return true
	}
	user := normalizeOperator(identity.User.Username)
	if user == strings.SplitN(key, "/", 2)[0] {
		return true
	}
	for _, artifact := range s.store.Snapshot().Artifacts {
		if store.LayerKey(artifact) == key && normalizeOperator(artifact.CreatedBy) == user {
			return true
		}
	}
	return false
}

// loginFilesHandler lists what a login holds, so a person sees what an
// agent started to collect and can take it out of the layer's tracking.
func (s *Server) loginFilesHandler(w http.ResponseWriter, r *http.Request) {
	files, ok := s.store.LoginFiles(r.PathValue("loginID"))
	if !ok {
		writeError(w, store.ErrNotFound)
		return
	}
	writeJSON(w, http.StatusOK, files)
}

// excludeLoginPathHandler adds a file or folder to the layer's excludes
// and takes it out of every login of the layer at once.
func (s *Server) excludeLoginPathHandler(w http.ResponseWriter, r *http.Request) {
	var request struct {
		Path string `json:"path"`
	}
	if !decodeJSON(w, r, &request) {
		return
	}
	artifact, err := s.store.Artifact(r.PathValue("artifactID"))
	if err != nil {
		writeError(w, err)
		return
	}
	key := store.LayerKey(artifact)
	if !s.mayManageLogins(r, key) {
		writeError(w, fmt.Errorf("only the layer's owner or an admin changes what it keeps: %w", store.ErrConflict))
		return
	}
	path := strings.TrimSpace(request.Path)
	updated, err := s.store.SetArtifactTracked(artifact.ID, artifact.TrackedPaths, append(append([]string{}, artifact.TrackedExcludes...), path))
	if err != nil {
		writeError(w, err)
		return
	}
	if !slices.Contains(updated.TrackedExcludes, path) {
		writeError(w, fmt.Errorf("%s lies outside the folders the layer keeps; untick it in Inhoud instead: %w", path, store.ErrConflict))
		return
	}
	removed, err := s.store.ExcludeFromLogins(key, path)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"artifact": updated, "removed": removed})
}

// deleteLoginHandler removes a login; the layer's owner or an admin may.
// A login a capsule holds goes too: that capsule is closed first, and
// marked stopped even when its runner cannot be reached, so a login can
// always be taken away when something hangs.
func (s *Server) deleteLoginHandler(w http.ResponseWriter, r *http.Request) {
	login, ok := s.store.Login(r.PathValue("loginID"))
	if !ok {
		writeError(w, store.ErrNotFound)
		return
	}
	if !s.mayManageLogins(r, login.Key) {
		writeError(w, fmt.Errorf("only the layer's owner or an admin removes this login: %w", store.ErrConflict))
		return
	}
	for _, composition := range s.store.RunningCompositions() {
		if composition.Logins[login.Key] != login.ID {
			continue
		}
		if _, err := s.stopCapsule(r.Context(), composition.ID, composition.Operator); err != nil {
			s.logger.Warn("close the capsule that holds a removed login", "composition", composition.ID, "error", err)
			runtime := *composition.Runtime
			runtime.Status = "stopped"
			if _, err := s.store.SetCompositionRuntime(composition.ID, composition.Operator, runtime); err != nil {
				writeError(w, fmt.Errorf("the capsule that holds this login could not be closed: %w", err))
				return
			}
		}
	}
	if _, err := s.store.DeleteLogin(login.ID); err != nil {
		writeError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}
