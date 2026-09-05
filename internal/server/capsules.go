package server

import (
	"context"
	"errors"
	"fmt"
	"io"
	"strings"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

func (s *Server) createCapsuleRecording(ctx context.Context, req domain.CreateRecordingRequest) (domain.Recording, error) {
	recording, err := s.store.CreateRecording(req)
	if err != nil {
		return domain.Recording{}, err
	}
	parents, err := s.recordingParents(recording)
	if err != nil {
		_, _ = s.store.CancelRecording(recording.ID, domain.CancelRecordingRequest{Actor: recording.Actor})
		return domain.Recording{}, err
	}
	runtime, err := s.engine.StartRecording(ctx, recording, parents)
	if err != nil {
		_, _ = s.store.CancelRecording(recording.ID, domain.CancelRecordingRequest{Actor: recording.Actor})
		return domain.Recording{}, fmt.Errorf("start capsule recording: %w", err)
	}
	recording, err = s.store.SetRecordingRuntime(recording.ID, recording.Actor, runtime)
	if err != nil {
		_ = s.engine.Cancel(ctx, recording)
		return domain.Recording{}, err
	}
	return recording, nil
}

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

func (s *Server) cancelCapsuleRecording(ctx context.Context, recordingID string, req domain.CancelRecordingRequest) (domain.Recording, error) {
	if s.sealInProgress(recordingID) {
		return domain.Recording{}, fmt.Errorf("this recording is being saved; wait for END RECORD to finish: %w", store.ErrConflict)
	}
	s.stopTerminal(recordingID)
	recording, err := s.store.Recording(recordingID)
	if err != nil {
		return domain.Recording{}, err
	}
	open, err := s.store.OpenRecording(req.Actor)
	if err != nil || open.ID != recording.ID {
		return domain.Recording{}, store.ErrConflict
	}
	if err := s.engine.Cancel(ctx, recording); err != nil {
		return domain.Recording{}, fmt.Errorf("remove capsule recording: %w", err)
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
	if composition.SessionID != "" && runtime.ClientID != "" {
		if _, err := s.store.BindSessionClient(composition.SessionID, runtime.ClientID); err != nil {
			_ = s.engine.Stop(context.Background(), runtime)
			_ = s.store.DiscardComposition(composition.ID, composition.Operator)
			return domain.Composition{}, fmt.Errorf("pin Session to runner: %w", err)
		}
	}
	return materialized, nil
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
	if err := s.engine.Stop(ctx, *composition.Runtime); err != nil {
		return domain.Composition{}, fmt.Errorf("stop composition capsule: %w", err)
	}
	runtime := *composition.Runtime
	runtime.Status = "stopped"
	return s.store.SetCompositionRuntime(composition.ID, composition.Operator, runtime)
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
