package server

import (
	"context"
	"easyacp/internal/worker"
	"errors"
	"fmt"
	"io"
	"strings"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

func (s *Server) executeRecordingCommand(ctx context.Context, recordingID string, req domain.ExecuteRecordingCommandRequest) (domain.Recording, capsule.Execution, error) {
	recording, err := s.store.Recording(recordingID)
	if err != nil {
		return domain.Recording{}, capsule.Execution{}, err
	}
	open, err := s.store.OpenRecording(req.Actor)
	if err != nil || open.ID != recording.ID {
		return domain.Recording{}, capsule.Execution{}, store.ErrConflict
	}
	execution, err := s.engine.Execute(ctx, recording, req.Input)
	if err != nil {
		return domain.Recording{}, capsule.Execution{}, fmt.Errorf("execute in capsule: %w", err)
	}
	recording, err = s.store.RecordExecution(recordingID, req.Actor, &execution.ExitCode)
	return recording, execution, err
}

// editCapsuleArtifact starts a new version: the current version of a layer is
// recorded again with every setting it has, and ending that recording makes
// the result the new version everything follows.
func (s *Server) editCapsuleArtifact(actor, artifactID string) (domain.Recording, *domain.StartStatus, error) {
	current, err := s.store.Artifact(artifactID)
	if err != nil {
		return domain.Recording{}, nil, err
	}
	if current.SupersededBy != "" {
		return domain.Recording{}, nil, fmt.Errorf("%s is an older version; edit the current one: %w", artifactSelectorOf(current), store.ErrConflict)
	}
	if !canUseArtifact(actor, current) {
		return domain.Recording{}, nil, fmt.Errorf("%s belongs to another user: %w", artifactSelectorOf(current), store.ErrConflict)
	}
	return s.createCapsuleRecording(domain.CreateRecordingRequest{
		Actor: actor, Kind: current.Kind, Name: current.Name,
		Scope: current.Scope, Subject: current.Subject, Profile: current.Profile,
		Provides: current.Provides, Requires: current.Requires, Enables: current.Enables, Slot: current.Slot,
		ParentArtifactIDs: []string{current.ID}, CompatibilityFingerprint: current.CompatibilityFingerprint,
		Sensitivity: current.Sensitivity, ReplacesArtifactID: current.ID,
	})
}

func artifactSelectorOf(artifact domain.Artifact) string {
	return string(artifact.Kind) + ":" + artifact.Name
}

// canUseArtifact mirrors the store's rule: a user-scoped layer is its
// subject's alone.
func canUseArtifact(actor string, artifact domain.Artifact) bool {
	return artifact.Scope != domain.ScopeUser || artifact.Subject == strings.ToLower(strings.TrimSpace(actor))
}

// endCapsuleRecording starts sealing and waits briefly. A small layer is done
// before the wait ends and the artifact comes back as it always did; a large
// one answers with its progress instead, and the caller follows the seal.
func (s *Server) endCapsuleRecording(recordingID string, req domain.EndRecordingRequest) (domain.Artifact, *domain.SealStatus, error) {
	job, err := s.startSeal(recordingID, req)
	if err != nil {
		return domain.Artifact{}, nil, err
	}
	status, finished := s.awaitSeal(job, s.sealWait)
	if !finished {
		return domain.Artifact{}, &status, nil
	}
	if status.Status == "error" {
		return domain.Artifact{}, nil, errors.New(status.Error)
	}
	return *status.Artifact, nil, nil
}

// archiveSealedSnapshot puts a sealed snapshot in the central archive. A
// remote engine has the runner upload it in 1 MiB chunks over HTTP, the same
// path a browser takes to restore a backup, so nothing large streams through
// the control plane and a dropped connection resumes instead of failing. A
// local engine still streams directly. A snapshot the archive already holds,
// from an attempt whose connection died after the runner finished, is kept.
func (s *Server) archiveSealedSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot) error {
	if !snapshot.Restorable || s.snapshotArchive == nil {
		return nil
	}
	if has, err := s.snapshotArchive.HasSnapshot(ctx, snapshot); err == nil && has {
		return nil
	}
	archiver, ok := s.engine.(capsule.SnapshotArchiver)
	if !ok {
		return s.archiveCapsuleSnapshot(ctx, snapshot)
	}
	if err := archiver.ArchiveSnapshot(ctx, snapshot); err != nil {
		return err
	}
	has, err := s.snapshotArchive.HasSnapshot(ctx, snapshot)
	if err != nil {
		return err
	}
	if !has {
		return errors.New("the runner reported the snapshot archived, but the archive does not hold it")
	}
	return nil
}

func (s *Server) archiveCapsuleSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot) error {
	if !snapshot.Restorable || s.snapshotArchive == nil {
		return nil
	}
	exporter, ok := s.engine.(capsule.SnapshotExporter)
	if !ok {
		return errors.New("Capsule engine cannot export its restorable snapshot")
	}
	reader, writer := io.Pipe()
	exported := make(chan error, 1)
	go func() {
		err := exporter.ExportSnapshot(ctx, snapshot, writer)
		_ = writer.CloseWithError(err)
		exported <- err
	}()
	storeErr := s.snapshotArchive.StoreSnapshot(ctx, snapshot, reader)
	if storeErr != nil {
		_ = reader.CloseWithError(storeErr)
	}
	exportErr := <-exported
	return errors.Join(storeErr, exportErr)
}

