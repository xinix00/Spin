package store

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"strings"
	"time"

	"easyacp/internal/domain"
)

// PortableState is the encrypted, schema-native control-plane state plus the
// key needed to decrypt it. The surrounding server backup keeps these in one
// admin-only archive and re-encrypts with the destination server key on import.
type PortableState struct {
	JSON      []byte
	MasterKey string
}

type PortableStateInspection struct {
	Artifacts    []domain.Artifact
	Attachments  []domain.JobAttachment
	Users        int
	Jobs         int
	Templates    int
	Deliverables int
}

func (s *Store) ExportPortableState() (PortableState, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	persisted, err := s.encryptedStateLocked()
	if err != nil {
		return PortableState{}, fmt.Errorf("encrypt backup state: %w", err)
	}
	var encoded bytes.Buffer
	encoder := json.NewEncoder(&encoded)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(persisted); err != nil {
		return PortableState{}, fmt.Errorf("encode backup state: %w", err)
	}
	return PortableState{JSON: encoded.Bytes(), MasterKey: s.secrets.portableKey()}, nil
}

func (s *Store) InspectPortableState(encoded []byte, sourceMasterKey string) (PortableStateInspection, error) {
	candidate, err := decodePortableState(encoded, sourceMasterKey)
	if err != nil {
		return PortableStateInspection{}, err
	}
	attachments := make([]domain.JobAttachment, 0, len(candidate.JobAttachments))
	for _, attachment := range candidate.JobAttachments {
		attachments = append(attachments, attachment)
	}
	artifacts := make([]domain.Artifact, 0, len(candidate.Artifacts))
	for _, artifact := range candidate.Artifacts {
		artifacts = append(artifacts, artifact)
	}
	return PortableStateInspection{
		Artifacts: artifacts, Attachments: attachments,
		Users: len(candidate.Users), Jobs: len(candidate.Jobs), Templates: len(candidate.WorkflowTemplates), Deliverables: len(candidate.Deliverables),
	}, nil
}

// RestorePortableState validates and decrypts with the source key before it
// takes the Store lock. It then replaces state through the normal atomic save,
// which encrypts every secret with this server's own key. A failed write puts
// the old in-memory state back.
func (s *Store) RestorePortableState(encoded []byte, sourceMasterKey string) error {
	candidate, err := decodePortableState(encoded, sourceMasterKey)
	if err != nil {
		return err
	}
	normalizeRestoredState(&candidate)

	s.mu.Lock()
	defer s.mu.Unlock()
	previous := s.state
	s.state = candidate
	if err := s.saveLocked(); err != nil {
		s.state = previous
		return fmt.Errorf("persist restored state: %w", err)
	}
	return nil
}

func decodePortableState(encoded []byte, sourceMasterKey string) (persistedState, error) {
	if len(encoded) == 0 {
		return persistedState{}, fmt.Errorf("backup state is empty: %w", ErrConflict)
	}
	decoder := json.NewDecoder(bytes.NewReader(encoded))
	decoder.DisallowUnknownFields()
	var candidate persistedState
	if err := decoder.Decode(&candidate); err != nil {
		return persistedState{}, fmt.Errorf("decode backup state: %w", err)
	}
	if err := requireJSONEOF(decoder); err != nil {
		return persistedState{}, err
	}
	cipher, err := newSecretCipher(OpenOptions{MasterKey: strings.TrimSpace(sourceMasterKey)}, "")
	if err != nil {
		return persistedState{}, fmt.Errorf("open backup master key: %w", err)
	}
	temporary := &Store{secrets: cipher, state: candidate}
	temporary.ensureMaps()
	if err := temporary.decryptSecretsLocked(); err != nil {
		return persistedState{}, fmt.Errorf("decrypt backup state: %w", err)
	}
	if err := validatePortableState(temporary.state); err != nil {
		return persistedState{}, err
	}
	return temporary.state, nil
}

func requireJSONEOF(decoder *json.Decoder) error {
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		if err == nil {
			return fmt.Errorf("backup state contains multiple JSON values: %w", ErrConflict)
		}
		return fmt.Errorf("read backup state trailer: %w", err)
	}
	return nil
}

func validatePortableState(state persistedState) error {
	if len(state.Users) == 0 {
		return fmt.Errorf("backup has no users: %w", ErrConflict)
	}
	admins := 0
	for id, user := range state.Users {
		if id == "" || user.ID != id || user.Username == "" || user.PasswordHash == "" {
			return fmt.Errorf("backup user %q is invalid: %w", id, ErrConflict)
		}
		if user.Role == domain.UserAdmin {
			admins++
		} else if user.Role != domain.UserMember {
			return fmt.Errorf("backup user %s has invalid role %q: %w", user.Username, user.Role, ErrConflict)
		}
	}
	if admins == 0 {
		return fmt.Errorf("backup has no admin user: %w", ErrConflict)
	}
	for id, attachment := range state.JobAttachments {
		if id == "" || attachment.ID != id || attachment.Name == "" || attachment.Size < 0 || len(attachment.SHA256) != 64 {
			return fmt.Errorf("backup attachment %q is invalid: %w", id, ErrConflict)
		}
		if attachment.JobID != "" {
			if _, ok := state.Jobs[attachment.JobID]; !ok {
				return fmt.Errorf("backup attachment %s refers to missing Job %s: %w", id, attachment.JobID, ErrConflict)
			}
		}
	}
	return nil
}

func normalizeRestoredState(state *persistedState) {
	now := time.Now().UTC()
	// Browser sessions and runtime handles belong to the machine that produced
	// the backup. Durable jobs and audit events remain; live work becomes frozen
	// and can be restarted explicitly from the UI.
	state.AuthSessions = map[string]domain.AuthSession{}
	for id, client := range state.Clients {
		client.Status = "offline"
		state.Clients[id] = client
	}
	for id, composition := range state.Compositions {
		composition.Runtime = nil
		state.Compositions[id] = composition
	}
	for id, recording := range state.Recordings {
		if recording.Status == domain.RecordingOpen {
			recording.Status = domain.RecordingCancelled
			recording.EndedAt = &now
			recording.Runtime = nil
			state.Recordings[id] = recording
		}
	}
	for id, session := range state.Sessions {
		if session.Status == domain.SessionClaimed || session.Status == domain.SessionRunning {
			session.Status = domain.SessionFrozen
			session.LeaseExpiresAt = nil
			session.ActivationID = ""
			session.UpdatedAt = now
			state.Sessions[id] = session
		}
	}
	for id, activation := range state.Activations {
		if activation.Status != domain.ActivationEnded {
			activation.Status = domain.ActivationEnded
			activation.Reason = "restored from portable backup"
			activation.EndedAt = &now
			state.Activations[id] = activation
		}
	}
	for id, turn := range state.Turns {
		if turn.Status == domain.TurnRunning {
			turn.Status = domain.TurnCompleted
			turn.EndedAt = &now
			state.Turns[id] = turn
		}
	}
}
