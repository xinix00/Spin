package store

import (
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"easyacp/internal/domain"
)

var (
	ErrNotFound        = errors.New("not found")
	ErrConflict        = errors.New("conflict")
	ErrNoWork          = errors.New("no matching work")
	ErrStaleActivation = errors.New("stale activation")
)

type persistedState struct {
	Artifacts                map[string]domain.Artifact              `json:"artifacts"`
	Recordings               map[string]domain.Recording             `json:"recordings"`
	Compositions             map[string]domain.Composition           `json:"compositions"`
	Jobs                     map[string]domain.Job                   `json:"jobs"`
	JobAttachments           map[string]domain.JobAttachment         `json:"job_attachments"`
	WorkflowTemplates        map[string]domain.WorkflowTemplate      `json:"workflow_templates"`
	PhaseRuns                map[string]domain.PhaseRun              `json:"phase_runs"`
	Deliverables             map[string]domain.Deliverable           `json:"deliverables"`
	DeliverableComments      map[string]domain.DeliverableComment    `json:"deliverable_comments"`
	WorkflowQuestions        map[string]domain.WorkflowQuestion      `json:"workflow_questions"`
	JobRequestKeys           map[string]string                       `json:"job_request_keys"`
	Sessions                 map[string]domain.Session               `json:"sessions"`
	Activations              map[string]domain.Activation            `json:"activations"`
	Turns                    map[string]domain.Turn                  `json:"turns"`
	Checkpoints              map[string]domain.Checkpoint            `json:"checkpoints"`
	Results                  map[string]domain.Result                `json:"results"`
	Clients                  map[string]domain.Client                `json:"clients"`
	MCPServers               map[string]domain.MCPServer             `json:"mcp_servers"`
	GitRepositories          map[string]domain.GitRepository         `json:"git_repositories"`
	GitAccounts              map[string]domain.GitAccount            `json:"git_accounts"`
	LegacyGitAccountBindings map[string]legacyGitAccountBinding      `json:"git_account_bindings,omitempty"`
	Users                    map[string]domain.User                  `json:"users"`
	AuthSessions             map[string]domain.AuthSession           `json:"auth_sessions"`
	GitOAuthConfigurations   map[string]domain.GitOAuthConfiguration `json:"git_oauth_configurations"`
}

// legacyGitAccountBinding exists only to read pre-scope state. ensureMaps uses
// it once to infer user scope and then clears it before the state is rewritten.
type legacyGitAccountBinding struct {
	RepositoryID string `json:"repository_id"`
}

type Store struct {
	mu       sync.Mutex
	path     string
	backend  StateBackend
	leaseTTL time.Duration
	secrets  *secretCipher
	state    persistedState
}

func Open(path string) (*Store, error) {
	return OpenWithOptions(path, OpenOptions{})
}

func OpenWithOptions(path string, options OpenOptions) (*Store, error) {
	return OpenWithBackend(path, options, filesystemStateBackend{})
}

// StateBackend keeps the state machine independent from its persistence
// substrate. Linux uses the atomic filesystem backend below; HopOS supplies
// its volume-backed hop-ABI implementation from the app entrypoint.
type StateBackend interface {
	ReadFile(path string) ([]byte, error)
	WriteFile(path string, data []byte) error
}

func OpenWithBackend(path string, options OpenOptions, backend StateBackend) (*Store, error) {
	secrets, err := newSecretCipher(options, path)
	if err != nil {
		return nil, fmt.Errorf("initialize secret encryption: %w", err)
	}
	if backend == nil {
		backend = filesystemStateBackend{}
	}
	s := &Store{path: path, backend: backend, leaseTTL: 30 * time.Second, secrets: secrets, state: newState()}
	if path == "" {
		return s, nil
	}
	b, err := backend.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return s, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read state: %w", err)
	}
	if err := json.Unmarshal(b, &s.state); err != nil {
		return nil, fmt.Errorf("decode state: %w", err)
	}
	s.ensureMaps()
	if err := s.decryptSecretsLocked(); err != nil {
		return nil, fmt.Errorf("decrypt state: %w", err)
	}
	// Rewriting through the current schema permanently drops legacy terminal
	// input/output fields. Snapshots are canonical; transcripts are ephemeral.
	if err := s.saveLocked(); err != nil {
		return nil, fmt.Errorf("migrate state: %w", err)
	}
	return s, nil
}

func newState() persistedState {
	return persistedState{
		Artifacts:                map[string]domain.Artifact{},
		Recordings:               map[string]domain.Recording{},
		Compositions:             map[string]domain.Composition{},
		Jobs:                     map[string]domain.Job{},
		JobAttachments:           map[string]domain.JobAttachment{},
		WorkflowTemplates:        map[string]domain.WorkflowTemplate{},
		PhaseRuns:                map[string]domain.PhaseRun{},
		Deliverables:             map[string]domain.Deliverable{},
		DeliverableComments:      map[string]domain.DeliverableComment{},
		WorkflowQuestions:        map[string]domain.WorkflowQuestion{},
		JobRequestKeys:           map[string]string{},
		Sessions:                 map[string]domain.Session{},
		Activations:              map[string]domain.Activation{},
		Turns:                    map[string]domain.Turn{},
		Checkpoints:              map[string]domain.Checkpoint{},
		Results:                  map[string]domain.Result{},
		Clients:                  map[string]domain.Client{},
		MCPServers:               map[string]domain.MCPServer{},
		GitRepositories:          map[string]domain.GitRepository{},
		GitAccounts:              map[string]domain.GitAccount{},
		LegacyGitAccountBindings: map[string]legacyGitAccountBinding{},
		Users:                    map[string]domain.User{},
		AuthSessions:             map[string]domain.AuthSession{},
		GitOAuthConfigurations:   map[string]domain.GitOAuthConfiguration{},
	}
}

func (s *Store) ensureMaps() {
	if s.state.Artifacts == nil {
		s.state.Artifacts = map[string]domain.Artifact{}
	}
	if s.state.Recordings == nil {
		s.state.Recordings = map[string]domain.Recording{}
	}
	// Older states advertised Git through the generic Provides list. Upgrade
	// that persisted metadata once; all runtime selection below uses ENABLES.
	for id, artifact := range s.state.Artifacts {
		if slices.Contains(artifact.Provides, "tool:git") && !enablementsContain(artifact.Enables, "git") {
			artifact.Enables = mergeEnablements(artifact.Enables, []domain.Enablement{{Name: "git"}})
			s.state.Artifacts[id] = artifact
		}
	}
	for id, recording := range s.state.Recordings {
		if slices.Contains(recording.Provides, "tool:git") && !enablementsContain(recording.Enables, "git") {
			recording.Enables = mergeEnablements(recording.Enables, []domain.Enablement{{Name: "git"}})
			s.state.Recordings[id] = recording
		}
	}
	if s.state.Compositions == nil {
		s.state.Compositions = map[string]domain.Composition{}
	}
	if s.state.Jobs == nil {
		s.state.Jobs = map[string]domain.Job{}
	}
	if s.state.JobAttachments == nil {
		s.state.JobAttachments = map[string]domain.JobAttachment{}
	}
	if s.state.WorkflowTemplates == nil {
		s.state.WorkflowTemplates = map[string]domain.WorkflowTemplate{}
	}
	for id, template := range s.state.WorkflowTemplates {
		changed := false
		if template.Revision < 1 {
			template.Revision = 1
			changed = true
		}
		if template.GitSelector == "" {
			if selector, err := s.defaultEnabledSelectorLocked(template.CreatedBy, "git", "default"); err == nil {
				template.GitSelector = selector
				changed = true
			}
		}
		availableDeliverables := make([]string, 0)
		seenDeliverables := map[string]bool{}
		for index := range template.Phases {
			phase := &template.Phases[index]
			if phase.Executor == "" {
				phase.Executor = domain.WorkflowExecutorAgent
				changed = true
			}
			if phase.AllowCommit {
				phase.AllowChanges = true
				phase.AllowCommit = false
				changed = true
			}
			if phase.AskUser {
				phase.Accept.AskUser = true
				phase.Reject.AskUser = true
				phase.AskUser = false
				changed = true
			}
			// Templates saved before explicit injection existed received every
			// deliverable produced by an earlier phase. Preserve that behavior
			// once, then persist an explicit (possibly empty) selection.
			if phase.Inject == nil {
				phase.Inject = append([]string{}, availableDeliverables...)
				changed = true
			}
			for _, deliverable := range phase.Deliverables {
				key := strings.ToLower(strings.TrimSpace(deliverable.Name))
				if key == "" || seenDeliverables[key] {
					continue
				}
				seenDeliverables[key] = true
				availableDeliverables = append(availableDeliverables, strings.TrimSpace(deliverable.Name))
			}
		}
		var finalized bool
		template, finalized = ensureWorkflowPullRequestFinalizer(template)
		changed = changed || finalized
		if changed {
			s.state.WorkflowTemplates[id] = template
		}
	}
	for id, job := range s.state.Jobs {
		if job.TemplateID == "" {
			continue
		}
		if job.TemplateSnapshot != nil {
			frozen, changed := ensureWorkflowPullRequestFinalizer(cloneWorkflowTemplate(*job.TemplateSnapshot))
			if changed {
				job.TemplateSnapshot = &frozen
				s.state.Jobs[id] = job
			}
			continue
		}
		if template, ok := s.state.WorkflowTemplates[job.TemplateID]; ok {
			frozen := cloneWorkflowTemplate(template)
			job.TemplateSnapshot = &frozen
			s.state.Jobs[id] = job
		}
	}
	if s.state.PhaseRuns == nil {
		s.state.PhaseRuns = map[string]domain.PhaseRun{}
	}
	if s.state.Deliverables == nil {
		s.state.Deliverables = map[string]domain.Deliverable{}
	}
	if s.state.DeliverableComments == nil {
		s.state.DeliverableComments = map[string]domain.DeliverableComment{}
	}
	if s.state.WorkflowQuestions == nil {
		s.state.WorkflowQuestions = map[string]domain.WorkflowQuestion{}
	}
	if s.state.JobRequestKeys == nil {
		s.state.JobRequestKeys = map[string]string{}
	}
	if s.state.Sessions == nil {
		s.state.Sessions = map[string]domain.Session{}
	}
	for id, session := range s.state.Sessions {
		if session.Executor == "" {
			session.Executor = domain.WorkflowExecutorAgent
		}
		if job, ok := s.state.Jobs[session.JobID]; ok && job.Branch != "" {
			session.BaseRef = job.Branch
			session.TargetBranch = job.Branch
		}
		s.state.Sessions[id] = session
	}
	s.backfillWorkflowPullRequestsLocked()
	if s.state.Activations == nil {
		s.state.Activations = map[string]domain.Activation{}
	}
	if s.state.Turns == nil {
		s.state.Turns = map[string]domain.Turn{}
	}
	if s.state.Checkpoints == nil {
		s.state.Checkpoints = map[string]domain.Checkpoint{}
	}
	if s.state.Results == nil {
		s.state.Results = map[string]domain.Result{}
	}
	if s.state.Clients == nil {
		s.state.Clients = map[string]domain.Client{}
	}
	if s.state.MCPServers == nil {
		s.state.MCPServers = map[string]domain.MCPServer{}
	}
	if s.state.GitRepositories == nil {
		s.state.GitRepositories = map[string]domain.GitRepository{}
	}
	if s.state.GitAccounts == nil {
		s.state.GitAccounts = map[string]domain.GitAccount{}
	}
	if s.state.LegacyGitAccountBindings == nil {
		s.state.LegacyGitAccountBindings = map[string]legacyGitAccountBinding{}
	}
	for id, account := range s.state.GitAccounts {
		if account.CredentialScope == "" {
			account.CredentialScope = domain.CredentialScopeUser
			s.state.GitAccounts[id] = account
		}
	}
	for id, repository := range s.state.GitRepositories {
		if repository.Provider == "" {
			repository.Provider, _ = gitRemoteIdentity(repository.RemoteURL)
		}
		if repository.CredentialScope == "" {
			repository.CredentialScope = domain.CredentialScopePublic
			for _, binding := range s.state.LegacyGitAccountBindings {
				if binding.RepositoryID == repository.ID {
					repository.CredentialScope = domain.CredentialScopeUser
					break
				}
			}
		}
		s.state.GitRepositories[id] = repository
	}
	for id, job := range s.state.Jobs {
		if repository, ok := s.state.GitRepositories[job.GitRepositoryID]; ok {
			if job.GitRepositoryName == "" {
				job.GitRepositoryName = repository.Name
			}
			if job.GitRemoteURL == "" {
				job.GitRemoteURL = repository.RemoteURL
			}
			if job.GitProvider == "" {
				job.GitProvider = repository.Provider
			}
			if job.GitCredentialScope == "" {
				job.GitCredentialScope = repository.CredentialScope
			}
			s.state.Jobs[id] = job
		}
	}
	s.state.LegacyGitAccountBindings = map[string]legacyGitAccountBinding{}
	if s.state.Users == nil {
		s.state.Users = map[string]domain.User{}
	}
	if s.state.AuthSessions == nil {
		s.state.AuthSessions = map[string]domain.AuthSession{}
	}
	if s.state.GitOAuthConfigurations == nil {
		s.state.GitOAuthConfigurations = map[string]domain.GitOAuthConfiguration{}
	}
}

