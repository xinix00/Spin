package store

import (
	"bytes"
	"errors"
	"fmt"
	"sort"
	"strings"
	"time"

	"easyacp/internal/domain"
)

// ErrLoginsBusy says every login of a credential layer is held by a
// running capsule; one more capsule needs one more login.
var ErrLoginsBusy = errors.New("every login of the layer is in use")

// legacyLoginState is the one login per layer of before v1.28.53.
type legacyLoginState struct {
	Key       string            `json:"key"`
	Files     map[string][]byte `json:"files"`
	UpdatedAt time.Time         `json:"updated_at"`
}

// LayerKey names a layer across its versions: subject/kind:name. Logins
// belong to the key, so a new version of the layer keeps them.
func LayerKey(artifact domain.Artifact) string {
	return artifact.Subject + "/" + string(artifact.Kind) + ":" + artifact.Name
}

func (s *Store) loginsForLocked(key string) []domain.Login {
	var logins []domain.Login
	for _, login := range s.state.Logins {
		if login.Key == key {
			logins = append(logins, login)
		}
	}
	sort.Slice(logins, func(i, j int) bool { return logins[i].Number < logins[j].Number })
	return logins
}

// loginHolderLocked is the running capsule that holds the login, if any.
func (s *Store) loginHolderLocked(login domain.Login) (domain.Composition, bool) {
	for _, composition := range s.state.Compositions {
		if composition.Runtime != nil && composition.Runtime.Status != "stopped" && composition.Logins[login.Key] == login.ID {
			return composition, true
		}
	}
	return domain.Composition{}, false
}

func (s *Store) loginSummariesLocked() []domain.LoginSummary {
	out := make([]domain.LoginSummary, 0, len(s.state.Logins))
	for _, login := range s.state.Logins {
		summary := domain.LoginSummary{ID: login.ID, Key: login.Key, Number: login.Number, Files: len(login.Files), CreatedAt: login.CreatedAt, UpdatedAt: login.UpdatedAt}
		for _, data := range login.Files {
			summary.Bytes += int64(len(data))
		}
		if holder, held := s.loginHolderLocked(login); held {
			summary.CompositionID = holder.ID
		}
		out = append(out, summary)
	}
	sort.Slice(out, func(i, j int) bool {
		if out[i].Key != out[j].Key {
			return out[i].Key < out[j].Key
		}
		return out[i].Number < out[j].Number
	})
	return out
}

// Login is one login with its files.
func (s *Store) Login(id string) (domain.Login, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	login, ok := s.state.Logins[id]
	return login, ok
}

// LoginsFor lists the logins of a layer, in order.
func (s *Store) LoginsFor(key string) []domain.Login {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.loginsForLocked(key)
}

// LoginsFree says whether the capsule could get a login of the layer right
// now: a shared layer always can, a credential layer when one is free or
// when it has none yet (its own files then become the first). A look
// ahead before the slow part of a start; HandOutLogin decides.
func (s *Store) LoginsFree(key string, exclusive bool) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	logins := s.loginsForLocked(key)
	if !exclusive || len(logins) == 0 {
		return true
	}
	for _, login := range logins {
		if _, held := s.loginHolderLocked(login); !held {
			return true
		}
	}
	return false
}

// HandOutLogin gives the running capsule a login of the layer: for a
// credential layer one that no other running capsule holds, for any other
// layer the one login everybody shares. ErrNotFound when the layer has no
// login yet, ErrLoginsBusy when every login is held.
func (s *Store) HandOutLogin(compositionID, key string, exclusive bool) (domain.Login, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	composition, ok := s.state.Compositions[compositionID]
	if !ok {
		return domain.Login{}, ErrNotFound
	}
	if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return domain.Login{}, fmt.Errorf("a login goes to a running capsule: %w", ErrConflict)
	}
	if id, held := composition.Logins[key]; held {
		if login, ok := s.state.Logins[id]; ok {
			return login, nil
		}
	}
	logins := s.loginsForLocked(key)
	if len(logins) == 0 {
		return domain.Login{}, ErrNotFound
	}
	for _, login := range logins {
		holder, held := s.loginHolderLocked(login)
		if exclusive && held && holder.ID != compositionID {
			continue
		}
		return login, s.holdLoginLocked(composition, login)
	}
	return domain.Login{}, ErrLoginsBusy
}