// cancelCapsuleRecording ends a recording without saving it. A start still
// under way is stopped first and cancels the recording itself; a recording
// that never got a capsule needs no runner to go away.
func (s *Server) cancelCapsuleRecording(ctx context.Context, recordingID string, req domain.CancelRecordingRequest) (domain.Recording, error) {
	if s.sealInProgress(recordingID) {
		return domain.Recording{}, fmt.Errorf("this recording is being saved; wait for the save to finish: %w", store.ErrConflict)
	}
	recording, err := s.store.Recording(recordingID)
	if err != nil {
		return domain.Recording{}, err
	}
	if normalizeOperator(req.Actor) != normalizeOperator(recording.Actor) {
		return domain.Recording{}, store.ErrConflict
	}
	if s.cancelStart(recordingID) {
		if recording, err = s.store.Recording(recordingID); err != nil {
			return domain.Recording{}, err
		}
		if recording.Status == domain.RecordingCancelled {
			return recording, nil
		}
	}
	s.stopTerminal(recordingID)
	open, err := s.store.OpenRecording(req.Actor)
	if err != nil || open.ID != recording.ID {
		return domain.Recording{}, store.ErrConflict
	}
	if recording.Runtime != nil && recording.Runtime.ContainerID != "" {
		if err := s.engine.Cancel(ctx, recording); err != nil {
			return domain.Recording{}, fmt.Errorf("remove capsule recording: %w", err)
		}
	}
	return s.store.CancelRecording(recordingID, req)
}

func (s *Server) attachCapsuleParent(ctx context.Context, recordingID string, req domain.AttachRecordingParentRequest) (domain.Recording, error) {
	if s.terminalBusy(recordingID) {
		return domain.Recording{}, fmt.Errorf("an interactive command is still running; wait for it or send Ctrl-C: %w", store.ErrConflict)
	}
	recording, err := s.store.Recording(recordingID)
	if err != nil {
		return domain.Recording{}, err
	}
	if len(recording.Commands) != 0 {
		return domain.Recording{}, fmt.Errorf("FROM must be set before the first capsule command: %w", store.ErrConflict)
	}
	if recording.Runtime != nil && recording.Runtime.Driver == "docker" && len(recording.ParentArtifactIDs) != 0 {
		return domain.Recording{}, fmt.Errorf("Docker recordings have one linear parent and this recording already has one: %w", store.ErrConflict)
	}
	updated, err := s.store.AttachRecordingParent(recordingID, req)
	if err != nil {
		return domain.Recording{}, err
	}
	if err := s.engine.Cancel(ctx, recording); err != nil {
		return domain.Recording{}, err
	}
	parents, err := s.recordingParents(updated)
	if err != nil {
		return domain.Recording{}, err
	}
	runtime, err := s.engine.StartRecording(ctx, updated, parents)
	if err != nil {
		return domain.Recording{}, fmt.Errorf("rebase capsule recording: %w", err)
	}
	return s.store.SetRecordingRuntime(updated.ID, updated.Actor, runtime)
}

func (s *Server) useCapsule(ctx context.Context, req domain.UseRequest) (domain.Composition, error) {
	composition, err := s.store.Use(req)
	if err != nil {
		return domain.Composition{}, err
	}
	// The logins are taken now, before the slow part: a credential layer
	// with every login in use has nothing for one more capsule, and says so
	// without building anything.
	if err := s.reserveLogins(composition); err != nil {
		_ = s.store.DiscardComposition(composition.ID, composition.Operator)
		return domain.Composition{}, err
	}
	artifacts := s.store.Snapshot().Artifacts
	var runtime domain.CapsuleRuntime
	if account, authenticated, accountErr := s.gitAccountForWorkspace(ctx, composition.Git, composition.Operator); accountErr != nil {
		_ = s.store.DiscardComposition(composition.ID, composition.Operator)
		return domain.Composition{}, fmt.Errorf("resolve Git account: %w", accountErr)
	} else if authenticated {
		materializer, ok := s.engine.(capsule.SecretMaterializer)
		if !ok {
			_ = s.store.DiscardComposition(composition.ID, composition.Operator)
			return domain.Composition{}, errors.New("capsule engine cannot receive transient Git authentication")
		}
		username := account.Login
		if account.Provider == "gitlab" {
			username = "oauth2"
		}
		runtime, err = materializer.MaterializeWithGitAuthentication(ctx, composition, artifacts, &capsule.GitAuthentication{
			Username: username, Password: account.AccessToken, AuthorName: account.Name, AuthorEmail: account.Email,
		})
	} else {
		runtime, err = s.engine.Materialize(ctx, composition, artifacts)
	}
	if err != nil {
		_ = s.store.DiscardComposition(composition.ID, composition.Operator)
		return domain.Composition{}, fmt.Errorf("materialize composition %s: %w", composition.ID, err)
	}
	if err := s.injectJobAttachments(ctx, composition, runtime); err != nil {
		_ = s.engine.Stop(context.Background(), runtime)
		_ = s.store.DiscardComposition(composition.ID, composition.Operator)
		return domain.Composition{}, fmt.Errorf("inject Job attachments into composition %s: %w", composition.ID, err)
	}
	materialized, err := s.store.SetCompositionRuntime(composition.ID, composition.Operator, runtime)
	if err != nil {
		_ = s.engine.Stop(context.Background(), runtime)
		return domain.Composition{}, err
	}
	// The capsule runs: the reserved logins go in.
	if err := s.placeLogins(ctx, materialized); err != nil {
		s.abandonCapsule(composition, runtime)
		return domain.Composition{}, err
	}
	if materialized, err = s.store.Composition(composition.ID); err != nil {
		return domain.Composition{}, err
	}
	if composition.SessionID != "" && runtime.ClientID != "" {
		if _, err := s.store.BindSessionClient(composition.SessionID, runtime.ClientID); err != nil {
			s.abandonCapsule(composition, runtime)
			return domain.Composition{}, fmt.Errorf("pin Session to runner: %w", err)
		}
	}
	return materialized, nil
}