func (s *Store) CreateRecording(req domain.CreateRecordingRequest) (domain.Recording, error) {
	actor := normalizeSubject(req.Actor)
	name := normalizeName(req.Name)
	if actor == "" || name == "" || req.Kind == "" {
		return domain.Recording{}, fmt.Errorf("actor, kind and name are required: %w", ErrConflict)
	}
	if !validToken(name) {
		return domain.Recording{}, fmt.Errorf("invalid artifact name %q: %w", name, ErrConflict)
	}
	if !validKind(req.Kind) {
		return domain.Recording{}, fmt.Errorf("unsupported artifact kind %q: %w", req.Kind, ErrConflict)
	}
	if req.Scope == "" {
		if req.Kind == domain.ArtifactTool {
			req.Scope = domain.ScopeGlobal
		} else {
			req.Scope = domain.ScopeUser
		}
	}
	if !validScope(req.Scope) {
		return domain.Recording{}, fmt.Errorf("invalid scope %q: %w", req.Scope, ErrConflict)
	}
	if req.Profile == "" {
		req.Profile = "default"
	}
	if req.Scope == domain.ScopeUser {
		if req.Subject == "" {
			req.Subject = actor
		}
		if normalizeSubject(req.Subject) != actor {
			return domain.Recording{}, fmt.Errorf("a user recording must belong to the current actor: %w", ErrConflict)
		}
		req.Subject = actor
	} else {
		req.Subject = normalizeSubject(req.Subject)
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	if _, ok := s.openRecordingLocked(actor); ok {
		return domain.Recording{}, fmt.Errorf("actor already has an open recording: %w", ErrConflict)
	}

	applyArtifactDefaults(&req, name)
	enables, err := normalizeEnablements(req.Enables)
	if err != nil {
		return domain.Recording{}, err
	}
	parents := uniqueStrings(req.ParentArtifactIDs)
	for _, parentID := range parents {
		parent, ok := s.state.Artifacts[parentID]
		if !ok || !canUseArtifact(actor, parent) {
			return domain.Recording{}, fmt.Errorf("parent artifact %s: %w", parentID, ErrNotFound)
		}
	}
	now := time.Now().UTC()
	recording := domain.Recording{
		ID:                       newID("rec"),
		Actor:                    actor,
		Kind:                     req.Kind,
		Name:                     name,
		Scope:                    req.Scope,
		Subject:                  req.Subject,
		Profile:                  normalizeName(req.Profile),
		Provides:                 uniqueStrings(req.Provides),
		Requires:                 uniqueStrings(req.Requires),
		Enables:                  enables,
		Slot:                     strings.TrimSpace(req.Slot),
		ParentArtifactIDs:        parents,
		CompatibilityFingerprint: strings.TrimSpace(req.CompatibilityFingerprint),
		Sensitivity:              req.Sensitivity,
		Status:                   domain.RecordingOpen,
		Commands:                 []domain.RecordingCommand{},
		StartedAt:                now,
	}
	s.state.Recordings[recording.ID] = recording
	return recording, s.saveLocked()
}

func (s *Store) RecordExecution(recordingID, actor string, exitCode *int) (domain.Recording, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	recording, ok := s.state.Recordings[recordingID]
	if !ok {
		return domain.Recording{}, ErrNotFound
	}
	if recording.Status != domain.RecordingOpen || normalizeSubject(actor) != recording.Actor {
		return domain.Recording{}, ErrConflict
	}
	recording.Commands = append(recording.Commands, domain.RecordingCommand{
		Sequence: len(recording.Commands) + 1,
		ExitCode: exitCode,
		At:       time.Now().UTC(),
	})
	s.state.Recordings[recording.ID] = recording
	return recording, s.saveLocked()
}

func (s *Store) AttachRecordingParent(recordingID string, req domain.AttachRecordingParentRequest) (domain.Recording, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	recording, ok := s.state.Recordings[recordingID]
	if !ok {
		return domain.Recording{}, ErrNotFound
	}
	actor := normalizeSubject(req.Actor)
	if recording.Status != domain.RecordingOpen || actor != recording.Actor {
		return domain.Recording{}, ErrConflict
	}
	parent, ok := s.latestArtifactLocked(req.Kind, normalizeName(req.Name), actor, "")
	if !ok {
		return domain.Recording{}, ErrNotFound
	}
	if !slices.Contains(recording.ParentArtifactIDs, parent.ID) {
		recording.ParentArtifactIDs = append(recording.ParentArtifactIDs, parent.ID)
	}
	s.state.Recordings[recording.ID] = recording
	return recording, s.saveLocked()
}

func (s *Store) EndRecording(recordingID string, req domain.EndRecordingRequest) (domain.Artifact, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	recording, ok := s.state.Recordings[recordingID]
	if !ok {
		return domain.Artifact{}, ErrNotFound
	}
	if recording.Status != domain.RecordingOpen || normalizeSubject(req.Actor) != recording.Actor {
		return domain.Artifact{}, ErrConflict
	}
	now := time.Now().UTC()
	digest := strings.TrimSpace(req.SnapshotDigest)
	if req.Snapshot.Digest != "" {
		digest = strings.TrimSpace(req.Snapshot.Digest)
	}
	if digest == "" {
		encoded, _ := json.Marshal(recording)
		sum := sha256.Sum256(encoded)
		digest = "sha256:" + hex.EncodeToString(sum[:])
	}
	snapshot := req.Snapshot
	if snapshot.Driver == "" {
		snapshot.Driver = "journal"
	}
	snapshot.Digest = digest
	artifact := domain.Artifact{
		ID:                       newID("art"),
		Kind:                     recording.Kind,
		Name:                     recording.Name,
		Scope:                    recording.Scope,
		Subject:                  recording.Subject,
		Profile:                  recording.Profile,
		Provides:                 append([]string{}, recording.Provides...),
		Requires:                 append([]string{}, recording.Requires...),
		Enables:                  append([]domain.Enablement{}, recording.Enables...),
		Slot:                     recording.Slot,
		ParentArtifactIDs:        append([]string{}, recording.ParentArtifactIDs...),
		SnapshotDigest:           digest,
		Snapshot:                 snapshot,
		CompatibilityFingerprint: recording.CompatibilityFingerprint,
		Sensitivity:              recording.Sensitivity,
		CreatedBy:                recording.Actor,
		CreatedAt:                now,
	}
	recording.Status = domain.RecordingCompleted
	recording.ArtifactID = artifact.ID
	recording.EndedAt = &now
	s.state.Artifacts[artifact.ID] = artifact
	s.state.Recordings[recording.ID] = recording
	return artifact, s.saveLocked()
}

func (s *Store) CancelRecording(recordingID string, req domain.CancelRecordingRequest) (domain.Recording, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	recording, ok := s.state.Recordings[recordingID]
	if !ok {
		return domain.Recording{}, ErrNotFound
	}
	if recording.Status != domain.RecordingOpen || normalizeSubject(req.Actor) != recording.Actor {
		return domain.Recording{}, ErrConflict
	}
	now := time.Now().UTC()
	recording.Status = domain.RecordingCancelled
	recording.EndedAt = &now
	s.state.Recordings[recording.ID] = recording
	return recording, s.saveLocked()
}

func (s *Store) OpenRecording(actor string) (domain.Recording, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if recording, ok := s.openRecordingLocked(normalizeSubject(actor)); ok {
		return recording, nil
	}
	return domain.Recording{}, ErrNotFound
}

func (s *Store) Recording(recordingID string) (domain.Recording, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	recording, ok := s.state.Recordings[recordingID]
	if !ok {
		return domain.Recording{}, ErrNotFound
	}
	return recording, nil
}

func (s *Store) SetRecordingRuntime(recordingID, actor string, runtime domain.CapsuleRuntime) (domain.Recording, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	recording, ok := s.state.Recordings[recordingID]
	if !ok {
		return domain.Recording{}, ErrNotFound
	}
	if recording.Status != domain.RecordingOpen || normalizeSubject(actor) != recording.Actor {
		return domain.Recording{}, ErrConflict
	}
	recording.Runtime = &runtime
	s.state.Recordings[recording.ID] = recording
	return recording, s.saveLocked()
}

func (s *Store) Artifact(artifactID string) (domain.Artifact, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	artifact, ok := s.state.Artifacts[artifactID]
	if !ok {
		return domain.Artifact{}, ErrNotFound
	}
	return artifact, nil
}

func (s *Store) PrepareArtifactDeletion(artifactID, operator string) (domain.Artifact, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	artifact, ok := s.state.Artifacts[artifactID]
	if !ok {
		return domain.Artifact{}, ErrNotFound
	}
	operator = normalizeSubject(operator)
	if operator == "" || !canUseArtifact(operator, artifact) || (artifact.CreatedBy != "" && artifact.CreatedBy != operator) {
		return domain.Artifact{}, ErrConflict
	}
	for _, child := range s.state.Artifacts {
		if slices.Contains(child.ParentArtifactIDs, artifact.ID) {
			return domain.Artifact{}, fmt.Errorf("artifact is parent of %s:%s (%s): %w", child.Kind, child.Name, child.ID, ErrConflict)
		}
	}
	for _, recording := range s.state.Recordings {
		if recording.Status == domain.RecordingOpen && slices.Contains(recording.ParentArtifactIDs, artifact.ID) {
			return domain.Artifact{}, fmt.Errorf("artifact is used by open recording %s: %w", recording.ID, ErrConflict)
		}
	}
	for _, composition := range s.state.Compositions {
		if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
			continue
		}
		if slices.ContainsFunc(composition.ResolvedArtifacts, func(resolved domain.ResolvedArtifact) bool {
			return resolved.ArtifactID == artifact.ID
		}) {
			return domain.Artifact{}, fmt.Errorf("artifact is used by running composition %s: %w", composition.ID, ErrConflict)
		}
	}
	return artifact, nil
}

func (s *Store) DeleteArtifact(artifactID, operator string) (domain.Artifact, error) {
	artifact, err := s.PrepareArtifactDeletion(artifactID, operator)
	if err != nil {
		return domain.Artifact{}, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.state.Artifacts, artifact.ID)
	for id, recording := range s.state.Recordings {
		if recording.ArtifactID == artifact.ID {
			delete(s.state.Recordings, id)
		}
	}
	removedCompositions := map[string]bool{}
	for id, composition := range s.state.Compositions {
		if slices.ContainsFunc(composition.ResolvedArtifacts, func(resolved domain.ResolvedArtifact) bool {
			return resolved.ArtifactID == artifact.ID
		}) {
			delete(s.state.Compositions, id)
			removedCompositions[id] = true
		}
	}
	for id, session := range s.state.Sessions {
		if removedCompositions[session.PreparedCompositionID] {
			session.PreparedCompositionID = ""
			s.state.Sessions[id] = session
		}
	}
	return artifact, s.saveLocked()
}

func (s *Store) LatestArtifact(kind domain.ArtifactKind, name, actor, profile string) (domain.Artifact, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	artifact, ok := s.latestArtifactLocked(kind, normalizeName(name), normalizeSubject(actor), normalizeName(profile))
	if !ok {
		return domain.Artifact{}, ErrNotFound
	}
	return artifact, nil
}

func (s *Store) SetCompositionRuntime(compositionID, operator string, runtime domain.CapsuleRuntime) (domain.Composition, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	composition, ok := s.state.Compositions[compositionID]
	if !ok {
		return domain.Composition{}, ErrNotFound
	}
	if normalizeSubject(operator) != composition.Operator {
		return domain.Composition{}, ErrConflict
	}
	composition.Runtime = &runtime
	s.state.Compositions[composition.ID] = composition
	return composition, s.saveLocked()
}

func (s *Store) Composition(compositionID string) (domain.Composition, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	composition, ok := s.state.Compositions[compositionID]
	if !ok {
		return domain.Composition{}, ErrNotFound
	}
	return composition, nil
}

// DiscardComposition removes a composition that could not be materialized.
// A runtime-bearing composition must be stopped through the capsule engine.
func (s *Store) DiscardComposition(compositionID, operator string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	composition, ok := s.state.Compositions[compositionID]
	if !ok {
		return ErrNotFound
	}
	if composition.Operator != normalizeSubject(operator) || composition.Runtime != nil {
		return ErrConflict
	}
	delete(s.state.Compositions, composition.ID)
	if composition.SessionID != "" {
		session := s.state.Sessions[composition.SessionID]
		if session.PreparedCompositionID == composition.ID {
			session.PreparedCompositionID = ""
			session.UpdatedAt = time.Now().UTC()
			s.state.Sessions[session.ID] = session
		}
	}
	return s.saveLocked()
}

func (s *Store) Use(req domain.UseRequest) (domain.Composition, error) {
	operator := normalizeSubject(req.Operator)
	if operator == "" {
		return domain.Composition{}, fmt.Errorf("operator is required: %w", ErrConflict)
	}
	if req.Profile == "" {
		req.Profile = "default"
	}
	requestedSelector := strings.ToLower(strings.TrimSpace(req.Selector))
	if requestedSelector == "" && strings.TrimSpace(req.Tool) != "" {
		requestedSelector = "tool:" + normalizeName(req.Tool)
	}
	if requestedSelector == "" && req.SessionID != "" {
		requestedSelector = "session:" + strings.TrimSpace(req.SessionID)
	}
	if requestedSelector == "" {
		return domain.Composition{}, fmt.Errorf("selector is required (for example tool:codex or credential:codex): %w", ErrConflict)
	}
	withSelectors, err := normalizeArtifactSelectors(req.WithSelectors)
	if err != nil {
		return domain.Composition{}, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()

	var session domain.Session
	sessionID := strings.TrimSpace(req.SessionID)
	selector := requestedSelector
	if kind, name, err := parseArtifactSelector(requestedSelector); err == nil && kind == domain.ArtifactSession {
		sessionID = name
	}
	if sessionID != "" {
		var ok bool
		session, ok = s.state.Sessions[sessionID]
		if !ok {
			return domain.Composition{}, fmt.Errorf("session: %w", ErrNotFound)
		}
		if session.Status == domain.SessionClaimed || session.Status == domain.SessionRunning || session.Status == domain.SessionCompleted || session.Status == domain.SessionCancelled {
			return domain.Composition{}, fmt.Errorf("session cannot change composition while %s: %w", session.Status, ErrConflict)
		}
		selector = strings.TrimSpace(session.EnvironmentSelector)
		if selector == "" {
			selector = "tool:" + normalizeName(session.Tool)
		}
		withSelectors = append([]string{}, session.WithSelectors...)
	}
	entry, err := s.resolveArtifactSelectorLocked(selector, operator, normalizeName(req.Profile))
	if err != nil {
		return domain.Composition{}, err
	}

	composition := domain.Composition{
		ID:                   newID("cmp"),
		Operator:             operator,
		Selector:             requestedSelector,
		EntryArtifactID:      entry.ID,
		SessionID:            sessionID,
		Profile:              normalizeName(req.Profile),
		WithSelectors:        append([]string{}, withSelectors...),
		RequestedArtifactIDs: []string{entry.ID},
		ResolvedArtifacts:    []domain.ResolvedArtifact{},
		SlotBindings:         map[string]string{},
		Enabled:              []domain.Enablement{},
		MCPServerIDs:         append([]string{}, session.MCPServerIDs...),
		Warnings:             []string{},
		CreatedAt:            time.Now().UTC(),
	}
	if err := s.addArtifactLocked(&composition, entry, "selected "+selector); err != nil {
		return domain.Composition{}, err
	}
	// The entry chooses the agent/worker identity. WITH layers may contain other
	// build tools, but must not silently change worker routing.
	for i := len(composition.ResolvedArtifacts) - 1; i >= 0; i-- {
		if composition.ResolvedArtifacts[i].Kind == string(domain.ArtifactTool) {
			composition.Tool = composition.ResolvedArtifacts[i].Name
			break
		}
	}
	for _, withSelector := range withSelectors {
		artifact, err := s.resolveArtifactSelectorLocked(withSelector, operator, normalizeName(req.Profile))
		if err != nil {
			return domain.Composition{}, fmt.Errorf("WITH %s: %w", withSelector, err)
		}
		if !slices.Contains(composition.RequestedArtifactIDs, artifact.ID) {
			composition.RequestedArtifactIDs = append(composition.RequestedArtifactIDs, artifact.ID)
		}
		if err := s.addArtifactLocked(&composition, artifact, "WITH "+withSelector); err != nil {
			return domain.Composition{}, err
		}
	}
	if err := validateRequirements(composition, s.state.Artifacts); err != nil {
		return domain.Composition{}, err
	}
	if sessionID != "" {
		if err := requireEnabledCapabilities(composition, "git", "acp"); err != nil {
			return domain.Composition{}, err
		}
		job, ok := s.state.Jobs[session.JobID]
		if !ok || job.GitRepositoryID == "" || session.GitRepositoryID != job.GitRepositoryID {
			return domain.Composition{}, fmt.Errorf("Session has no valid Git repository binding: %w", ErrConflict)
		}
		repository, ok := s.state.GitRepositories[job.GitRepositoryID]
		if !ok {
			return domain.Composition{}, fmt.Errorf("Git repository: %w", ErrNotFound)
		}
		repositoryName, remoteURL, provider, credentialScope := job.GitRepositoryName, job.GitRemoteURL, job.GitProvider, job.GitCredentialScope
		if repositoryName == "" {
			repositoryName = repository.Name
		}
		if remoteURL == "" {
			remoteURL = repository.RemoteURL
		}
		if provider == "" {
			provider = repository.Provider
		}
		if credentialScope == "" {
			credentialScope = repository.CredentialScope
		}
		workspace := domain.GitWorkspace{
			RepositoryID: repository.ID, RepositoryName: repositoryName, RemoteURL: remoteURL,
			BaseRef: job.Branch, BootstrapRef: job.BaseRef, HeadRef: session.GitRef, TargetRef: job.Branch,
			CredentialScope: credentialScope, Provider: provider,
		}
		if workspace.BootstrapRef == "" {
			workspace.BootstrapRef = repository.DefaultRef
		}
		if credentialScope != domain.CredentialScopePublic {
			account, err := s.resolveGitAccountForLocked(repositoryName, remoteURL, provider, credentialScope, operator)
			if err != nil {
				return domain.Composition{}, err
			}
			workspace.Provider = account.Provider
			workspace.Login = account.Login
			workspace.AuthorName = account.Name
			workspace.AuthorEmail = account.Email
		}
		composition.Git = &workspace
	}
	s.state.Compositions[composition.ID] = composition
	if sessionID != "" {
		session.Operator = operator
		session.PreparedCompositionID = composition.ID
		session.UpdatedAt = composition.CreatedAt
		s.state.Sessions[session.ID] = session
	}
	return composition, s.saveLocked()
}

func (s *Store) CreateMCPServer(req domain.CreateMCPServerRequest) (domain.MCPServer, error) {
	operator := normalizeSubject(req.Operator)
	name := strings.TrimSpace(req.Name)
	if operator == "" || name == "" {
		return domain.MCPServer{}, fmt.Errorf("operator and MCP name are required: %w", ErrConflict)
	}
	if req.Transport == "" {
		req.Transport = domain.MCPTransportStdio
	}
	switch req.Transport {
	case domain.MCPTransportStdio:
		if !strings.HasPrefix(strings.TrimSpace(req.Command), "/") {
			return domain.MCPServer{}, fmt.Errorf("ACP requires an absolute MCP stdio command: %w", ErrConflict)
		}
	case domain.MCPTransportHTTP:
		url := strings.ToLower(strings.TrimSpace(req.URL))
		if !strings.HasPrefix(url, "http://") && !strings.HasPrefix(url, "https://") {
			return domain.MCPServer{}, fmt.Errorf("MCP HTTP URL must start with http:// or https://: %w", ErrConflict)
		}
	default:
		return domain.MCPServer{}, fmt.Errorf("unsupported MCP transport %q: %w", req.Transport, ErrConflict)
	}
	env, err := normalizeMCPSecrets(req.Env)
	if err != nil {
		return domain.MCPServer{}, err
	}
	headers, err := normalizeMCPSecrets(req.Headers)
	if err != nil {
		return domain.MCPServer{}, err
	}
	server := domain.MCPServer{
		ID:        newID("mcp"),
		Operator:  operator,
		Name:      name,
		Transport: req.Transport,
		Command:   strings.TrimSpace(req.Command),
		// Argument order and duplicates can be meaningful to an MCP command.
		Args:      append([]string(nil), req.Args...),
		URL:       strings.TrimSpace(req.URL),
		Env:       env,
		Headers:   headers,
		CreatedAt: time.Now().UTC(),
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, existing := range s.state.MCPServers {
		if existing.Operator == operator && strings.EqualFold(existing.Name, name) {
			return domain.MCPServer{}, fmt.Errorf("MCP server %q already exists for %s: %w", name, operator, ErrConflict)
		}
	}
	s.state.MCPServers[server.ID] = server
	return redactMCPServer(server), s.saveLocked()
}

func (s *Store) DeleteMCPServer(id, operator string) (domain.MCPServer, error) {
	operator = normalizeSubject(operator)
	s.mu.Lock()
	defer s.mu.Unlock()
	server, ok := s.state.MCPServers[id]
	if !ok {
		return domain.MCPServer{}, ErrNotFound
	}
	if operator == "" || server.Operator != operator {
		return domain.MCPServer{}, ErrConflict
	}
	for _, composition := range s.state.Compositions {
		if slices.Contains(composition.MCPServerIDs, id) && composition.Runtime != nil && composition.Runtime.Status != "stopped" {
			return domain.MCPServer{}, fmt.Errorf("MCP server is used by running composition %s: %w", composition.ID, ErrConflict)
		}
	}
	delete(s.state.MCPServers, id)
	for jobID, job := range s.state.Jobs {
		job.MCPServerIDs = withoutString(job.MCPServerIDs, id)
		s.state.Jobs[jobID] = job
	}
	for sessionID, session := range s.state.Sessions {
		session.MCPServerIDs = withoutString(session.MCPServerIDs, id)
		s.state.Sessions[sessionID] = session
	}
	return redactMCPServer(server), s.saveLocked()
}

func (s *Store) MCPServersForOperator(operator string, ids []string) ([]domain.MCPServer, error) {
	operator = normalizeSubject(operator)
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.mcpServersLocked(operator, ids)
}

func (s *Store) HasUsers() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.state.Users) != 0
}