func (s *Store) holdLoginLocked(composition domain.Composition, login domain.Login) error {
	if composition.Logins == nil {
		composition.Logins = map[string]string{}
	}
	composition.Logins[login.Key] = login.ID
	s.state.Compositions[composition.ID] = composition
	return s.saveLocked()
}

// CreateLogin adds a login of the layer with these files and hands it to
// the running capsule the files came from, so that capsule's later
// changes keep it up to date; no capsule when compositionID is empty.
func (s *Store) CreateLogin(compositionID, key string, files map[string][]byte) (domain.Login, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	composition, ok := s.state.Compositions[compositionID]
	if !ok && compositionID != "" {
		return domain.Login{}, ErrNotFound
	}
	if len(files) == 0 {
		return domain.Login{}, fmt.Errorf("the capsule holds none of the layer's tracked files: %w", ErrConflict)
	}
	number := 0
	for _, login := range s.loginsForLocked(key) {
		if login.Number > number {
			number = login.Number
		}
	}
	now := time.Now().UTC()
	login := domain.Login{ID: newID("lgn"), Key: key, Number: number + 1, Files: copyFiles(files), CreatedAt: now, UpdatedAt: now}
	s.state.Logins[login.ID] = login
	if compositionID == "" {
		return login, s.saveLocked()
	}
	return login, s.holdLoginLocked(composition, login)
}

// SaveLoginFiles keeps the files as a capsule has them now. A file the
// capsule did not report stays as it was, unless it lies in one of the
// folders (paths ending in "/"): a folder is kept whole, so a file gone
// from it in the capsule goes from the login too. It reports whether
// anything differed.
func (s *Store) SaveLoginFiles(id string, files map[string][]byte, folders []string) (bool, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	login, ok := s.state.Logins[id]
	if !ok {
		return false, ErrNotFound
	}
	merged := copyFiles(login.Files)
	changed := false
	for path := range merged {
		if _, present := files[path]; present {
			continue
		}
		for _, folder := range folders {
			if domain.TrackedFolder(folder) && strings.HasPrefix(path, folder) {
				delete(merged, path)
				changed = true
				break
			}
		}
	}
	for path, data := range files {
		if !bytes.Equal(merged[path], data) {
			merged[path] = append([]byte(nil), data...)
			changed = true
		}
	}
	if !changed {
		return false, nil
	}
	login.Files = merged
	login.UpdatedAt = time.Now().UTC()
	s.state.Logins[id] = login
	return true, s.saveLocked()
}

// LoginFiles lists the files of a login with their sizes, in path order.
func (s *Store) LoginFiles(id string) ([]domain.LoginFile, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	login, ok := s.state.Logins[id]
	if !ok {
		return nil, false
	}
	files := make([]domain.LoginFile, 0, len(login.Files))
	for path, data := range login.Files {
		files = append(files, domain.LoginFile{Path: path, Size: int64(len(data))})
	}
	sort.Slice(files, func(i, j int) bool { return files[i].Path < files[j].Path })
	return files, true
}

// ExcludeFromLogins takes a file, or a folder (path ending in "/"), out of
// every login of the layer; it reports how many files went.
func (s *Store) ExcludeFromLogins(key, path string) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	removed := 0
	for id, login := range s.state.Logins {
		if login.Key != key {
			continue
		}
		changed := false
		for file := range login.Files {
			if file == path || (domain.TrackedFolder(path) && strings.HasPrefix(file, path)) {
				delete(login.Files, file)
				removed++
				changed = true
			}
		}
		if changed {
			login.UpdatedAt = time.Now().UTC()
			s.state.Logins[id] = login
		}
	}
	if removed == 0 {
		return 0, nil
	}
	return removed, s.saveLocked()
}

// DeleteLogin removes a login nobody holds.
func (s *Store) DeleteLogin(id string) (domain.Login, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	login, ok := s.state.Logins[id]
	if !ok {
		return domain.Login{}, ErrNotFound
	}
	if holder, held := s.loginHolderLocked(login); held {
		return domain.Login{}, fmt.Errorf("login %d is held by a running capsule (%s); stop it first: %w", login.Number, holder.ID, ErrConflict)
	}
	delete(s.state.Logins, id)
	return login, s.saveLocked()
}

func copyFiles(files map[string][]byte) map[string][]byte {
	copied := make(map[string][]byte, len(files))
	for path, data := range files {
		copied[path] = append([]byte(nil), data...)
	}
	return copied
}