// abandonCapsule gives up a capsule whose start failed after its runtime
// was recorded: the container goes and the composition is marked stopped.
// A composition with a runtime cannot be discarded, and one left "ready"
// without a container would be launched into forever ("No such
// container") and would hold its logins.
func (s *Server) abandonCapsule(composition domain.Composition, runtime domain.CapsuleRuntime) {
	_ = s.engine.Stop(context.Background(), runtime)
	runtime.Status = "stopped"
	if _, err := s.store.SetCompositionRuntime(composition.ID, composition.Operator, runtime); err != nil {
		s.logger.Warn("mark abandoned capsule stopped", "composition", composition.ID, "error", err)
	}
}

// forgetLostCapsule marks a Session's composition stopped when its
// container turned out to be gone, so the next launch builds a new one
// instead of failing into the same hole.
func (s *Server) forgetLostCapsule(sessionID string, err error) bool {
	if err == nil || !strings.Contains(err.Error(), "No such container") {
		return false
	}
	record, recordErr := s.sessionRecord(sessionID)
	if recordErr != nil {
		return false
	}
	_, composition, compositionErr := s.sessionComposition(sessionID, record.Operator)
	if compositionErr != nil || composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return false
	}
	runtime := *composition.Runtime
	runtime.Status = "stopped"
	if _, setErr := s.store.SetCompositionRuntime(composition.ID, composition.Operator, runtime); setErr != nil {
		s.logger.Warn("mark lost capsule stopped", "composition", composition.ID, "error", setErr)
		return false
	}
	s.logger.Warn("capsule container is gone; composition marked stopped", "composition", composition.ID, "session", sessionID, "container", runtime.ContainerID)
	return true
}

func (s *Server) stopCapsule(ctx context.Context, compositionID, operator string) (domain.Composition, error) {
	composition, err := s.store.Composition(compositionID)
	if err != nil {
		return domain.Composition{}, err
	}
	if composition.Operator != normalizeOperator(operator) {
		return domain.Composition{}, store.ErrConflict
	}
	if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return composition, nil
	}
	s.stopACPComposition(composition.ID)
	s.stopAppServicesForComposition(ctx, composition)
	if s.engineConnected(composition.Runtime.ClientID) {
		s.captureLoginState(ctx, composition)
	}
	if err := s.engine.Stop(ctx, *composition.Runtime); err != nil {
		if !errors.Is(err, worker.ErrRunnerOffline) {
			return domain.Composition{}, fmt.Errorf("stop composition capsule: %w", err)
		}
		// The runner is away; its capsule cannot be reached and is done
		// here. Comes it back, cleanup of stopped runtimes takes it.
		s.logger.Warn("composition stopped without its runner", "composition", composition.ID, "runner", composition.Runtime.ClientID)
	}
	runtime := *composition.Runtime
	runtime.Status = "stopped"
	stopped, err := s.store.SetCompositionRuntime(composition.ID, composition.Operator, runtime)
	if err == nil && len(composition.Logins) > 0 {
		// The logins this capsule held are free: a Job that waited for one
		// can start.
		go s.launchQueuedWorkflowPhases("login released")
	}
	return stopped, err
}

func normalizeOperator(value string) string {
	return strings.ToLower(strings.TrimSpace(value))
}

func (s *Server) recordingParents(recording domain.Recording) ([]domain.Artifact, error) {
	parents := make([]domain.Artifact, 0, len(recording.ParentArtifactIDs))
	for _, id := range recording.ParentArtifactIDs {
		parent, err := s.store.Artifact(id)
		if err != nil {
			return nil, err
		}
		parents = append(parents, parent)
	}
	return parents, nil
}

// engineConnected reports whether the runner of a runtime is reachable now;
// a local engine always is.
func (s *Server) engineConnected(clientID string) bool {
	if s.runnerBroker == nil {
		return true
	}
	return s.runnerBroker.Connected(clientID)
}