func (s *Store) CreateInitialUser(user domain.User) (domain.PublicUser, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(s.state.Users) != 0 {
		return domain.PublicUser{}, fmt.Errorf("owner setup is already complete: %w", ErrConflict)
	}
	user.Username = normalizeSubject(user.Username)
	user.DisplayName = strings.TrimSpace(user.DisplayName)
	if user.DisplayName == "" {
		user.DisplayName = user.Username
	}
	if user.Username == "" || user.PasswordHash == "" {
		return domain.PublicUser{}, fmt.Errorf("username and password hash are required: %w", ErrConflict)
	}
	now := time.Now().UTC()
	user.ID = newID("usr")
	user.Role = domain.UserAdmin
	user.CreatedAt, user.UpdatedAt = now, now
	s.state.Users[user.ID] = user
	return publicUser(user), s.saveLocked()
}

func (s *Store) CreateUser(actorID string, user domain.User) (domain.PublicUser, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, err := s.requireAdminLocked(actorID); err != nil {
		return domain.PublicUser{}, err
	}
	user.Username = normalizeSubject(user.Username)
	user.DisplayName = strings.TrimSpace(user.DisplayName)
	if user.DisplayName == "" {
		user.DisplayName = user.Username
	}
	if user.Role == "" {
		user.Role = domain.UserMember
	}
	if user.Username == "" || user.PasswordHash == "" || (user.Role != domain.UserAdmin && user.Role != domain.UserMember) {
		return domain.PublicUser{}, fmt.Errorf("valid username, password hash and role are required: %w", ErrConflict)
	}
	for _, existing := range s.state.Users {
		if existing.Username == user.Username {
			return domain.PublicUser{}, fmt.Errorf("user %s already exists: %w", user.Username, ErrConflict)
		}
	}
	now := time.Now().UTC()
	user.ID = newID("usr")
	user.CreatedAt, user.UpdatedAt = now, now
	s.state.Users[user.ID] = user
	return publicUser(user), s.saveLocked()
}

// SetUserArchived disables or restores an identity without deleting any
// records that refer to it. Archiving also revokes every login session owned
// by the user in the same persisted transition.
func (s *Store) SetUserArchived(actorID, userID string, archived bool) (domain.PublicUser, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	actor, err := s.requireAdminLocked(actorID)
	if err != nil {
		return domain.PublicUser{}, err
	}
	user, ok := s.state.Users[strings.TrimSpace(userID)]
	if !ok {
		return domain.PublicUser{}, ErrNotFound
	}
	if archived && user.ID == actor.ID {
		return domain.PublicUser{}, fmt.Errorf("an admin cannot archive their own active identity: %w", ErrConflict)
	}
	if archived && user.ArchivedAt == nil && user.Role == domain.UserAdmin {
		activeAdmins := 0
		for _, candidate := range s.state.Users {
			if candidate.Role == domain.UserAdmin && candidate.ArchivedAt == nil {
				activeAdmins++
			}
		}
		if activeAdmins <= 1 {
			return domain.PublicUser{}, fmt.Errorf("the last active admin cannot be archived: %w", ErrConflict)
		}
	}
	now := time.Now().UTC()
	if archived {
		if user.ArchivedAt == nil {
			user.ArchivedAt = &now
		}
		for id, session := range s.state.AuthSessions {
			if session.UserID == user.ID {
				delete(s.state.AuthSessions, id)
			}
		}
	} else {
		user.ArchivedAt = nil
	}
	user.UpdatedAt = now
	s.state.Users[user.ID] = user
	return publicUser(user), s.saveLocked()
}

func (s *Store) UserByUsername(username string) (domain.User, error) {
	username = normalizeSubject(username)
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, user := range s.state.Users {
		if user.Username == username {
			return user, nil
		}
	}
	return domain.User{}, ErrNotFound
}

func (s *Store) User(userID string) (domain.User, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	user, ok := s.state.Users[strings.TrimSpace(userID)]
	if !ok {
		return domain.User{}, ErrNotFound
	}
	return user, nil
}

func (s *Store) CreateAuthSession(userID, tokenHash, csrfHash string, expiresAt time.Time) (domain.AuthSession, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	user, ok := s.state.Users[userID]
	if !ok {
		return domain.AuthSession{}, ErrNotFound
	}
	if user.ArchivedAt != nil {
		return domain.AuthSession{}, ErrConflict
	}
	if tokenHash == "" || csrfHash == "" || !expiresAt.After(time.Now()) {
		return domain.AuthSession{}, fmt.Errorf("valid session hashes and expiry are required: %w", ErrConflict)
	}
	now := time.Now().UTC()
	session := domain.AuthSession{ID: newID("aus"), UserID: userID, TokenHash: tokenHash, CSRFHash: csrfHash, ExpiresAt: expiresAt.UTC(), CreatedAt: now, LastSeenAt: now}
	s.state.AuthSessions[session.ID] = session
	return session, s.saveLocked()
}

func (s *Store) AuthenticateSession(tokenHash string) (domain.User, domain.AuthSession, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	now := time.Now().UTC()
	changed := false
	for id, session := range s.state.AuthSessions {
		if !session.ExpiresAt.After(now) {
			delete(s.state.AuthSessions, id)
			changed = true
			continue
		}
		if session.TokenHash != tokenHash {
			continue
		}
		user, ok := s.state.Users[session.UserID]
		if !ok || user.ArchivedAt != nil {
			delete(s.state.AuthSessions, id)
			_ = s.saveLocked()
			return domain.User{}, domain.AuthSession{}, ErrNotFound
		}
		if now.Sub(session.LastSeenAt) > 5*time.Minute {
			session.LastSeenAt = now
			s.state.AuthSessions[id] = session
			changed = true
		}
		if changed {
			_ = s.saveLocked()
		}
		return user, session, nil
	}
	if changed {
		_ = s.saveLocked()
	}
	return domain.User{}, domain.AuthSession{}, ErrNotFound
}

func (s *Store) DeleteAuthSession(sessionID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, ok := s.state.AuthSessions[sessionID]; !ok {
		return ErrNotFound
	}
	delete(s.state.AuthSessions, sessionID)
	return s.saveLocked()
}

func (s *Store) SaveGitOAuthConfiguration(actorID string, req domain.SaveGitOAuthConfigurationRequest) (domain.GitOAuthConfiguration, error) {
	provider := normalizeName(req.Provider)
	clientID := strings.TrimSpace(req.ClientID)
	clientSecret := strings.TrimSpace(req.ClientSecret)
	if (provider != "github" && provider != "gitlab") || clientID == "" || clientSecret == "" || strings.ContainsAny(clientID+clientSecret, "\r\n") {
		return domain.GitOAuthConfiguration{}, fmt.Errorf("github/gitlab provider, client ID and client secret are required: %w", ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	actor, err := s.requireAdminLocked(actorID)
	if err != nil {
		return domain.GitOAuthConfiguration{}, err
	}
	now := time.Now().UTC()
	configuration := s.state.GitOAuthConfigurations[provider]
	if configuration.CreatedAt.IsZero() {
		configuration.CreatedAt = now
		configuration.CreatedBy = actor.Username
	}
	configuration.Provider = provider
	configuration.ClientID = clientID
	configuration.ClientSecret = clientSecret
	configuration.UpdatedAt = now
	s.state.GitOAuthConfigurations[provider] = configuration
	configuration.ClientSecret = ""
	return configuration, s.saveLocked()
}

func (s *Store) GitOAuthConfiguration(provider string) (domain.GitOAuthConfiguration, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	configuration, ok := s.state.GitOAuthConfigurations[normalizeName(provider)]
	if !ok {
		return domain.GitOAuthConfiguration{}, ErrNotFound
	}
	return configuration, nil
}

func (s *Store) DeleteGitOAuthConfiguration(actorID, provider string) (domain.GitOAuthConfiguration, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, err := s.requireAdminLocked(actorID); err != nil {
		return domain.GitOAuthConfiguration{}, err
	}
	provider = normalizeName(provider)
	configuration, ok := s.state.GitOAuthConfigurations[provider]
	if !ok {
		return domain.GitOAuthConfiguration{}, ErrNotFound
	}
	delete(s.state.GitOAuthConfigurations, provider)
	configuration.ClientSecret = ""
	return configuration, s.saveLocked()
}

func (s *Store) GitOAuthConfigurations() []domain.GitOAuthConfiguration {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]domain.GitOAuthConfiguration, 0, len(s.state.GitOAuthConfigurations))
	for _, configuration := range s.state.GitOAuthConfigurations {
		configuration.ClientSecret = ""
		out = append(out, configuration)
	}
	slices.SortFunc(out, func(a, b domain.GitOAuthConfiguration) int { return strings.Compare(a.Provider, b.Provider) })
	return out
}

func (s *Store) requireAdminLocked(userID string) (domain.User, error) {
	user, ok := s.state.Users[strings.TrimSpace(userID)]
	if !ok {
		return domain.User{}, ErrNotFound
	}
	if user.Role != domain.UserAdmin || user.ArchivedAt != nil {
		return domain.User{}, ErrConflict
	}
	return user, nil
}

func publicUser(user domain.User) domain.PublicUser {
	return domain.PublicUser{ID: user.ID, Username: user.Username, DisplayName: user.DisplayName, Role: user.Role, ArchivedAt: user.ArchivedAt, CreatedAt: user.CreatedAt}
}

func (s *Store) CreateGitRepository(req domain.CreateGitRepositoryRequest) (domain.CreateGitRepositoryResponse, error) {
	operator := normalizeSubject(req.Operator)
	name := strings.TrimSpace(req.Name)
	remoteURL := strings.TrimSpace(req.RemoteURL)
	credentialScope := normalizeGitCredentialScope(req.CredentialScope, domain.CredentialScopePublic)
	defaultRef := strings.TrimSpace(req.DefaultRef)
	if defaultRef == "" {
		defaultRef = "main"
	}
	provider, _ := gitRemoteIdentity(remoteURL)
	if operator == "" || name == "" || !validGitRemote(remoteURL) || !validGitBaseRef(defaultRef) || !validGitCredentialScope(credentialScope) {
		return domain.CreateGitRepositoryResponse{}, fmt.Errorf("operator, name, a credential-free Git remote and a valid default ref are required: %w", ErrConflict)
	}
	layerSelectors, err := normalizeArtifactSelectors(req.LayerSelectors)
	if err != nil {
		return domain.CreateGitRepositoryResponse{}, fmt.Errorf("repository layers: %w", err)
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	if err := s.validateProjectLayerSelectorsLocked(operator, layerSelectors, "default"); err != nil {
		return domain.CreateGitRepositoryResponse{}, err
	}
	for _, existing := range s.state.GitRepositories {
		if strings.EqualFold(existing.Name, name) || existing.RemoteURL == remoteURL {
			return domain.CreateGitRepositoryResponse{}, fmt.Errorf("Git repository %q already exists: %w", name, ErrConflict)
		}
	}
	now := time.Now().UTC()
	repository := domain.GitRepository{
		ID: newID("git"), Name: name, RemoteURL: remoteURL, DefaultRef: defaultRef,
		Provider: provider, CredentialScope: credentialScope, LayerSelectors: layerSelectors,
		CreatedBy: operator, CreatedAt: now, UpdatedAt: now,
	}
	s.state.GitRepositories[repository.ID] = repository
	return domain.CreateGitRepositoryResponse{Repository: repository}, s.saveLocked()
}

func (s *Store) UpdateGitRepository(repositoryID string, req domain.UpdateGitRepositoryRequest) (domain.GitRepository, error) {
	operator := normalizeSubject(req.Operator)
	name := strings.TrimSpace(req.Name)
	remoteURL := strings.TrimSpace(req.RemoteURL)
	defaultRef := strings.TrimSpace(req.DefaultRef)
	credentialScope := normalizeGitCredentialScope(req.CredentialScope, "")
	if credentialScope != "" && !validGitCredentialScope(credentialScope) {
		return domain.GitRepository{}, fmt.Errorf("repository credential scope: %w", ErrConflict)
	}
	layerSelectors, err := normalizeArtifactSelectors(req.LayerSelectors)
	if err != nil {
		return domain.GitRepository{}, fmt.Errorf("repository layers: %w", err)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	repository, ok := s.state.GitRepositories[strings.TrimSpace(repositoryID)]
	if !ok {
		return domain.GitRepository{}, ErrNotFound
	}
	if operator == "" || repository.CreatedBy != operator {
		return domain.GitRepository{}, ErrConflict
	}
	if name == "" {
		name = repository.Name
	}
	if remoteURL == "" {
		remoteURL = repository.RemoteURL
	}
	if defaultRef == "" {
		defaultRef = repository.DefaultRef
	}
	if name == "" || !validGitRemote(remoteURL) || !validGitBaseRef(defaultRef) {
		return domain.GitRepository{}, fmt.Errorf("name, a credential-free Git remote and a valid default ref are required: %w", ErrConflict)
	}
	for _, existing := range s.state.GitRepositories {
		if existing.ID != repository.ID && (strings.EqualFold(existing.Name, name) || existing.RemoteURL == remoteURL) {
			return domain.GitRepository{}, fmt.Errorf("Git repository %q already exists: %w", name, ErrConflict)
		}
	}
	if err := s.validateProjectLayerSelectorsLocked(operator, layerSelectors, "default"); err != nil {
		return domain.GitRepository{}, err
	}
	repository.Name = name
	repository.RemoteURL = remoteURL
	repository.DefaultRef = defaultRef
	repository.Provider, _ = gitRemoteIdentity(remoteURL)
	repository.LayerSelectors = append([]string(nil), layerSelectors...)
	if credentialScope != "" {
		repository.CredentialScope = credentialScope
	}
	repository.UpdatedAt = time.Now().UTC()
	s.state.GitRepositories[repository.ID] = repository
	return repository, s.saveLocked()
}

func (s *Store) SaveGitAccount(account domain.GitAccount) (domain.GitAccount, error) {
	account.Operator = normalizeSubject(account.Operator)
	account.Provider = normalizeName(account.Provider)
	account.Host = strings.ToLower(strings.TrimSpace(account.Host))
	account.ProviderID = strings.TrimSpace(account.ProviderID)
	account.Login = strings.TrimSpace(account.Login)
	account.Name = strings.TrimSpace(account.Name)
	account.Email = strings.TrimSpace(account.Email)
	account.AccessToken = strings.TrimSpace(account.AccessToken)
	account.RefreshToken = strings.TrimSpace(account.RefreshToken)
	account.TokenType = strings.TrimSpace(account.TokenType)
	account.Scope = strings.TrimSpace(account.Scope)
	account.CredentialScope = normalizeGitCredentialScope(account.CredentialScope, domain.CredentialScopeUser)
	if account.Host == "" {
		switch account.Provider {
		case "github":
			account.Host = "github.com"
		case "gitlab":
			account.Host = "gitlab.com"
		}
	}
	if account.Name == "" {
		account.Name = account.Login
	}
	if account.Operator == "" || account.Provider == "" || account.Host == "" || account.Login == "" || account.AccessToken == "" || (account.CredentialScope != domain.CredentialScopeUser && account.CredentialScope != domain.CredentialScopeGlobal) {
		return domain.GitAccount{}, fmt.Errorf("operator, provider, host, login and access token are required: %w", ErrConflict)
	}
	if strings.ContainsAny(account.Host, "/@?#\r\n\t ") {
		return domain.GitAccount{}, fmt.Errorf("Git account host must be a hostname with an optional port: %w", ErrConflict)
	}
	for _, value := range []string{account.AccessToken, account.RefreshToken, account.Login, account.Name, account.Email} {
		if strings.ContainsAny(value, "\r\n") {
			return domain.GitAccount{}, fmt.Errorf("Git account values cannot contain newlines: %w", ErrConflict)
		}
	}
	now := time.Now().UTC()
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, existing := range s.state.GitAccounts {
		sameOwner := account.CredentialScope == domain.CredentialScopeGlobal || existing.Operator == account.Operator
		if existing.CredentialScope == account.CredentialScope && sameOwner && existing.Provider == account.Provider && existing.Host == account.Host {
			account.ID = existing.ID
			account.CreatedAt = existing.CreatedAt
			account.UpdatedAt = now
			s.state.GitAccounts[account.ID] = account
			return redactGitAccount(account), s.saveLocked()
		}
	}
	account.ID = newID("gac")
	account.CreatedAt = now
	account.UpdatedAt = now
	s.state.GitAccounts[account.ID] = account
	return redactGitAccount(account), s.saveLocked()
}

func (s *Store) CreateGitAccount(req domain.CreateGitAccountRequest) (domain.GitAccount, error) {
	return s.SaveGitAccount(domain.GitAccount{
		Operator: req.Operator, Provider: req.Provider, Host: req.Host, ProviderID: req.ProviderID,
		Login: req.Login, Name: req.Name, Email: req.Email, AccessToken: req.AccessToken,
		CredentialScope: req.CredentialScope,
	})
}

func (s *Store) GitAccount(accountID, operator string) (domain.GitAccount, error) {
	operator = normalizeSubject(operator)
	s.mu.Lock()
	defer s.mu.Unlock()
	account, ok := s.state.GitAccounts[strings.TrimSpace(accountID)]
	if !ok {
		return domain.GitAccount{}, ErrNotFound
	}
	if operator == "" || (account.CredentialScope != domain.CredentialScopeGlobal && account.Operator != operator) {
		return domain.GitAccount{}, ErrConflict
	}
	return account, nil
}

// ResolveGitAccount selects an identity from repository metadata at action
// time. Repositories never bind an account ID: user scope follows the current
// operator, while global scope selects the shared identity for the remote host.
func (s *Store) ResolveGitAccount(repositoryID, operator string) (domain.GitAccount, error) {
	operator = normalizeSubject(operator)
	s.mu.Lock()
	defer s.mu.Unlock()
	repository, ok := s.state.GitRepositories[strings.TrimSpace(repositoryID)]
	if !ok {
		return domain.GitAccount{}, fmt.Errorf("Git repository: %w", ErrNotFound)
	}
	return s.resolveGitAccountLocked(repository, operator)
}

func (s *Store) resolveGitAccountLocked(repository domain.GitRepository, operator string) (domain.GitAccount, error) {
	return s.resolveGitAccountForLocked(repository.Name, repository.RemoteURL, repository.Provider, repository.CredentialScope, operator)
}

func (s *Store) ResolveGitWorkspaceAccount(workspace domain.GitWorkspace, operator string) (domain.GitAccount, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.resolveGitAccountForLocked(workspace.RepositoryName, workspace.RemoteURL, workspace.Provider, workspace.CredentialScope, operator)
}

func (s *Store) resolveGitAccountForLocked(name, remoteURL, provider string, credentialScope domain.CredentialScope, operator string) (domain.GitAccount, error) {
	if credentialScope == domain.CredentialScopePublic {
		return domain.GitAccount{}, fmt.Errorf("repository %s uses public Git access: %w", name, ErrNotFound)
	}
	_, host := gitRemoteIdentity(remoteURL)
	operator = normalizeSubject(operator)
	var selected *domain.GitAccount
	for _, candidate := range s.state.GitAccounts {
		if candidate.CredentialScope != credentialScope || !strings.EqualFold(candidate.Host, host) {
			continue
		}
		if (provider == "github" || provider == "gitlab") && candidate.Provider != provider {
			continue
		}
		if credentialScope == domain.CredentialScopeUser && candidate.Operator != operator {
			continue
		}
		candidateCopy := candidate
		if selected == nil || candidateCopy.UpdatedAt.After(selected.UpdatedAt) {
			selected = &candidateCopy
		}
	}
	if selected == nil {
		identity := "a global account"
		if credentialScope == domain.CredentialScopeUser {
			identity = "the account of user " + operator
		}
		return domain.GitAccount{}, fmt.Errorf("repository %s needs %s for %s: %w", name, identity, host, ErrNotFound)
	}
	return *selected, nil
}

func (s *Store) DeleteGitAccount(accountID, operator string) (domain.GitAccount, error) {
	operator = normalizeSubject(operator)
	s.mu.Lock()
	defer s.mu.Unlock()
	account, ok := s.state.GitAccounts[strings.TrimSpace(accountID)]
	if !ok {
		return domain.GitAccount{}, ErrNotFound
	}
	if operator == "" || account.Operator != operator {
		return domain.GitAccount{}, ErrConflict
	}
	for _, composition := range s.state.Compositions {
		if composition.Git != nil && composition.Git.AccountID == account.ID && composition.Runtime != nil && composition.Runtime.Status != "stopped" {
			return domain.GitAccount{}, fmt.Errorf("Git account is used by running composition %s: %w", composition.ID, ErrConflict)
		}
	}
	delete(s.state.GitAccounts, account.ID)
	return redactGitAccount(account), s.saveLocked()
}

func (s *Store) DeleteGitRepository(repositoryID, operator string) (domain.GitRepository, error) {
	operator = normalizeSubject(operator)
	s.mu.Lock()
	defer s.mu.Unlock()
	repository, ok := s.state.GitRepositories[repositoryID]
	if !ok {
		return domain.GitRepository{}, ErrNotFound
	}
	if operator == "" || repository.CreatedBy != operator {
		return domain.GitRepository{}, ErrConflict
	}
	for _, job := range s.state.Jobs {
		if job.GitRepositoryID == repository.ID {
			return domain.GitRepository{}, fmt.Errorf("Git repository is used by job %s: %w", job.ID, ErrConflict)
		}
	}
	delete(s.state.GitRepositories, repository.ID)
	return repository, s.saveLocked()
}

func (s *Store) CreateJobAttachment(req domain.CreateJobAttachmentRequest) (domain.JobAttachment, error) {
	operator := normalizeSubject(req.Operator)
	attachment := domain.JobAttachment{
		ID: strings.TrimSpace(req.ID), JobID: strings.TrimSpace(req.JobID), Name: strings.TrimSpace(req.Name),
		MediaType: strings.TrimSpace(req.MediaType), Size: req.Size, SHA256: strings.TrimSpace(req.SHA256),
		CapsulePath: strings.TrimSpace(req.CapsulePath), CreatedBy: operator, CreatedAt: time.Now().UTC(),
	}
	if operator == "" || !strings.HasPrefix(attachment.ID, "att_") || attachment.Name == "" || len(attachment.Name) > 180 || attachment.MediaType == "" || attachment.Size < 1 || attachment.Size > 15<<20 || len(attachment.SHA256) != 64 || !strings.HasPrefix(attachment.CapsulePath, "/spin/job-attachments/") {
		return domain.JobAttachment{}, fmt.Errorf("invalid Job attachment metadata: %w", ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, exists := s.state.JobAttachments[attachment.ID]; exists {
		return domain.JobAttachment{}, ErrConflict
	}
	if attachment.JobID != "" {
		job, exists := s.state.Jobs[attachment.JobID]
		if !exists {
			return domain.JobAttachment{}, ErrNotFound
		}
		if job.Owner != operator || job.Status == domain.JobDone || job.Status == domain.JobCancelled || len(job.AttachmentIDs) >= 8 {
			return domain.JobAttachment{}, fmt.Errorf("Job cannot accept this attachment: %w", ErrConflict)
		}
		var total int64
		for _, attachmentID := range job.AttachmentIDs {
			total += s.state.JobAttachments[attachmentID].Size
		}
		if total+attachment.Size > 40<<20 {
			return domain.JobAttachment{}, fmt.Errorf("Job attachments may be at most 40 MiB in total: %w", ErrConflict)
		}
		job.AttachmentIDs = append(job.AttachmentIDs, attachment.ID)
		job.UpdatedAt = attachment.CreatedAt
		s.state.Jobs[job.ID] = job
	}
	s.state.JobAttachments[attachment.ID] = attachment
	if err := s.saveLocked(); err != nil {
		return domain.JobAttachment{}, err
	}
	return attachment, nil
}

func (s *Store) JobAttachment(attachmentID, operator string) (domain.JobAttachment, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	attachment, ok := s.state.JobAttachments[strings.TrimSpace(attachmentID)]
	if !ok {
		return domain.JobAttachment{}, ErrNotFound
	}
	operator = normalizeSubject(operator)
	if operator == "" || (attachment.JobID == "" && attachment.CreatedBy != operator) {
		return domain.JobAttachment{}, ErrConflict
	}
	if attachment.JobID != "" {
		if _, exists := s.state.Jobs[attachment.JobID]; !exists {
			return domain.JobAttachment{}, ErrNotFound
		}
	}
	return attachment, nil
}

func (s *Store) JobAttachments(jobID string) []domain.JobAttachment {
	s.mu.Lock()
	defer s.mu.Unlock()
	attachments := make([]domain.JobAttachment, 0)
	for _, attachment := range s.state.JobAttachments {
		if attachment.JobID == strings.TrimSpace(jobID) {
			attachments = append(attachments, attachment)
		}
	}
	slices.SortFunc(attachments, func(a, b domain.JobAttachment) int { return a.CreatedAt.Compare(b.CreatedAt) })
	return attachments
}

func (s *Store) DeleteStagedJobAttachment(attachmentID, operator string) (domain.JobAttachment, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	attachment, ok := s.state.JobAttachments[strings.TrimSpace(attachmentID)]
	if !ok {
		return domain.JobAttachment{}, ErrNotFound
	}
	if attachment.JobID != "" || attachment.CreatedBy != normalizeSubject(operator) {
		return domain.JobAttachment{}, ErrConflict
	}
	delete(s.state.JobAttachments, attachment.ID)
	return attachment, s.saveLocked()
}

func (s *Store) CreateJob(req domain.CreateJobRequest) (domain.CreateJobResponse, error) {
	selector := strings.ToLower(strings.TrimSpace(req.EnvironmentSelector))
	if selector == "" && strings.TrimSpace(req.Tool) != "" {
		selector = "tool:" + normalizeName(req.Tool)
	}
	_, selectorName, selectorErr := parseArtifactSelector(selector)
	if strings.TrimSpace(req.Title) == "" || strings.TrimSpace(req.Objective) == "" || (strings.TrimSpace(req.GitRepositoryID) == "" && strings.TrimSpace(req.ForkedFromJobID) == "") || selectorErr != nil {
		return domain.CreateJobResponse{}, fmt.Errorf("title, objective, git_repository_id and a valid environment_selector are required: %w", ErrConflict)
	}
	requestedWith, err := normalizeArtifactSelectors(req.WithSelectors)
	if err != nil {
		return domain.CreateJobResponse{}, err
	}
	tool := normalizeName(req.Tool)
	if tool == "" {
		tool = selectorName
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	operator := normalizeSubject(req.Operator)
	if operator == "" {
		operator = normalizeSubject(req.Owner)
	}
	owner := normalizeSubject(req.Owner)
	if owner == "" {
		owner = operator
	}
	if operator == "" {
		return domain.CreateJobResponse{}, fmt.Errorf("operator is required: %w", ErrConflict)
	}
	forkedFromJobID := strings.TrimSpace(req.ForkedFromJobID)
	if forkedFromJobID != "" {
		source, exists := s.state.Jobs[forkedFromJobID]
		if !exists {
			return domain.CreateJobResponse{}, fmt.Errorf("fork source Job: %w", ErrNotFound)
		}
		if source.Status != domain.JobDone && source.Status != domain.JobCancelled {
			return domain.CreateJobResponse{}, fmt.Errorf("only a closed Job can be forked: %w", ErrConflict)
		}
		if strings.TrimSpace(source.Branch) == "" {
			return domain.CreateJobResponse{}, fmt.Errorf("fork source Job has no remote branch: %w", ErrConflict)
		}
		req.GitRepositoryID = source.GitRepositoryID
		req.BaseRef = source.Branch
		owner = operator
	}
	idempotencyKey := strings.TrimSpace(req.IdempotencyKey)
	if len(idempotencyKey) > 128 {
		return domain.CreateJobResponse{}, fmt.Errorf("idempotency_key may be at most 128 characters: %w", ErrConflict)
	}
	requestKey := ""
	if idempotencyKey != "" {
		requestKey = operator + "\x00" + idempotencyKey
		if existingJobID, exists := s.state.JobRequestKeys[requestKey]; exists {
			existingJob, jobExists := s.state.Jobs[existingJobID]
			if !jobExists || len(existingJob.SessionIDs) == 0 {
				return domain.CreateJobResponse{}, fmt.Errorf("idempotent Job creation points to incomplete state: %w", ErrConflict)
			}
			existingSession, sessionExists := s.state.Sessions[existingJob.SessionIDs[0]]
			if !sessionExists {
				return domain.CreateJobResponse{}, fmt.Errorf("idempotent Job creation has no root Session: %w", ErrConflict)
			}
			return domain.CreateJobResponse{Job: existingJob, Session: existingSession, Replayed: true}, nil
		}
	}
	repository, ok := s.state.GitRepositories[strings.TrimSpace(req.GitRepositoryID)]
	if !ok {
		return domain.CreateJobResponse{}, fmt.Errorf("Git repository: %w", ErrNotFound)
	}
	if repository.CredentialScope != domain.CredentialScopePublic {
		if _, err := s.resolveGitAccountLocked(repository, owner); err != nil {
			return domain.CreateJobResponse{}, fmt.Errorf("Git identity: %w", err)
		}
	}
	if err := s.validateProjectLayerSelectorsLocked(operator, repository.LayerSelectors, "default"); err != nil {
		return domain.CreateJobResponse{}, fmt.Errorf("Git repository %s: %w", repository.Name, err)
	}
	attachmentIDs := uniqueStrings(req.AttachmentIDs)
	if len(attachmentIDs) > 8 {
		return domain.CreateJobResponse{}, fmt.Errorf("a Job may contain at most 8 attachments: %w", ErrConflict)
	}
	var attachmentBytes int64
	for _, attachmentID := range attachmentIDs {
		attachment, exists := s.state.JobAttachments[attachmentID]
		if !exists || attachment.JobID != "" || attachment.CreatedBy != operator {
			return domain.CreateJobResponse{}, fmt.Errorf("staged Job attachment %s: %w", attachmentID, ErrNotFound)
		}
		attachmentBytes += attachment.Size
	}
	if attachmentBytes > 40<<20 {
		return domain.CreateJobResponse{}, fmt.Errorf("Job attachments may be at most 40 MiB in total: %w", ErrConflict)
	}
	mcpIDs := uniqueStrings(req.MCPServerIDs)
	if _, err := s.mcpServersLocked(operator, mcpIDs); err != nil {
		return domain.CreateJobResponse{}, err
	}
	var workflowTemplate domain.WorkflowTemplate
	if strings.TrimSpace(req.TemplateID) != "" {
		var ok bool
		workflowTemplate, ok = s.state.WorkflowTemplates[strings.TrimSpace(req.TemplateID)]
		if !ok || len(workflowTemplate.Phases) == 0 {
			return domain.CreateJobResponse{}, fmt.Errorf("workflow template: %w", ErrNotFound)
		}
	}
	withSelectors := make([]string, 0, 1+len(repository.LayerSelectors)+len(requestedWith))
	if workflowTemplate.GitSelector != "" {
		withSelectors = append(withSelectors, workflowTemplate.GitSelector)
	}
	withSelectors = append(withSelectors, repository.LayerSelectors...)
	withSelectors = append(withSelectors, requestedWith...)
	withSelectors = uniqueStrings(withSelectors)
	if err := s.validateSessionEnvironmentLocked(operator, selector, withSelectors, "default"); err != nil {
		return domain.CreateJobResponse{}, err
	}
	if workflowTemplate.ID != "" {
		for _, phase := range workflowTemplate.Phases {
			if phase.Executor == domain.WorkflowExecutorAction {
				continue
			}
			phaseSelector, phaseWith := workflowPhaseEnvironment(phase, selector, withSelectors)
			if err := s.validateSessionEnvironmentLocked(operator, phaseSelector, phaseWith, "default"); err != nil {
				return domain.CreateJobResponse{}, fmt.Errorf("phase %s environment: %w", phase.Name, err)
			}
		}
	}

	now := time.Now().UTC()
	jobID := newID("job")
	sessionID := newID("ses")
	baseRef := strings.TrimSpace(req.BaseRef)
	if baseRef == "" {
		baseRef = repository.DefaultRef
	}
	if !validGitBaseRef(baseRef) {
		return domain.CreateJobResponse{}, fmt.Errorf("invalid Git base ref %q: %w", baseRef, ErrConflict)
	}
	namespace := "jobs/" + gitSlug(req.Title) + "-" + strings.TrimPrefix(jobID, "job_")[:6]
	branch := namespace + "/main"
	var templateSnapshot *domain.WorkflowTemplate
	if workflowTemplate.ID != "" {
		frozen := cloneWorkflowTemplate(workflowTemplate)
		templateSnapshot = &frozen
	}
	job := domain.Job{
		ID:                  jobID,
		ForkedFromJobID:     forkedFromJobID,
		Title:               strings.TrimSpace(req.Title),
		Objective:           strings.TrimSpace(req.Objective),
		AcceptanceCriteria:  append([]string(nil), req.AcceptanceCriteria...),
		Owner:               owner,
		GitRepositoryID:     repository.ID,
		GitRepositoryName:   repository.Name,
		GitRemoteURL:        repository.RemoteURL,
		GitProvider:         repository.Provider,
		GitCredentialScope:  repository.CredentialScope,
		BaseRef:             baseRef,
		Branch:              branch,
		WithSelectors:       append([]string{}, withSelectors...),
		MCPServerIDs:        append([]string{}, mcpIDs...),
		AttachmentIDs:       append([]string{}, attachmentIDs...),
		TemplateID:          workflowTemplate.ID,
		TemplateSnapshot:    templateSnapshot,
		EnvironmentSelector: selector,
		Model:               req.Model,
		PhaseRunIDs:         []string{},
		Status:              domain.JobActive,
		SessionIDs:          []string{},
		CandidateResultIDs:  []string{},
		CreatedAt:           now,
		UpdatedAt:           now,
	}
	for _, attachmentID := range attachmentIDs {
		attachment := s.state.JobAttachments[attachmentID]
		attachment.JobID = job.ID
		s.state.JobAttachments[attachment.ID] = attachment
	}
	if workflowTemplate.ID != "" {
		session, run := s.newWorkflowSessionLocked(&job, workflowTemplate, workflowTemplate.Phases[0], "")
		s.state.Jobs[jobID] = job
		s.state.Sessions[session.ID] = session
		s.state.PhaseRuns[run.ID] = run
		if requestKey != "" {
			s.state.JobRequestKeys[requestKey] = jobID
		}
		if err := s.saveLocked(); err != nil {
			return domain.CreateJobResponse{}, err
		}
		return domain.CreateJobResponse{Job: job, Session: session}, nil
	}
	job.SessionIDs = []string{sessionID}
	session := domain.Session{
		ID:                  sessionID,
		JobID:               jobID,
		ForkMode:            domain.ForkRoot,
		Tool:                tool,
		EnvironmentSelector: selector,
		WithSelectors:       append([]string{}, withSelectors...),
		MCPServerIDs:        append([]string{}, mcpIDs...),
		Role:                "primary",
		Model:               req.Model,
		Operator:            operator,
		GitRepositoryID:     repository.ID,
		BaseRef:             branch,
		GitRef:              namespace + "/sessions/" + sessionID,
		TargetBranch:        branch,
		Status:              domain.SessionQueued,
		TurnIDs:             []string{},
		CheckpointIDs:       []string{},
		ContinuityLevel:     "job_root",
		ContinuityScore:     10,
		CreatedAt:           now,
		UpdatedAt:           now,
	}
	s.state.Jobs[jobID] = job
	s.state.Sessions[sessionID] = session
	if requestKey != "" {
		s.state.JobRequestKeys[requestKey] = jobID
	}
	if err := s.saveLocked(); err != nil {
		return domain.CreateJobResponse{}, err
	}
	return domain.CreateJobResponse{Job: job, Session: session}, nil
}

func (s *Store) PrepareJobDeletion(jobID, operator string) (domain.Job, []domain.Composition, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	job, ok := s.state.Jobs[strings.TrimSpace(jobID)]
	if !ok {
		return domain.Job{}, nil, ErrNotFound
	}
	if normalizeSubject(operator) == "" || job.Owner != normalizeSubject(operator) {
		return domain.Job{}, nil, ErrConflict
	}
	sessionIDs := map[string]bool{}
	for _, session := range s.state.Sessions {
		if session.JobID == job.ID {
			sessionIDs[session.ID] = true
		}
	}
	compositions := make([]domain.Composition, 0)
	for _, composition := range s.state.Compositions {
		if sessionIDs[composition.SessionID] {
			compositions = append(compositions, composition)
		}
	}
	slices.SortFunc(compositions, func(a, b domain.Composition) int { return a.CreatedAt.Compare(b.CreatedAt) })
	return job, compositions, nil
}

// CloseJob preserves the complete Job history while making the Job
// non-runnable. Any active workflow decision is closed as part of the same
// persisted state transition so a browser refresh can never show it as live.
func (s *Store) CloseJob(jobID, operator string) (domain.Job, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	job, ok := s.state.Jobs[strings.TrimSpace(jobID)]
	if !ok {
		return domain.Job{}, ErrNotFound
	}
	operator = normalizeSubject(operator)
	if operator == "" || job.Owner != operator {
		return domain.Job{}, ErrConflict
	}
	if job.Status == domain.JobDone || job.Status == domain.JobCancelled {
		return job, nil
	}

	now := time.Now().UTC()
	if run, exists := s.state.PhaseRuns[job.CurrentPhaseRunID]; exists && (run.Status == domain.PhaseRunQueued || run.Status == domain.PhaseRunRunning || run.Status == domain.PhaseRunPending) {
		run.Status = domain.PhaseRunRejected
		run.PendingReason = ""
		run.PendingOutcome = ""
		run.RejectReason = "Job closed by " + operator
		run.CompletedAt = &now
		s.state.PhaseRuns[run.ID] = run
		if session, exists := s.state.Sessions[run.SessionID]; exists {
			session.Status = domain.SessionCancelled
			session.UpdatedAt = now
			s.state.Sessions[session.ID] = session
		}
	}
	for id, question := range s.state.WorkflowQuestions {
		if question.JobID != job.ID || question.Status != "open" {
			continue
		}
		question.Answer = "closed"
		question.Reason = "Job closed by " + operator
		question.AnsweredBy = operator
		question.Status = "answered"
		question.AnsweredAt = &now
		s.state.WorkflowQuestions[id] = question
	}
	job.Status = domain.JobCancelled
	job.WorkflowStatus = domain.WorkflowDone
	job.PendingReason = ""
	job.CurrentPhaseRunID = ""
	job.UpdatedAt = now
	s.state.Jobs[job.ID] = job
	return job, s.saveLocked()
}

func (s *Store) DeleteJob(jobID, operator string) (domain.Job, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	job, ok := s.state.Jobs[strings.TrimSpace(jobID)]
	if !ok {
		return domain.Job{}, ErrNotFound
	}
	if normalizeSubject(operator) == "" || job.Owner != normalizeSubject(operator) {
		return domain.Job{}, ErrConflict
	}
	for _, candidate := range s.state.Jobs {
		if candidate.ForkedFromJobID == job.ID {
			return domain.Job{}, fmt.Errorf("Job is context for fork %s: %w", candidate.ID, ErrConflict)
		}
	}
	sessionIDs := map[string]bool{}
	for id, session := range s.state.Sessions {
		if session.JobID == job.ID {
			sessionIDs[id] = true
		}
	}
	for _, composition := range s.state.Compositions {
		if !sessionIDs[composition.SessionID] || composition.Runtime == nil || composition.Runtime.Status == "stopped" {
			continue
		}
		return domain.Job{}, fmt.Errorf("Job still has running composition %s: %w", composition.ID, ErrConflict)
	}
	for id, composition := range s.state.Compositions {
		if sessionIDs[composition.SessionID] {
			delete(s.state.Compositions, id)
		}
	}
	for id, activation := range s.state.Activations {
		if sessionIDs[activation.SessionID] {
			delete(s.state.Activations, id)
		}
	}
	for id, turn := range s.state.Turns {
		if sessionIDs[turn.SessionID] {
			delete(s.state.Turns, id)
		}
	}
	for id, checkpoint := range s.state.Checkpoints {
		if sessionIDs[checkpoint.SessionID] {
			delete(s.state.Checkpoints, id)
		}
	}
	for id, result := range s.state.Results {
		if result.JobID == job.ID || sessionIDs[result.SessionID] {
			delete(s.state.Results, id)
		}
	}
	for id, run := range s.state.PhaseRuns {
		if run.JobID == job.ID {
			delete(s.state.PhaseRuns, id)
		}
	}
	removedDeliverables := map[string]bool{}
	for id, deliverable := range s.state.Deliverables {
		if deliverable.JobID == job.ID {
			delete(s.state.Deliverables, id)
			removedDeliverables[id] = true
		}
	}
	for id, comment := range s.state.DeliverableComments {
		if removedDeliverables[comment.DeliverableID] {
			delete(s.state.DeliverableComments, id)
		}
	}
	for id, question := range s.state.WorkflowQuestions {
		if question.JobID == job.ID {
			delete(s.state.WorkflowQuestions, id)
		}
	}
	for _, attachmentID := range job.AttachmentIDs {
		delete(s.state.JobAttachments, attachmentID)
	}
	for sessionID := range sessionIDs {
		delete(s.state.Sessions, sessionID)
	}
	for key, mappedJobID := range s.state.JobRequestKeys {
		if mappedJobID == job.ID {
			delete(s.state.JobRequestKeys, key)
		}
	}
	delete(s.state.Jobs, job.ID)
	return job, s.saveLocked()
}

func (s *Store) CreateJobSession(jobID string, req domain.CreateJobSessionRequest) (domain.Session, error) {
	operator := normalizeSubject(req.Operator)
	selector := strings.ToLower(strings.TrimSpace(req.EnvironmentSelector))
	_, selectorName, err := parseArtifactSelector(selector)
	if operator == "" || strings.TrimSpace(req.ObjectiveDelta) == "" || err != nil {
		return domain.Session{}, fmt.Errorf("operator, objective_delta and a valid environment_selector are required: %w", ErrConflict)
	}
	var requestedWith []string
	if req.WithSelectors != nil {
		requestedWith, err = normalizeArtifactSelectors(req.WithSelectors)
		if err != nil {
			return domain.Session{}, err
		}
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	job, ok := s.state.Jobs[jobID]
	if !ok {
		return domain.Session{}, ErrNotFound
	}
	if job.Status == domain.JobDone || job.Status == domain.JobCancelled {
		return domain.Session{}, fmt.Errorf("job cannot accept sessions while %s: %w", job.Status, ErrConflict)
	}
	if _, ok := s.state.GitRepositories[job.GitRepositoryID]; !ok {
		return domain.Session{}, fmt.Errorf("job Git repository: %w", ErrNotFound)
	}
	if req.SpawnedBySessionID != "" {
		parent, ok := s.state.Sessions[req.SpawnedBySessionID]
		if !ok || parent.JobID != job.ID {
			return domain.Session{}, fmt.Errorf("spawning session must belong to job: %w", ErrConflict)
		}
	}
	withSelectors := append([]string{}, job.WithSelectors...)
	withSelectors = uniqueStrings(append(withSelectors, requestedWith...))
	if err := s.validateSessionEnvironmentLocked(operator, selector, withSelectors, "default"); err != nil {
		return domain.Session{}, err
	}
	mcpIDs := uniqueStrings(req.MCPServerIDs)
	if len(mcpIDs) == 0 {
		mcpIDs = append([]string{}, job.MCPServerIDs...)
	}
	if _, err := s.mcpServersLocked(operator, mcpIDs); err != nil {
		return domain.Session{}, err
	}
	branch := ensureJobBranch(&job)
	now := time.Now().UTC()
	id := newID("ses")
	role := normalizeName(req.Role)
	if role == "" {
		role = "worker"
	}
	session := domain.Session{
		ID:                  id,
		JobID:               job.ID,
		ParentSessionID:     req.SpawnedBySessionID,
		SpawnedBySessionID:  req.SpawnedBySessionID,
		ForkMode:            domain.ForkRoot,
		Tool:                selectorName,
		EnvironmentSelector: selector,
		WithSelectors:       append([]string{}, withSelectors...),
		MCPServerIDs:        append([]string{}, mcpIDs...),
		Role:                role,
		Model:               req.Model,
		Operator:            operator,
		ObjectiveDelta:      strings.TrimSpace(req.ObjectiveDelta),
		GitRepositoryID:     job.GitRepositoryID,
		BaseRef:             branch,
		GitRef:              sessionGitRef(job, id),
		TargetBranch:        branch,
		Status:              domain.SessionQueued,
		TurnIDs:             []string{},
		CheckpointIDs:       []string{},
		ContinuityLevel:     "job_session",
		ContinuityScore:     10,
		CreatedAt:           now,
		UpdatedAt:           now,
	}
	job.SessionIDs = append(job.SessionIDs, session.ID)
	job.Status = domain.JobActive
	job.UpdatedAt = now
	s.state.Sessions[session.ID] = session
	s.state.Jobs[job.ID] = job
	return session, s.saveLocked()
}

func (s *Store) StartTurn(sessionID string, req domain.CreateTurnRequest) (domain.Turn, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	session, activation, err := s.activeLocked(sessionID, req.ActivationID, req.Epoch)
	if err != nil {
		return domain.Turn{}, err
	}
	if session.Status != domain.SessionRunning {
		return domain.Turn{}, ErrConflict
	}
	if strings.TrimSpace(req.Input) == "" {
		return domain.Turn{}, fmt.Errorf("turn input is required: %w", ErrConflict)
	}
	for _, turnID := range session.TurnIDs {
		if s.state.Turns[turnID].Status == domain.TurnRunning {
			return domain.Turn{}, fmt.Errorf("session already has a running turn: %w", ErrConflict)
		}
	}
	now := time.Now().UTC()
	turn := domain.Turn{
		ID:                 newID("trn"),
		SessionID:          session.ID,
		ActivationID:       req.ActivationID,
		ActivationEpoch:    req.Epoch,
		Sequence:           len(session.TurnIDs) + 1,
		Input:              strings.TrimSpace(req.Input),
		Actor:              req.Actor,
		CredentialBindings: cloneMap(activation.CredentialBindings),
		Status:             domain.TurnRunning,
		StartedAt:          now,
	}
	session.TurnIDs = append(session.TurnIDs, turn.ID)
	session.UpdatedAt = now
	s.state.Turns[turn.ID] = turn
	s.state.Sessions[session.ID] = session
	return turn, s.saveLocked()
}

func (s *Store) RegisterClient(req domain.RegisterClientRequest) (domain.Client, error) {
	if strings.TrimSpace(req.Name) == "" {
		return domain.Client{}, fmt.Errorf("client name is required: %w", ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()

	now := time.Now().UTC()
	for id, existing := range s.state.Clients {
		if (strings.TrimSpace(req.InstanceID) != "" && existing.InstanceID == strings.TrimSpace(req.InstanceID)) || (existing.InstanceID == "" && existing.Name == req.Name) {
			existing.InstanceID = strings.TrimSpace(req.InstanceID)
			existing.Name = strings.TrimSpace(req.Name)
			existing.Capabilities = req.Capabilities
			existing.Status = "online"
			if existing.Draining {
				existing.Status = "draining"
			}
			existing.LastSeenAt = now
			s.state.Clients[id] = existing
			return existing, s.saveLocked()
		}
	}
	client := domain.Client{
		ID:           newID("cli"),
		InstanceID:   strings.TrimSpace(req.InstanceID),
		Name:         strings.TrimSpace(req.Name),
		Capabilities: req.Capabilities,
		Status:       "online",
		LastSeenAt:   now,
		CreatedAt:    now,
	}
	s.state.Clients[client.ID] = client
	return client, s.saveLocked()
}

func (s *Store) Client(clientID string) (domain.Client, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	client, ok := s.state.Clients[strings.TrimSpace(clientID)]
	if !ok {
		return domain.Client{}, ErrNotFound
	}
	return client, nil
}

// SetClientDraining changes placement policy without removing runner affinity
// from existing Sessions. Connected is supplied by the live broker so durable
// presence and the explicit drain flag remain separate.
func (s *Store) SetClientDraining(clientID string, draining, connected bool) (domain.Client, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	client, ok := s.state.Clients[strings.TrimSpace(clientID)]
	if !ok {
		return domain.Client{}, ErrNotFound
	}
	client.Draining = draining
	if connected {
		if draining {
			client.Status = "draining"
		} else {
			client.Status = "online"
		}
	} else {
		client.Status = "offline"
	}
	s.state.Clients[client.ID] = client
	return client, s.saveLocked()
}

// SetClientStatus updates connection presence without changing a Session's
// runner affinity. A WebSocket outage is not evidence that local work died.
func (s *Store) SetClientStatus(clientID, status string) (domain.Client, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	client, ok := s.state.Clients[clientID]
	if !ok {
		return domain.Client{}, ErrNotFound
	}
	client.Status = strings.TrimSpace(status)
	client.LastSeenAt = time.Now().UTC()
	s.state.Clients[client.ID] = client
	return client, s.saveLocked()
}

// BindSessionClient records the runner chosen at materialization time. It does
// not change workflow state and is deliberately never cleared on disconnect.
func (s *Store) BindSessionClient(sessionID, clientID string) (domain.Session, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[sessionID]
	if !ok {
		return domain.Session{}, ErrNotFound
	}
	if _, ok := s.state.Clients[clientID]; !ok {
		return domain.Session{}, fmt.Errorf("client: %w", ErrNotFound)
	}
	if session.ClientID != "" && session.ClientID != clientID {
		return domain.Session{}, fmt.Errorf("Session is already pinned to client %s: %w", session.ClientID, ErrConflict)
	}
	session.ClientID = clientID
	session.UpdatedAt = time.Now().UTC()
	s.state.Sessions[session.ID] = session
	return session, s.saveLocked()
}

func (s *Store) AddSnapshotReplica(artifactID, clientID string) (domain.Artifact, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	artifact, ok := s.state.Artifacts[artifactID]
	if !ok {
		return domain.Artifact{}, ErrNotFound
	}
	if _, ok := s.state.Clients[clientID]; !ok {
		return domain.Artifact{}, fmt.Errorf("client: %w", ErrNotFound)
	}
	if artifact.Snapshot.ClientID != clientID && !slices.Contains(artifact.Snapshot.ReplicaClientIDs, clientID) {
		artifact.Snapshot.ReplicaClientIDs = append(artifact.Snapshot.ReplicaClientIDs, clientID)
		slices.Sort(artifact.Snapshot.ReplicaClientIDs)
		s.state.Artifacts[artifact.ID] = artifact
		if err := s.saveLocked(); err != nil {
			return domain.Artifact{}, err
		}
	}
	return artifact, nil
}

func (s *Store) Claim(req domain.ClaimRequest) (domain.Assignment, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	client, ok := s.state.Clients[req.ClientID]
	if !ok {
		return domain.Assignment{}, ErrNotFound
	}
	if client.Draining {
		return domain.Assignment{}, ErrNoWork
	}
	now := time.Now().UTC()
	client.LastSeenAt = now
	client.Status = "online"
	s.state.Clients[client.ID] = client

	sessions := make([]domain.Session, 0, len(s.state.Sessions))
	for _, session := range s.state.Sessions {
		// Workflow Sessions are driven by the in-process ACP supervisor. External
		// workers must not race it for the same isolated workspace.
		if session.PhaseRunID == "" && session.Status == domain.SessionQueued && supportsTool(req.Tools, session.Tool) {
			sessions = append(sessions, session)
		}
	}
	if len(sessions) == 0 {
		_ = s.saveLocked()
		return domain.Assignment{}, ErrNoWork
	}
	slices.SortFunc(sessions, func(a, b domain.Session) int {
		if c := a.CreatedAt.Compare(b.CreatedAt); c != 0 {
			return c
		}
		return strings.Compare(a.ID, b.ID)
	})
	session := sessions[0]
	session.ActivationEpoch++
	activation := domain.Activation{
		ID:            newID("act"),
		SessionID:     session.ID,
		ClientID:      client.ID,
		Operator:      session.Operator,
		CompositionID: session.PreparedCompositionID,
		Epoch:         session.ActivationEpoch,
		Status:        domain.ActivationClaimed,
		StartedAt:     now,
	}
	if composition, ok := s.state.Compositions[session.PreparedCompositionID]; ok {
		activation.CredentialBindings = credentialBindings(composition)
	}
	lease := now.Add(s.leaseTTL)
	session.ClientID = client.ID
	session.ActivationID = activation.ID
	session.LeaseExpiresAt = &lease
	session.Status = domain.SessionClaimed
	session.UpdatedAt = now
	s.state.Sessions[session.ID] = session
	s.state.Activations[activation.ID] = activation
	if err := s.saveLocked(); err != nil {
		return domain.Assignment{}, err
	}
	assignment := domain.Assignment{Job: s.state.Jobs[session.JobID], Session: session, Activation: activation}
	if composition, ok := s.state.Compositions[activation.CompositionID]; ok {
		assignment.Composition = &composition
	}
	return assignment, nil
}

func (s *Store) StartSession(sessionID string, req domain.ActivationRequest) (domain.Session, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	session, activation, err := s.activeLocked(sessionID, req.ActivationID, req.Epoch)
	if err != nil {
		return domain.Session{}, err
	}
	if session.Status != domain.SessionClaimed && session.Status != domain.SessionRunning {
		return domain.Session{}, ErrConflict
	}
	now := time.Now().UTC()
	session.Status = domain.SessionRunning
	session.UpdatedAt = now
	activation.Status = domain.ActivationRunning
	s.state.Sessions[session.ID] = session
	s.state.Activations[activation.ID] = activation
	return session, s.saveLocked()
}

func (s *Store) Heartbeat(activationID string, req domain.ActivationRequest) (domain.Activation, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	activation, ok := s.state.Activations[activationID]
	if !ok {
		return domain.Activation{}, ErrNotFound
	}
	if req.ActivationID != activationID || req.Epoch != activation.Epoch || activation.Status == domain.ActivationEnded {
		return domain.Activation{}, ErrStaleActivation
	}
	session, ok := s.state.Sessions[activation.SessionID]
	if !ok || session.ActivationID != activationID || session.ActivationEpoch != req.Epoch {
		return domain.Activation{}, ErrStaleActivation
	}
	now := time.Now().UTC()
	lease := now.Add(s.leaseTTL)
	session.LeaseExpiresAt = &lease
	session.UpdatedAt = now
	s.state.Sessions[session.ID] = session
	client := s.state.Clients[activation.ClientID]
	client.LastSeenAt = now
	client.Status = "online"
	s.state.Clients[client.ID] = client
	return activation, s.saveLocked()
}

func (s *Store) AddCheckpoint(sessionID string, req domain.CreateCheckpointRequest) (domain.Checkpoint, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	session, _, err := s.activeLocked(sessionID, req.ActivationID, req.Epoch)
	if err != nil {
		return domain.Checkpoint{}, err
	}
	if session.Status != domain.SessionRunning && session.Status != domain.SessionClaimed {
		return domain.Checkpoint{}, ErrConflict
	}
	if req.Kind == "" {
		req.Kind = domain.CheckpointTurnEnd
	}
	var turn domain.Turn
	if req.TurnID != "" {
		var ok bool
		turn, ok = s.state.Turns[req.TurnID]
		if !ok || turn.SessionID != session.ID {
			return domain.Checkpoint{}, fmt.Errorf("turn: %w", ErrNotFound)
		}
		if turn.ActivationID != req.ActivationID || turn.ActivationEpoch != req.Epoch || turn.Status != domain.TurnRunning {
			return domain.Checkpoint{}, ErrStaleActivation
		}
	}
	now := time.Now().UTC()
	checkpoint := domain.Checkpoint{
		ID:                 newID("chk"),
		SessionID:          session.ID,
		ActivationID:       req.ActivationID,
		ActivationEpoch:    req.Epoch,
		TurnID:             req.TurnID,
		ParentCheckpointID: session.CurrentCheckpointID,
		Sequence:           len(session.CheckpointIDs) + 1,
		Kind:               req.Kind,
		Summary:            req.Summary,
		Capsule:            req.Capsule,
		CreatedAt:          now,
	}
	session.CheckpointIDs = append(session.CheckpointIDs, checkpoint.ID)
	session.CurrentCheckpointID = checkpoint.ID
	session.UpdatedAt = now
	if checkpoint.Capsule.Restorable {
		session.ContinuityLevel = "full_checkpoint"
		session.ContinuityScore = 95
	}
	s.state.Checkpoints[checkpoint.ID] = checkpoint
	if req.TurnID != "" && (req.Kind == domain.CheckpointTurnEnd || req.Kind == domain.CheckpointResult) {
		turn.Status = domain.TurnCompleted
		turn.CheckpointID = checkpoint.ID
		turn.EndedAt = &now
		s.state.Turns[turn.ID] = turn
	}
	s.state.Sessions[session.ID] = session
	return checkpoint, s.saveLocked()
}

func (s *Store) CompleteSession(sessionID string, req domain.CreateResultRequest) (domain.Result, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	session, activation, err := s.activeLocked(sessionID, req.ActivationID, req.Epoch)
	if err != nil {
		return domain.Result{}, err
	}
	if session.Status == domain.SessionCompleted {
		return domain.Result{}, ErrConflict
	}
	checkpoint, ok := s.state.Checkpoints[req.CheckpointID]
	if !ok || checkpoint.SessionID != session.ID {
		return domain.Result{}, fmt.Errorf("result checkpoint: %w", ErrNotFound)
	}
	if checkpoint.Kind != domain.CheckpointResult {
		return domain.Result{}, fmt.Errorf("result must reference a result checkpoint: %w", ErrConflict)
	}
	if req.Status == "" {
		req.Status = domain.ResultFailed
	}
	if req.Status != domain.ResultSuccess && req.Status != domain.ResultPartial && req.Status != domain.ResultFailed {
		return domain.Result{}, fmt.Errorf("invalid result status %q: %w", req.Status, ErrConflict)
	}
	if strings.TrimSpace(req.Summary) == "" {
		return domain.Result{}, fmt.Errorf("result summary is required: %w", ErrConflict)
	}
	now := time.Now().UTC()
	result := domain.Result{
		ID:                 newID("res"),
		JobID:              session.JobID,
		SessionID:          session.ID,
		CheckpointID:       checkpoint.ID,
		Status:             req.Status,
		Summary:            req.Summary,
		GitHead:            req.GitHead,
		Tests:              append([]domain.TestEvidence(nil), req.Tests...),
		AcceptanceEvidence: append([]domain.CriterionEvidence(nil), req.AcceptanceEvidence...),
		OpenIssues:         append([]string(nil), req.OpenIssues...),
		Usage:              req.Usage,
		CreatedAt:          now,
	}
	session.Status = domain.SessionCompleted
	session.FinalResultID = result.ID
	session.LeaseExpiresAt = nil
	session.UpdatedAt = now
	activation.Status = domain.ActivationEnded
	activation.Reason = "completed"
	activation.EndedAt = &now
	job := s.state.Jobs[session.JobID]
	job.CandidateResultIDs = append(job.CandidateResultIDs, result.ID)
	if result.Status == domain.ResultSuccess {
		successCount := 1
		for _, resultID := range job.CandidateResultIDs[:len(job.CandidateResultIDs)-1] {
			if candidate, ok := s.state.Results[resultID]; ok && candidate.Status == domain.ResultSuccess {
				successCount++
			}
		}
		if successCount > 1 {
			job.Status = domain.JobComparing
		} else {
			job.Status = domain.JobReview
		}
	} else {
		job.Status = domain.JobActive
	}
	job.UpdatedAt = now
	s.state.Results[result.ID] = result
	s.state.Sessions[session.ID] = session
	s.state.Activations[activation.ID] = activation
	s.state.Jobs[job.ID] = job
	return result, s.saveLocked()
}

func (s *Store) ForkSession(parentSessionID string, req domain.ForkSessionRequest) (domain.Session, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	parent, ok := s.state.Sessions[parentSessionID]
	if !ok {
		return domain.Session{}, ErrNotFound
	}
	if req.ForkMode == "" {
		req.ForkMode = domain.ForkFull
	}
	checkpointID := req.CheckpointID
	if checkpointID == "" {
		checkpointID = parent.CurrentCheckpointID
	}
	var sourceCheckpoint domain.Checkpoint
	if req.ForkMode != domain.ForkRoot {
		var ok bool
		sourceCheckpoint, ok = s.state.Checkpoints[checkpointID]
		if !ok || sourceCheckpoint.SessionID != parent.ID {
			return domain.Session{}, fmt.Errorf("fork checkpoint: %w", ErrNotFound)
		}
	}
	for _, resultID := range req.InputResultIDs {
		result, ok := s.state.Results[resultID]
		if !ok || result.JobID != parent.JobID {
			return domain.Session{}, fmt.Errorf("input result %s: %w", resultID, ErrNotFound)
		}
	}
	tool := normalizeName(req.Tool)
	environmentSelector := parent.EnvironmentSelector
	if tool == "" {
		tool = parent.Tool
	} else {
		environmentSelector = "tool:" + tool
	}
	if req.ForkMode == domain.ForkFull && (!sourceCheckpoint.Capsule.Restorable || tool != parent.Tool) {
		return domain.Session{}, fmt.Errorf("full fork requires a restorable checkpoint and the same tool: %w", ErrConflict)
	}
	operator := normalizeSubject(req.Operator)
	if operator == "" {
		operator = parent.Operator
	}
	if err := s.validateSessionEnvironmentLocked(operator, environmentSelector, parent.WithSelectors, "default"); err != nil {
		return domain.Session{}, err
	}
	now := time.Now().UTC()
	id := newID("ses")
	job := s.state.Jobs[parent.JobID]
	branch := ensureJobBranch(&job)
	session := domain.Session{
		ID:                  id,
		JobID:               parent.JobID,
		ParentSessionID:     parent.ID,
		SpawnedBySessionID:  parent.ID,
		ParentCheckpointID:  checkpointID,
		InputResultIDs:      append([]string(nil), req.InputResultIDs...),
		ForkMode:            req.ForkMode,
		Tool:                tool,
		EnvironmentSelector: environmentSelector,
		WithSelectors:       append([]string{}, parent.WithSelectors...),
		MCPServerIDs:        append([]string{}, parent.MCPServerIDs...),
		Role:                parent.Role,
		Model:               req.Model,
		Operator:            operator,
		ObjectiveDelta:      req.ObjectiveDelta,
		GitRepositoryID:     parent.GitRepositoryID,
		BaseRef:             branch,
		GitRef:              sessionGitRef(job, id),
		TargetBranch:        branch,
		Status:              domain.SessionQueued,
		TurnIDs:             []string{},
		CheckpointIDs:       []string{},
		ContinuityLevel:     continuityLevel(req.ForkMode),
		ContinuityScore:     continuityScore(req.ForkMode, tool == parent.Tool),
		CreatedAt:           now,
		UpdatedAt:           now,
	}
	if session.Model == "" {
		session.Model = parent.Model
	}
	job.SessionIDs = append(job.SessionIDs, session.ID)
	job.Status = domain.JobActive
	job.UpdatedAt = now
	s.state.Sessions[session.ID] = session
	s.state.Jobs[job.ID] = job
	return session, s.saveLocked()
}

func (s *Store) SelectResult(jobID string, req domain.SelectResultRequest) (domain.Job, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	job, ok := s.state.Jobs[jobID]
	if !ok {
		return domain.Job{}, ErrNotFound
	}
	result, ok := s.state.Results[req.ResultID]
	if !ok || result.JobID != job.ID {
		return domain.Job{}, ErrNotFound
	}
	job.FinalResultID = result.ID
	job.Status = domain.JobDone
	job.UpdatedAt = time.Now().UTC()
	s.state.Jobs[job.ID] = job
	return job, s.saveLocked()
}

func (s *Store) Snapshot() domain.Snapshot {
	s.mu.Lock()
	defer s.mu.Unlock()

	// Keep the wire format stable for empty stores. A nil Go slice is encoded as
	// JSON null, while every snapshot collection is an array in the API contract.
	out := domain.Snapshot{
		Artifacts:           []domain.Artifact{},
		Recordings:          []domain.Recording{},
		Compositions:        []domain.Composition{},
		Jobs:                []domain.Job{},
		JobAttachments:      []domain.JobAttachment{},
		WorkflowTemplates:   []domain.WorkflowTemplate{},
		PhaseRuns:           []domain.PhaseRun{},
		Deliverables:        []domain.Deliverable{},
		DeliverableComments: []domain.DeliverableComment{},
		WorkflowQuestions:   []domain.WorkflowQuestion{},
		Sessions:            []domain.Session{},
		Activations:         []domain.Activation{},
		Turns:               []domain.Turn{},
		Checkpoints:         []domain.Checkpoint{},
		Results:             []domain.Result{},
		Clients:             []domain.Client{},
		MCPServers:          []domain.MCPServer{},
		GitRepositories:     []domain.GitRepository{},
		GitAccounts:         []domain.GitAccount{},
		Users:               []domain.PublicUser{},
	}
	for _, v := range s.state.Artifacts {
		out.Artifacts = append(out.Artifacts, v)
	}
	for _, v := range s.state.Recordings {
		out.Recordings = append(out.Recordings, v)
	}
	for _, v := range s.state.Compositions {
		out.Compositions = append(out.Compositions, v)
	}
	for _, v := range s.state.Jobs {
		out.Jobs = append(out.Jobs, v)
	}
	for _, v := range s.state.JobAttachments {
		if v.JobID != "" {
			out.JobAttachments = append(out.JobAttachments, v)
		}
	}
	for _, v := range s.state.WorkflowTemplates {
		out.WorkflowTemplates = append(out.WorkflowTemplates, v)
	}
	for _, v := range s.state.PhaseRuns {
		out.PhaseRuns = append(out.PhaseRuns, v)
	}
	for _, v := range s.state.Deliverables {
		out.Deliverables = append(out.Deliverables, v)
	}
	for _, v := range s.state.DeliverableComments {
		out.DeliverableComments = append(out.DeliverableComments, v)
	}
	for _, v := range s.state.WorkflowQuestions {
		out.WorkflowQuestions = append(out.WorkflowQuestions, v)
	}
	for _, v := range s.state.Sessions {
		out.Sessions = append(out.Sessions, v)
	}
	for _, v := range s.state.Activations {
		out.Activations = append(out.Activations, v)
	}
	for _, v := range s.state.Turns {
		out.Turns = append(out.Turns, v)
	}
	for _, v := range s.state.Checkpoints {
		out.Checkpoints = append(out.Checkpoints, v)
	}
	for _, v := range s.state.Results {
		out.Results = append(out.Results, v)
	}
	for _, v := range s.state.Clients {
		out.Clients = append(out.Clients, v)
	}
	for _, v := range s.state.MCPServers {
		out.MCPServers = append(out.MCPServers, redactMCPServer(v))
	}
	for _, v := range s.state.GitRepositories {
		out.GitRepositories = append(out.GitRepositories, v)
	}
	for _, v := range s.state.GitAccounts {
		out.GitAccounts = append(out.GitAccounts, redactGitAccount(v))
	}
	for _, v := range s.state.Users {
		out.Users = append(out.Users, publicUser(v))
	}
	slices.SortFunc(out.Artifacts, func(a, b domain.Artifact) int { return b.CreatedAt.Compare(a.CreatedAt) })
	slices.SortFunc(out.Recordings, func(a, b domain.Recording) int { return b.StartedAt.Compare(a.StartedAt) })
	slices.SortFunc(out.Compositions, func(a, b domain.Composition) int { return b.CreatedAt.Compare(a.CreatedAt) })
	slices.SortFunc(out.Jobs, func(a, b domain.Job) int { return b.CreatedAt.Compare(a.CreatedAt) })
	slices.SortFunc(out.JobAttachments, func(a, b domain.JobAttachment) int { return a.CreatedAt.Compare(b.CreatedAt) })
	slices.SortFunc(out.WorkflowTemplates, func(a, b domain.WorkflowTemplate) int { return strings.Compare(a.Name, b.Name) })
	slices.SortFunc(out.PhaseRuns, func(a, b domain.PhaseRun) int { return a.StartedAt.Compare(b.StartedAt) })
	slices.SortFunc(out.Deliverables, func(a, b domain.Deliverable) int { return a.CreatedAt.Compare(b.CreatedAt) })
	slices.SortFunc(out.DeliverableComments, func(a, b domain.DeliverableComment) int { return a.CreatedAt.Compare(b.CreatedAt) })
	slices.SortFunc(out.WorkflowQuestions, func(a, b domain.WorkflowQuestion) int { return a.CreatedAt.Compare(b.CreatedAt) })
	slices.SortFunc(out.Sessions, func(a, b domain.Session) int { return a.CreatedAt.Compare(b.CreatedAt) })
	slices.SortFunc(out.Activations, func(a, b domain.Activation) int { return a.StartedAt.Compare(b.StartedAt) })
	slices.SortFunc(out.Turns, func(a, b domain.Turn) int { return a.StartedAt.Compare(b.StartedAt) })
	slices.SortFunc(out.Checkpoints, func(a, b domain.Checkpoint) int { return a.CreatedAt.Compare(b.CreatedAt) })
	slices.SortFunc(out.Results, func(a, b domain.Result) int { return a.CreatedAt.Compare(b.CreatedAt) })
	slices.SortFunc(out.Clients, func(a, b domain.Client) int { return strings.Compare(a.Name, b.Name) })
	slices.SortFunc(out.MCPServers, func(a, b domain.MCPServer) int { return b.CreatedAt.Compare(a.CreatedAt) })
	slices.SortFunc(out.GitRepositories, func(a, b domain.GitRepository) int { return strings.Compare(a.Name, b.Name) })
	slices.SortFunc(out.GitAccounts, func(a, b domain.GitAccount) int {
		if byOperator := strings.Compare(a.Operator, b.Operator); byOperator != 0 {
			return byOperator
		}
		if byProvider := strings.Compare(a.Provider, b.Provider); byProvider != 0 {
			return byProvider
		}
		return strings.Compare(a.Login, b.Login)
	})
	slices.SortFunc(out.Users, func(a, b domain.PublicUser) int { return strings.Compare(a.Username, b.Username) })
	return out
}

func (s *Store) activeLocked(sessionID, activationID string, epoch int64) (domain.Session, domain.Activation, error) {
	session, ok := s.state.Sessions[sessionID]
	if !ok {
		return domain.Session{}, domain.Activation{}, ErrNotFound
	}
	activation, ok := s.state.Activations[activationID]
	if !ok {
		return domain.Session{}, domain.Activation{}, ErrNotFound
	}
	if session.ActivationID != activationID || session.ActivationEpoch != epoch || activation.SessionID != sessionID || activation.Epoch != epoch || activation.Status == domain.ActivationEnded {
		return domain.Session{}, domain.Activation{}, ErrStaleActivation
	}
	return session, activation, nil
}

func (s *Store) saveLocked() error {
	if s.path == "" {
		return nil
	}
	persisted, err := s.encryptedStateLocked()
	if err != nil {
		return fmt.Errorf("encrypt state: %w", err)
	}
	var encoded bytes.Buffer
	enc := json.NewEncoder(&encoded)
	enc.SetIndent("", "  ")
	if err := enc.Encode(persisted); err != nil {
		return fmt.Errorf("encode state: %w", err)
	}
	if err := s.backend.WriteFile(s.path, encoded.Bytes()); err != nil {
		return fmt.Errorf("replace state: %w", err)
	}
	return nil
}

type filesystemStateBackend struct{}

func (filesystemStateBackend) ReadFile(path string) ([]byte, error) {
	return os.ReadFile(path)
}

func (filesystemStateBackend) WriteFile(path string, data []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o750); err != nil {
		return fmt.Errorf("create state directory: %w", err)
	}
	tmp, err := os.CreateTemp(filepath.Dir(path), ".spin-state-*")
	if err != nil {
		return fmt.Errorf("create state temp file: %w", err)
	}
	tmpName := tmp.Name()
	defer os.Remove(tmpName)
	if _, err := tmp.Write(data); err != nil {
		_ = tmp.Close()
		return fmt.Errorf("write state temp file: %w", err)
	}
	if err := tmp.Sync(); err != nil {
		_ = tmp.Close()
		return fmt.Errorf("sync state: %w", err)
	}
	if err := tmp.Close(); err != nil {
		return fmt.Errorf("close state: %w", err)
	}
	return os.Rename(tmpName, path)
}

func supportsTool(supported []string, wanted string) bool {
	return slices.Contains(supported, "*") || slices.Contains(supported, wanted)
}

func continuityLevel(mode domain.ForkMode) string {
	switch mode {
	case domain.ForkFull:
		return "full_checkpoint"
	case domain.ForkFilesystem:
		return "filesystem"
	case domain.ForkResult, domain.ForkCritic, domain.ForkSynthesis:
		return "result_handoff"
	default:
		return "job_root"
	}
}

func continuityScore(mode domain.ForkMode, sameTool bool) int {
	switch mode {
	case domain.ForkFull:
		if sameTool {
			return 95
		}
		return 30
	case domain.ForkFilesystem:
		return 30
	case domain.ForkResult, domain.ForkCritic, domain.ForkSynthesis:
		return 20
	default:
		return 10
	}
}

func applyArtifactDefaults(req *domain.CreateRecordingRequest, name string) {
	if len(req.Provides) == 0 {
		req.Provides = []string{string(req.Kind) + ":" + name}
	}
	if req.Slot == "" {
		req.Slot = string(req.Kind) + ":" + name
	}
	if req.Kind == domain.ArtifactCredential {
		if req.Sensitivity == "" {
			req.Sensitivity = domain.SensitivitySecret
		}
	} else if req.Sensitivity == "" {
		if req.Scope == domain.ScopeGlobal && req.Kind == domain.ArtifactTool {
			req.Sensitivity = domain.SensitivityPublic
		} else {
			req.Sensitivity = domain.SensitivityPrivate
		}
	}
}

func (s *Store) openRecordingLocked(actor string) (domain.Recording, bool) {
	for _, recording := range s.state.Recordings {
		if recording.Actor == actor && recording.Status == domain.RecordingOpen {
			return recording, true
		}
	}
	return domain.Recording{}, false
}

func (s *Store) latestArtifactLocked(kind domain.ArtifactKind, name, actor, profile string) (domain.Artifact, bool) {
	var selected domain.Artifact
	selectedRank := -1
	for _, artifact := range s.state.Artifacts {
		if artifact.Kind != kind || artifact.Name != name || !canUseArtifact(actor, artifact) {
			continue
		}
		if profile != "" && artifact.Profile != profile {
			continue
		}
		rank := artifactScopeRank(artifact, actor)
		if rank > selectedRank || (rank == selectedRank && artifact.CreatedAt.After(selected.CreatedAt)) {
			selected = artifact
			selectedRank = rank
		}
	}
	return selected, selectedRank >= 0
}

func (s *Store) resolveArtifactSelectorLocked(selector, operator, profile string) (domain.Artifact, error) {
	kind, name, err := parseArtifactSelector(selector)
	if err != nil {
		return domain.Artifact{}, err
	}
	var selected domain.Artifact
	selectedRank := -1
	for _, artifact := range s.state.Artifacts {
		if artifact.Kind != kind || artifact.Name != name || !canUseArtifact(operator, artifact) {
			continue
		}
		if profile != "" && artifact.Profile != profile {
			continue
		}
		rank := artifactScopeRank(artifact, operator)
		if rank > selectedRank || (rank == selectedRank && artifact.CreatedAt.After(selected.CreatedAt)) {
			selected = artifact
			selectedRank = rank
		}
	}
	if selectedRank < 0 {
		return domain.Artifact{}, fmt.Errorf("%s: %w", selector, ErrNotFound)
	}
	return selected, nil
}

func (s *Store) addArtifactLocked(composition *domain.Composition, artifact domain.Artifact, reason string) error {
	for _, existing := range composition.ResolvedArtifacts {
		if existing.ArtifactID == artifact.ID {
			return nil
		}
	}
	for _, parentID := range artifact.ParentArtifactIDs {
		parent, ok := s.state.Artifacts[parentID]
		if !ok {
			return fmt.Errorf("artifact %s parent %s: %w", artifact.ID, parentID, ErrNotFound)
		}
		if err := s.addArtifactLocked(composition, parent, "dependency of "+artifact.ID); err != nil {
			return err
		}
	}
	if current, exists := composition.SlotBindings[artifact.Slot]; artifact.Slot != "" && exists && current != artifact.ID {
		if !s.artifactDependsOnLocked(artifact.ID, current) {
			return fmt.Errorf("slot %s is already bound by %s: %w", artifact.Slot, current, ErrConflict)
		}
	}
	if artifact.Slot != "" {
		// A descendant with the same logical selector is a new immutable version
		// of that layer. Replace its ancestor binding while preserving the graph.
		composition.SlotBindings[artifact.Slot] = artifact.ID
	}
	composition.ResolvedArtifacts = append(composition.ResolvedArtifacts, domain.ResolvedArtifact{
		ArtifactID: artifact.ID,
		Kind:       string(artifact.Kind),
		Name:       artifact.Name,
		Slot:       artifact.Slot,
		Scope:      string(artifact.Scope),
		Subject:    artifact.Subject,
		Profile:    artifact.Profile,
		Enables:    append([]domain.Enablement{}, artifact.Enables...),
		Reason:     reason,
	})
	composition.Enabled = mergeEnablements(composition.Enabled, artifact.Enables)
	return nil
}

func (s *Store) artifactDependsOnLocked(artifactID, ancestorID string) bool {
	visited := map[string]bool{}
	var dependsOn func(string) bool
	dependsOn = func(id string) bool {
		if id == ancestorID {
			return true
		}
		if visited[id] {
			return false
		}
		visited[id] = true
		artifact, ok := s.state.Artifacts[id]
		if !ok {
			return false
		}
		return slices.ContainsFunc(artifact.ParentArtifactIDs, dependsOn)
	}
	return dependsOn(artifactID)
}

func validateRequirements(composition domain.Composition, artifacts map[string]domain.Artifact) error {
	provided := map[string]bool{}
	for _, resolved := range composition.ResolvedArtifacts {
		artifact, ok := artifacts[resolved.ArtifactID]
		if !ok {
			return ErrNotFound
		}
		for _, capability := range artifact.Provides {
			provided[capability] = true
		}
	}
	for _, resolved := range composition.ResolvedArtifacts {
		artifact := artifacts[resolved.ArtifactID]
		for _, requirement := range artifact.Requires {
			if !provided[requirement] {
				return fmt.Errorf("artifact %s requires %s: %w", artifact.ID, requirement, ErrConflict)
			}
		}
	}
	return nil
}

func credentialBindings(composition domain.Composition) map[string]string {
	out := map[string]string{}
	for slot, artifactID := range composition.SlotBindings {
		if strings.HasPrefix(slot, "credential:") {
			out[slot] = artifactID
		}
	}
	return out
}

func (s *Store) artifactEnablesLocked(artifactID, capability string) bool {
	visited := map[string]bool{}
	var enables func(string) bool
	enables = func(id string) bool {
		if visited[id] {
			return false
		}
		visited[id] = true
		artifact, ok := s.state.Artifacts[id]
		if !ok {
			return false
		}
		if enablementsContain(artifact.Enables, capability) {
			return true
		}
		for _, parentID := range artifact.ParentArtifactIDs {
			if enables(parentID) {
				return true
			}
		}
		return false
	}
	return enables(artifactID)
}

func (s *Store) validateDirectEnabledSelectorLocked(operator, selector, capability, profile string) error {
	artifact, err := s.resolveArtifactSelectorLocked(selector, operator, profile)
	if err != nil {
		return fmt.Errorf("resolve %s: %w", selector, err)
	}
	if !enablementsContain(artifact.Enables, capability) {
		return fmt.Errorf("%s does not directly ENABLE %s: %w", selector, normalizeName(capability), ErrConflict)
	}
	return nil
}

func (s *Store) defaultEnabledSelectorLocked(operator, capability, profile string) (string, error) {
	selectors := make([]string, 0)
	for _, artifact := range s.state.Artifacts {
		if !canUseArtifact(operator, artifact) || (profile != "" && artifact.Profile != profile) || !enablementsContain(artifact.Enables, capability) {
			continue
		}
		selectors = append(selectors, string(artifact.Kind)+":"+artifact.Name)
	}
	selectors = uniqueStrings(selectors)
	if len(selectors) == 0 {
		return "", fmt.Errorf("no environment ENABLES %s: %w", normalizeName(capability), ErrConflict)
	}
	if len(selectors) > 1 {
		return "", fmt.Errorf("multiple environments ENABLE %s; select one explicitly: %w", normalizeName(capability), ErrConflict)
	}
	return selectors[0], nil
}

func (s *Store) validateProjectLayerSelectorsLocked(operator string, selectors []string, profile string) error {
	for _, selector := range selectors {
		artifact, err := s.resolveArtifactSelectorLocked(selector, operator, profile)
		if err != nil {
			return fmt.Errorf("resolve project layer %s: %w", selector, err)
		}
		if artifact.Kind == domain.ArtifactCredential || enablementsContain(artifact.Enables, "git") || enablementsContain(artifact.Enables, "acp") {
			return fmt.Errorf("%s is an identity or control-plane environment, not a project layer: %w", selector, ErrConflict)
		}
	}
	return nil
}

func (s *Store) validateSessionEnvironmentLocked(operator, selector string, withSelectors []string, profile string) error {
	selectors := append([]string{selector}, withSelectors...)
	enabled := map[string]bool{}
	for _, current := range selectors {
		artifact, err := s.resolveArtifactSelectorLocked(current, operator, profile)
		if err != nil {
			return fmt.Errorf("resolve Session environment %s: %w", current, err)
		}
		for _, capability := range []string{"git", "acp"} {
			if s.artifactEnablesLocked(artifact.ID, capability) {
				enabled[capability] = true
			}
		}
	}
	for _, capability := range []string{"git", "acp"} {
		if !enabled[capability] {
			return fmt.Errorf("Session environment does not ENABLE %s: %w", capability, ErrConflict)
		}
	}
	return nil
}

func ensureJobBranch(job *domain.Job) string {
	if strings.HasPrefix(job.Branch, "jobs/") && strings.HasSuffix(job.Branch, "/main") {
		return job.Branch
	}
	suffix := strings.TrimPrefix(job.ID, "job_")
	if len(suffix) > 6 {
		suffix = suffix[:6]
	}
	job.Branch = "jobs/" + gitSlug(job.Title) + "-" + suffix + "/main"
	return job.Branch
}

func sessionGitRef(job domain.Job, sessionID string) string {
	namespace := strings.TrimSuffix(job.Branch, "/main")
	return namespace + "/sessions/" + sessionID
}

func normalizeGitCredentialScope(scope domain.CredentialScope, fallback domain.CredentialScope) domain.CredentialScope {
	switch domain.CredentialScope(strings.ToLower(strings.TrimSpace(string(scope)))) {
	case domain.CredentialScopeUser:
		return domain.CredentialScopeUser
	case domain.CredentialScopeGlobal:
		return domain.CredentialScopeGlobal
	case domain.CredentialScopePublic:
		return domain.CredentialScopePublic
	default:
		return fallback
	}
}

func validGitCredentialScope(scope domain.CredentialScope) bool {
	return scope == domain.CredentialScopeUser || scope == domain.CredentialScopeGlobal || scope == domain.CredentialScopePublic
}

func gitRemoteIdentity(remote string) (provider, host string) {
	remote = strings.TrimSpace(remote)
	if strings.HasPrefix(remote, "git@") {
		host, _, _ = strings.Cut(strings.TrimPrefix(remote, "git@"), ":")
	} else if parsed, err := url.Parse(remote); err == nil {
		host = parsed.Hostname()
		if parsed.Port() != "" {
			host += ":" + parsed.Port()
		}
	}
	host = strings.ToLower(strings.TrimSpace(host))
	switch host {
	case "github.com":
		provider = "github"
	case "gitlab.com":
		provider = "gitlab"
	default:
		provider = host
	}
	return provider, host
}

func validGitRemote(value string) bool {
	if value == "" || strings.ContainsAny(value, "\r\n\t ") {
		return false
	}
	if strings.HasPrefix(value, "git@") {
		return strings.Contains(strings.TrimPrefix(value, "git@"), ":")
	}
	parsed, err := url.Parse(value)
	if err != nil || parsed.Host == "" || parsed.RawQuery != "" || parsed.Fragment != "" {
		return false
	}
	switch parsed.Scheme {
	case "http", "https", "git":
		return parsed.User == nil
	case "ssh":
		if parsed.User == nil {
			return true
		}
		_, hasPassword := parsed.User.Password()
		return !hasPassword
	default:
		return false
	}
}

func validGitBaseRef(value string) bool {
	if value == "" || strings.HasPrefix(value, "-") || strings.HasPrefix(value, ".") || strings.HasSuffix(value, ".") || strings.Contains(value, "..") || strings.Contains(value, "@{") || strings.ContainsAny(value, "\r\n\t ~^:?*[\\") {
		return false
	}
	for _, part := range strings.Split(value, "/") {
		if part == "" || strings.HasSuffix(part, ".lock") {
			return false
		}
	}
	return true
}

func canUseArtifact(actor string, artifact domain.Artifact) bool {
	return artifact.Scope != domain.ScopeUser || artifact.Subject == normalizeSubject(actor)
}

func parseArtifactSelector(selector string) (domain.ArtifactKind, string, error) {
	selector = strings.ToLower(strings.TrimSpace(selector))
	kind, name, ok := strings.Cut(selector, ":")
	if !ok || !validToken(kind) || !validToken(name) {
		return "", "", fmt.Errorf("selector must be kind:name, got %q: %w", selector, ErrConflict)
	}
	return domain.ArtifactKind(kind), name, nil
}

func normalizeArtifactSelectors(values []string) ([]string, error) {
	out := make([]string, 0, len(values))
	seen := map[string]bool{}
	for _, value := range values {
		selector := strings.ToLower(strings.TrimSpace(value))
		if selector == "" {
			continue
		}
		if _, _, err := parseArtifactSelector(selector); err != nil {
			return nil, fmt.Errorf("invalid WITH selector %q: %w", value, ErrConflict)
		}
		if !seen[selector] {
			seen[selector] = true
			out = append(out, selector)
		}
	}
	return out, nil
}

func artifactScopeRank(artifact domain.Artifact, operator string) int {
	switch artifact.Scope {
	case domain.ScopeUser:
		if artifact.Subject == normalizeSubject(operator) {
			return 4
		}
		return -1
	case domain.ScopeProject:
		return 3
	case domain.ScopeTeam:
		return 2
	case domain.ScopeGlobal:
		return 1
	default:
		return 0
	}
}

func normalizeEnablements(values []domain.Enablement) ([]domain.Enablement, error) {
	out := make([]domain.Enablement, 0, len(values))
	for _, value := range values {
		value.Name = normalizeName(value.Name)
		value.Command = strings.TrimSpace(value.Command)
		value.Transport = normalizeName(value.Transport)
		if !validToken(value.Name) {
			return nil, fmt.Errorf("invalid enabled capability %q: %w", value.Name, ErrConflict)
		}
		if value.ProtocolVersion < 0 {
			return nil, fmt.Errorf("protocol version cannot be negative: %w", ErrConflict)
		}
		if value.Name == "acp" {
			if value.Transport == "" {
				value.Transport = "stdio"
			}
			if value.ProtocolVersion == 0 {
				value.ProtocolVersion = 1
			}
		}
		out = mergeEnablements(out, []domain.Enablement{value})
	}
	return out, nil
}

func mergeEnablements(base, overrides []domain.Enablement) []domain.Enablement {
	out := append([]domain.Enablement{}, base...)
	for _, override := range overrides {
		replaced := false
		for i := range out {
			if out[i].Name == override.Name {
				out[i] = override
				replaced = true
				break
			}
		}
		if !replaced {
			out = append(out, override)
		}
	}
	return out
}

func enablementsContain(values []domain.Enablement, name string) bool {
	name = normalizeName(name)
	return slices.ContainsFunc(values, func(value domain.Enablement) bool { return normalizeName(value.Name) == name })
}

func requireEnabledCapabilities(composition domain.Composition, names ...string) error {
	for _, name := range names {
		if !enablementsContain(composition.Enabled, name) {
			return fmt.Errorf("Session environment does not ENABLE %s: %w", normalizeName(name), ErrConflict)
		}
	}
	return nil
}

func (s *Store) mcpServersLocked(operator string, ids []string) ([]domain.MCPServer, error) {
	out := make([]domain.MCPServer, 0, len(ids))
	for _, id := range uniqueStrings(ids) {
		server, ok := s.state.MCPServers[id]
		if !ok || server.Operator != normalizeSubject(operator) {
			return nil, fmt.Errorf("MCP server %s is not available to %s: %w", id, operator, ErrNotFound)
		}
		out = append(out, server)
	}
	return out, nil
}

func normalizeMCPSecrets(values []domain.MCPSecret) ([]domain.MCPSecret, error) {
	out := make([]domain.MCPSecret, 0, len(values))
	seen := map[string]bool{}
	for _, value := range values {
		value.Name = strings.TrimSpace(value.Name)
		if value.Name == "" || seen[strings.ToLower(value.Name)] {
			return nil, fmt.Errorf("MCP credential names must be non-empty and unique: %w", ErrConflict)
		}
		seen[strings.ToLower(value.Name)] = true
		out = append(out, value)
	}
	return out, nil
}

func redactMCPServer(server domain.MCPServer) domain.MCPServer {
	server.Env = redactMCPSecrets(server.Env)
	server.Headers = redactMCPSecrets(server.Headers)
	return server
}

func redactGitAccount(account domain.GitAccount) domain.GitAccount {
	account.AccessToken = ""
	account.RefreshToken = ""
	return account
}

func redactMCPSecrets(values []domain.MCPSecret) []domain.MCPSecret {
	out := append([]domain.MCPSecret{}, values...)
	for i := range out {
		out[i].Value = ""
	}
	return out
}

func withoutString(values []string, remove string) []string {
	out := make([]string, 0, len(values))
	for _, value := range values {
		if value != remove {
			out = append(out, value)
		}
	}
	return out
}

func gitSlug(value string) string {
	value = strings.ToLower(strings.TrimSpace(value))
	var builder strings.Builder
	lastDash := false
	for _, r := range value {
		if (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') {
			builder.WriteRune(r)
			lastDash = false
			continue
		}
		if builder.Len() > 0 && !lastDash {
			builder.WriteByte('-')
			lastDash = true
		}
	}
	slug := strings.Trim(builder.String(), "-")
	if slug == "" {
		return "work"
	}
	return slug
}

func validKind(kind domain.ArtifactKind) bool {
	return validToken(string(kind))
}

func validScope(scope domain.ArtifactScope) bool {
	return scope == domain.ScopeGlobal || scope == domain.ScopeTeam || scope == domain.ScopeProject || scope == domain.ScopeUser
}

func validToken(value string) bool {
	value = strings.TrimSpace(value)
	if value == "" {
		return false
	}
	for _, r := range value {
		if (r < 'a' || r > 'z') && (r < '0' || r > '9') && r != '-' && r != '_' {
			return false
		}
	}
	return true
}

func normalizeName(value string) string {
	return strings.ToLower(strings.TrimSpace(value))
}

func normalizeSubject(value string) string {
	return strings.ToLower(strings.TrimSpace(value))
}

func uniqueStrings(values []string) []string {
	out := make([]string, 0, len(values))
	seen := map[string]bool{}
	for _, value := range values {
		value = strings.TrimSpace(value)
		if value == "" || seen[value] {
			continue
		}
		seen[value] = true
		out = append(out, value)
	}
	return out
}

func cloneMap(values map[string]string) map[string]string {
	if len(values) == 0 {
		return nil
	}
	out := make(map[string]string, len(values))
	for key, value := range values {
		out[key] = value
	}
	return out
}

func newID(prefix string) string {
	b := make([]byte, 6)
	if _, err := rand.Read(b); err != nil {
		panic(err)
	}
	return prefix + "_" + hex.EncodeToString(b)
}
