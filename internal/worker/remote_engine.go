package worker

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"slices"
	"strings"
	"sync"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/persistence"
)

// RemoteEngine preserves the existing capsule.Engine boundary while moving
// every Docker operation behind the runner broker. Runtime and snapshot
// ClientIDs are the only placement knowledge that leaks into durable state.
type RemoteEngine struct {
	pullMu sync.Mutex
	pulls  map[string]*pullState

	broker  *Broker
	archive capsule.SnapshotArchive

	placementMu sync.Mutex
	placement   func(sessionID, clientID string)
}

func NewRemoteEngine(broker *Broker, archive ...capsule.SnapshotArchive) *RemoteEngine {
	engine := &RemoteEngine{broker: broker}
	if len(archive) > 0 {
		engine.archive = archive[0]
	}
	return engine
}

func (e *RemoteEngine) Info() domain.CapsuleEngineInfo { return e.broker.info() }

// OnPlacement registers an observer that learns which runner will materialize a
// Session before the slow part begins. Choosing a runner is instant once one is
// connected, while pulling images and checking out a repository is not, so
// without this a Job reports "waiting for a runner" for work already under way.
func (e *RemoteEngine) OnPlacement(observe func(sessionID, clientID string)) {
	e.placementMu.Lock()
	e.placement = observe
	e.placementMu.Unlock()
}

func (e *RemoteEngine) reportPlacement(sessionID, clientID string) {
	if sessionID == "" || clientID == "" {
		return
	}
	e.placementMu.Lock()
	observe := e.placement
	e.placementMu.Unlock()
	if observe != nil {
		observe(sessionID, clientID)
	}
}

func (e *RemoteEngine) StartRecording(ctx context.Context, recording domain.Recording, parents []domain.Artifact) (domain.CapsuleRuntime, error) {
	target, err := e.broker.choose(ctx, "")
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	for index := range parents {
		parent := &parents[index]
		if !parent.Snapshot.Restorable || parent.Snapshot.Ref == "" || parent.SnapshotPrunedAt != nil || snapshotAvailableOn(parent.Snapshot, target.id) {
			continue
		}
		if err := e.ensureSnapshotOn(ctx, *parent, target.id); err != nil {
			return domain.CapsuleRuntime{}, fmt.Errorf("provide parent %s to runner %s: %w", parent.ID, target.id, err)
		}
		parent.Snapshot.ReplicaClientIDs = append(parent.Snapshot.ReplicaClientIDs, target.id)
	}
	capsule.ReportProgress(ctx, "start", "Capsule starten op runner "+target.name, 0, 0)
	var runtime domain.CapsuleRuntime
	peer, err := e.broker.call(ctx, target.id, methodStartRecording, startRecordingPayload{Recording: recording, Parents: parents}, &runtime)
	if err != nil {
		if ctx.Err() != nil {
			// The start was abandoned while the runner may have been creating
			// the capsule; have it take the capsule down again.
			go func() {
				cleanup, cancel := context.WithTimeout(context.Background(), time.Minute)
				defer cancel()
				_, _ = e.broker.call(cleanup, target.id, methodCancelRecording, recordingPayload{Recording: recording}, nil)
			}()
		}
		return domain.CapsuleRuntime{}, err
	}
	runtime.ClientID = peer.id
	peer.addWorkload(1)
	return runtime, nil
}

func (e *RemoteEngine) Execute(ctx context.Context, recording domain.Recording, input string) (capsule.Execution, error) {
	var execution capsule.Execution
	_, err := e.broker.call(ctx, recordingAffinity(recording), methodExecute, executePayload{Recording: recording, Input: input}, &execution)
	return execution, err
}

func (e *RemoteEngine) Seal(ctx context.Context, recording domain.Recording) (domain.CapsuleSnapshot, error) {
	affinity := recordingAffinity(recording)
	var snapshot domain.CapsuleSnapshot
	peer, err := e.broker.call(ctx, affinity, methodSeal, recordingPayload{Recording: recording}, &snapshot)
	if err != nil {
		return domain.CapsuleSnapshot{}, err
	}
	snapshot.ClientID = peer.id
	peer.addWorkload(-1)
	e.broker.notifyAvailable()
	return snapshot, nil
}

// Cancel removes the recording's capsule on the runner that holds it. A
// recording that never reached a runner has nothing to remove anywhere, and
// must not wait for a runner to say so.
func (e *RemoteEngine) Cancel(ctx context.Context, recording domain.Recording) error {
	affinity := recordingAffinity(recording)
	if affinity == "" {
		return nil
	}
	peer, err := e.broker.call(ctx, affinity, methodCancelRecording, recordingPayload{Recording: recording}, nil)
	if err == nil {
		peer.addWorkload(-1)
		e.broker.notifyAvailable()
	}
	return err
}

func (e *RemoteEngine) MergeWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, merge capsule.WorkspaceMerge) (capsule.WorkspaceMergeResult, error) {
	var result capsule.WorkspaceMergeResult
	_, err := e.broker.call(ctx, runtime.ClientID, methodMergeWorkspace, mergePayload{Runtime: runtime, Merge: merge}, &result)
	return result, err
}

func (e *RemoteEngine) SyncWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, sync capsule.WorkspaceSync) (capsule.WorkspaceSyncResult, error) {
	var result capsule.WorkspaceSyncResult
	_, err := e.broker.call(ctx, runtime.ClientID, methodSyncWorkspace, syncPayload{Runtime: runtime, Sync: sync}, &result)
	return result, err
}

// BrowseRepository runs on any connected runner; each keeps its own clone.
func (e *RemoteEngine) BrowseRepository(ctx context.Context, browse capsule.RepositoryBrowse) (capsule.RepositoryBrowseResult, error) {
	var result capsule.RepositoryBrowseResult
	target, err := e.broker.choose(ctx, "")
	if err != nil {
		return result, err
	}
	_, err = e.broker.call(ctx, target.id, methodBrowseRepository, repositoryBrowsePayload{Browse: browse}, &result)
	return result, err
}

// CompareRepository runs on any runner: the clone is the runner's own and
// the remote is the source of truth.
func (e *RemoteEngine) CompareRepository(ctx context.Context, comparison capsule.RepositoryComparison) (capsule.WorkspaceChanges, error) {
	var changes capsule.WorkspaceChanges
	target, err := e.broker.choose(ctx, "")
	if err != nil {
		return changes, err
	}
	_, err = e.broker.call(ctx, target.id, methodCompareRepository, repositoryComparePayload{Comparison: comparison}, &changes)
	return changes, err
}

// App services live on the runner that holds the Session's capsule; the
// runtime's ClientID is the affinity for all four calls.
func (e *RemoteEngine) StartAppServices(ctx context.Context, runtime domain.CapsuleRuntime, sessionID string, services []domain.AppService, hosts []string) ([]domain.AppServiceRuntime, error) {
	var result appStatusResult
	_, err := e.broker.call(ctx, runtime.ClientID, methodStartApp, appPayload{Runtime: runtime, SessionID: sessionID, Services: services, Hosts: hosts}, &result)
	return result.Services, err
}

func (e *RemoteEngine) StopAppServicesOn(ctx context.Context, clientID, sessionID string) error {
	if clientID == "" {
		return nil
	}
	_, err := e.broker.call(ctx, clientID, methodStopApp, appPayload{SessionID: sessionID}, nil)
	return err
}

func (e *RemoteEngine) AppServiceStatusOn(ctx context.Context, clientID, sessionID string) ([]domain.AppServiceRuntime, error) {
	var result appStatusResult
	_, err := e.broker.call(ctx, clientID, methodAppStatus, appPayload{SessionID: sessionID}, &result)
	return result.Services, err
}

func (e *RemoteEngine) AppServiceLogsOn(ctx context.Context, clientID, sessionID, service string, tail int) (string, error) {
	var result appLogsResult
	_, err := e.broker.call(ctx, clientID, methodAppLogs, appPayload{SessionID: sessionID, Service: service, Tail: tail}, &result)
	return result.Output, err
}

func (e *RemoteEngine) Materialize(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact) (domain.CapsuleRuntime, error) {
	return e.materialize(ctx, composition, artifacts, nil)
}

func (e *RemoteEngine) MaterializeWithGitAuthentication(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact, authentication *capsule.GitAuthentication) (domain.CapsuleRuntime, error) {
	return e.materialize(ctx, composition, artifacts, authentication)
}

func (e *RemoteEngine) materialize(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact, authentication *capsule.GitAuthentication) (domain.CapsuleRuntime, error) {
	// Only the base and the layers above it have to reach the runner; a
	// layer under the base is inside its image.
	needed := artifacts
	if plan, err := capsule.PlanLayers(composition, artifacts); err == nil {
		needed = plan.Needed()
	}
	target, err := e.broker.choosePreferring(ctx, "", func(clientID string) bool {
		return snapshotsAvailableOn(needed, clientID)
	})
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	e.reportPlacement(composition.SessionID, target.id)
	for index := range artifacts {
		artifact := &artifacts[index]
		if !slices.ContainsFunc(needed, func(candidate domain.Artifact) bool { return candidate.ID == artifact.ID }) {
			continue
		}
		// A pruned snapshot belongs to a superseded version: its newer
		// version is in the list and carries its content, so it is never
		// applied on its own and does not have to reach the runner.
		if !artifact.Snapshot.Restorable || artifact.Snapshot.Ref == "" || artifact.SnapshotPrunedAt != nil || snapshotAvailableOn(artifact.Snapshot, target.id) {
			continue
		}
		if err := e.ensureSnapshotOn(ctx, *artifact, target.id); err != nil {
			return domain.CapsuleRuntime{}, fmt.Errorf("provide %s to runner %s: %w", artifact.ID, target.id, err)
		}
		artifact.Snapshot.ReplicaClientIDs = append(artifact.Snapshot.ReplicaClientIDs, target.id)
	}
	capsule.ReportProgress(ctx, "start", "Workspace en capsule starten op runner "+target.name, 0, 0)
	var runtime domain.CapsuleRuntime
	peer, err := e.broker.call(ctx, target.id, methodMaterialize, materializePayload{Composition: composition, Artifacts: artifacts, Authentication: authentication}, &runtime)
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	runtime.ClientID = peer.id
	peer.addWorkload(1)
	return runtime, nil
}

// ArchiveSnapshot asks the runner that holds the snapshot to upload it to the
// central archive in chunks. The call itself is small; the bytes travel over
// HTTP beside the runner socket instead of through it.
func (e *RemoteEngine) ArchiveSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot) error {
	var result archiveResult
	_, err := e.broker.call(ctx, snapshot.ClientID, methodArchiveSnapshot, snapshotPayload{Snapshot: snapshot}, &result)
	return err
}

func (e *RemoteEngine) ExportSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot, destination io.Writer) error {
	process, err := e.broker.openStream(ctx, snapshot.ClientID, methodExportSnapshot, snapshotPayload{Snapshot: snapshot})
	if err != nil {
		return err
	}
	_, copyErr := io.Copy(destination, process)
	execution, waitErr := process.Wait()
	return errors.Join(copyErr, waitErr, executionError("snapshot export", execution))
}

func (e *RemoteEngine) Stop(ctx context.Context, runtime domain.CapsuleRuntime) error {
	peer, err := e.broker.call(ctx, runtime.ClientID, methodStop, runtimePayload{Runtime: runtime}, nil)
	if err == nil {
		peer.addWorkload(-1)
		e.broker.notifyAvailable()
	}
	return err
}

func (e *RemoteEngine) ProbeEnabled(ctx context.Context, runtime domain.CapsuleRuntime, enablement domain.Enablement, request json.RawMessage) (json.RawMessage, error) {
	var response json.RawMessage
	_, err := e.broker.call(ctx, runtime.ClientID, methodProbeEnabled, enabledPayload{Runtime: runtime, Enablement: enablement, Request: request}, &response)
	return response, err
}

func (e *RemoteEngine) StartEnabled(ctx context.Context, runtime domain.CapsuleRuntime, enablement domain.Enablement) (capsule.EnabledProcess, error) {
	return e.broker.openStream(ctx, runtime.ClientID, methodStartEnabled, enabledPayload{Runtime: runtime, Enablement: enablement})
}

func (e *RemoteEngine) StartInteractive(ctx context.Context, recording domain.Recording, input string, rows, cols uint16) (capsule.InteractiveProcess, error) {
	return e.broker.openStream(ctx, recordingAffinity(recording), methodStartInteractive, interactivePayload{Recording: recording, Input: input, Rows: rows, Cols: cols})
}

func (e *RemoteEngine) InspectWorkspace(ctx context.Context, runtime domain.CapsuleRuntime) (capsule.WorkspaceChanges, error) {
	var changes capsule.WorkspaceChanges
	_, err := e.broker.call(ctx, runtime.ClientID, methodInspectWorkspace, runtimePayload{Runtime: runtime}, &changes)
	return changes, err
}

func (e *RemoteEngine) InspectWorkspaceRange(ctx context.Context, runtime domain.CapsuleRuntime, comparison capsule.WorkspaceComparison) (capsule.WorkspaceChanges, error) {
	var changes capsule.WorkspaceChanges
	_, err := e.broker.call(ctx, runtime.ClientID, methodInspectRange, inspectRangePayload{Runtime: runtime, Comparison: comparison}, &changes)
	return changes, err
}

func (e *RemoteEngine) CaptureCapsuleChanges(ctx context.Context, runtime domain.CapsuleRuntime) (domain.LayerContents, error) {
	var changes domain.LayerContents
	_, err := e.broker.call(ctx, runtime.ClientID, methodCapsuleChanges, runtimePayload{Runtime: runtime}, &changes)
	return changes, err
}

func (e *RemoteEngine) CaptureLoginState(ctx context.Context, runtime domain.CapsuleRuntime, credential domain.CapsuleSnapshot) (map[string][]byte, error) {
	var files map[string][]byte
	_, err := e.broker.call(ctx, runtime.ClientID, methodCaptureLogin, homeFilesPayload{Runtime: runtime, Credential: credential}, &files)
	return files, err
}

func (e *RemoteEngine) WriteHomeFiles(ctx context.Context, runtime domain.CapsuleRuntime, files map[string][]byte) error {
	_, err := e.broker.call(ctx, runtime.ClientID, methodWriteHomeFiles, homeFilesPayload{Runtime: runtime, Files: files}, nil)
	return err
}

func (e *RemoteEngine) InjectWorkspaceAttachments(ctx context.Context, runtime domain.CapsuleRuntime, attachments []capsule.WorkspaceAttachment) error {
	for _, attachment := range attachments {
		data := attachment.Data
		if data == nil {
			info, err := os.Stat(attachment.SourcePath)
			if err != nil {
				return err
			}
			if info.Size() > 15<<20 {
				return errors.New("runner attachment exceeds the 15 MiB upload limit")
			}
			data, err = os.ReadFile(attachment.SourcePath)
			if err != nil {
				return err
			}
		}
		if len(data) > 15<<20 {
			return errors.New("runner attachment exceeds the 15 MiB upload limit")
		}
		payload := injectAttachmentsPayload{Runtime: runtime, Attachments: []attachmentPayload{{TargetPath: attachment.TargetPath, Data: data}}}
		if _, err := e.broker.call(ctx, runtime.ClientID, methodInjectAttachments, payload, nil); err != nil {
			return err
		}
	}
	return nil
}

func (e *RemoteEngine) AcceptWorkspace(ctx context.Context, runtime domain.CapsuleRuntime, acceptance capsule.WorkspaceAcceptance) (capsule.WorkspaceAcceptanceResult, error) {
	var result capsule.WorkspaceAcceptanceResult
	_, err := e.broker.call(ctx, runtime.ClientID, methodAcceptWorkspace, acceptWorkspacePayload{Runtime: runtime, Acceptance: acceptance}, &result)
	return result, err
}

func (e *RemoteEngine) RemoveSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot) error {
	clientIDs := append([]string{snapshot.ClientID}, snapshot.ReplicaClientIDs...)
	seen := map[string]bool{}
	for _, clientID := range clientIDs {
		clientID = strings.TrimSpace(clientID)
		if seen[clientID] {
			continue
		}
		seen[clientID] = true
		if _, err := e.broker.call(ctx, clientID, methodRemoveSnapshot, snapshotPayload{Snapshot: snapshot}, nil); err != nil {
			return err
		}
	}
	if e.archive != nil {
		return e.archive.RemoveArchivedSnapshot(ctx, snapshot)
	}
	return nil
}

func (e *RemoteEngine) replicateSnapshot(ctx context.Context, artifact domain.Artifact, targetID string) error {
	sourceID := e.broker.snapshotSource(artifact.Snapshot, targetID)
	if sourceID == "" {
		// Legacy snapshots predate runner affinity. The chosen runner may share
		// the old daemon; let Docker resolve the ref normally.
		return nil
	}
	importProcess, err := e.broker.openStream(ctx, targetID, methodImportSnapshot, snapshotPayload{Snapshot: artifact.Snapshot})
	if err == nil {
		importProcess.bulk = true
	}
	if err != nil {
		return err
	}
	exportProcess, err := e.broker.openStream(ctx, sourceID, methodExportSnapshot, snapshotPayload{Snapshot: artifact.Snapshot})
	if err == nil {
		exportProcess.bulk = true
	}
	if err != nil {
		_ = importProcess.Close()
		_, _ = importProcess.Wait()
		return err
	}
	cancelTransfers := make(chan struct{})
	go func() {
		select {
		case <-ctx.Done():
			_ = exportProcess.Close()
			_ = importProcess.Close()
		case <-cancelTransfers:
		}
	}()
	_, copyErr := io.Copy(importProcess, exportProcess)
	close(cancelTransfers)
	closeErr := importProcess.Close()
	exportExecution, exportErr := exportProcess.Wait()
	importExecution, importErr := importProcess.Wait()
	if copyErr != nil || closeErr != nil || exportErr != nil || importErr != nil || exportExecution.ExitCode != 0 || importExecution.ExitCode != 0 {
		return errors.Join(copyErr, closeErr, exportErr, importErr,
			executionError("snapshot export", exportExecution), executionError("snapshot import", importExecution))
	}
	_, err = e.broker.store.AddSnapshotReplica(artifact.ID, targetID)
	return err
}

// runnerHasSnapshot asks the target whether it already holds the image. The
// durable placement may be stale, after a restore for instance, and asking is
// far cheaper than shipping a gigabyte that is already there.
func (e *RemoteEngine) runnerHasSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot, targetID string) bool {
	var presence presenceResult
	if _, err := e.broker.call(ctx, targetID, methodHasSnapshot, snapshotPayload{Snapshot: snapshot}, &presence); err != nil {
		return false
	}
	return presence.Present
}

// snapshotSizer is what an archive offers beyond the capsule interface: the
// size of a snapshot, so a restore can report how far it is.
type snapshotSizer interface {
	SnapshotInfo(context.Context, domain.CapsuleSnapshot) (persistence.BlobInfo, error)
}

type progressWriter struct {
	io.Writer
	written int64
	total   int64
	ctx     context.Context
}

func (w *progressWriter) Write(p []byte) (int, error) {
	n, err := w.Writer.Write(p)
	w.written += int64(n)
	capsule.ReportProgress(w.ctx, "parents", "Basisimage uit het archief naar de runner", w.written, w.total)
	return n, err
}

func (e *RemoteEngine) ensureSnapshotOn(ctx context.Context, artifact domain.Artifact, targetID string) error {
	if e.runnerHasSnapshot(ctx, artifact.Snapshot, targetID) {
		_, err := e.broker.store.AddSnapshotReplica(artifact.ID, targetID)
		return err
	}
	// A delta is rebuilt from its parent: the parent goes first.
	if artifact.Snapshot.Delta && artifact.Snapshot.ParentRef != "" {
		parent, ok := e.parentArtifact(artifact)
		if !ok {
			return fmt.Errorf("parent %s of %s is not a known layer", artifact.Snapshot.ParentRef, artifact.ID)
		}
		if err := e.ensureSnapshotOn(ctx, parent, targetID); err != nil {
			return fmt.Errorf("parent of %s: %w", artifact.ID, err)
		}
	}
	var replicaErr error
	connectedSource := e.broker.connectedSnapshotSource(artifact.Snapshot, targetID)
	if e.archive == nil || connectedSource != "" {
		replicaErr = e.replicateSnapshot(ctx, artifact, targetID)
		if replicaErr == nil && connectedSource != "" {
			return nil
		}
	}
	if e.archive == nil {
		return replicaErr
	}
	hasSnapshot, archiveErr := e.archive.HasSnapshot(ctx, artifact.Snapshot)
	if archiveErr != nil {
		return errors.Join(replicaErr, archiveErr)
	}
	if !hasSnapshot {
		if replicaErr != nil {
			return replicaErr
		}
		// Legacy state without runner affinity may still share the target
		// daemon. Preserve that compatibility until it has been archived.
		if !snapshotHasPlacement(artifact.Snapshot) {
			return nil
		}
		return errors.New("snapshot is absent from the central archive and every known runner is offline")
	}
	var total int64
	if sizer, ok := e.archive.(snapshotSizer); ok {
		if info, err := sizer.SnapshotInfo(ctx, artifact.Snapshot); err == nil {
			total = info.Size
		}
	}
	// A runner that can pull fetches the image itself in resumable HTTP
	// chunks; the link only carries its progress. Older runners get the
	// image pushed over the link.
	_, archiveServesChunks := e.archive.(snapshotChunkArchive)
	if archiveServesChunks && e.broker.supportsSnapshotMode(targetID, snapshotModePull) {
		if err := e.pullSnapshotOn(ctx, artifact, targetID, total); err != nil {
			return errors.Join(replicaErr, err)
		}
		_, replicaAddErr := e.broker.store.AddSnapshotReplica(artifact.ID, targetID)
		return replicaAddErr
	}
	process, err := e.broker.openStream(ctx, targetID, methodImportSnapshot, snapshotPayload{Snapshot: artifact.Snapshot})
	if err != nil {
		return errors.Join(replicaErr, err)
	}
	process.bulk = true
	capsule.ReportProgress(ctx, "parents", "Basisimage uit het archief naar de runner", 0, total)
	restoreErr := e.archive.RestoreSnapshot(ctx, artifact.Snapshot, &progressWriter{Writer: process, total: total, ctx: ctx})
	closeErr := process.Close()
	if restoreErr == nil && closeErr == nil {
		capsule.ReportProgress(ctx, "load", "Runner laadt de image in Docker", 0, 0)
	}
	execution, waitErr := process.Wait()
	if err := errors.Join(restoreErr, closeErr, waitErr, executionError("snapshot import", execution)); err != nil {
		return errors.Join(replicaErr, err)
	}
	_, err = e.broker.store.AddSnapshotReplica(artifact.ID, targetID)
	return err
}

// parentArtifact finds the layer a delta was recorded on: among its
// parents, or any layer whose image is the recorded parent.
func (e *RemoteEngine) parentArtifact(artifact domain.Artifact) (domain.Artifact, bool) {
	for _, parentID := range artifact.ParentArtifactIDs {
		parent, err := e.broker.store.Artifact(parentID)
		if err == nil && parent.Snapshot.Ref == artifact.Snapshot.ParentRef {
			return parent, true
		}
	}
	for _, candidate := range e.broker.store.Snapshot().Artifacts {
		if candidate.Snapshot.Ref == artifact.Snapshot.ParentRef {
			return candidate, true
		}
	}
	return domain.Artifact{}, false
}

// snapshotChunkArchive is an archive the runner can pull from over HTTP.
type snapshotChunkArchive interface {
	ReadSnapshotChunk(context.Context, domain.CapsuleSnapshot, int64) ([]byte, persistence.BlobInfo, error)
}

// pullState is one image being fetched by one runner. Callers join it and
// wait with their own deadline; the fetch itself runs on, so a caller that
// gives up (a comparison request under a proxy limit) does not throw away
// minutes of download, and the next caller finds the image there.
type pullState struct {
	done chan struct{}
	err  error
}

// pullSnapshotOn has the runner fetch the archived snapshot, once per
// runner and image at a time, and relays progress to the first caller.
func (e *RemoteEngine) pullSnapshotOn(ctx context.Context, artifact domain.Artifact, targetID string, total int64) error {
	key := targetID + " " + artifact.Snapshot.Digest
	e.pullMu.Lock()
	if e.pulls == nil {
		e.pulls = map[string]*pullState{}
	}
	state, joined := e.pulls[key]
	if !joined {
		state = &pullState{done: make(chan struct{})}
		e.pulls[key] = state
	}
	e.pullMu.Unlock()
	if !joined {
		go func() {
			background, cancel := context.WithTimeout(context.Background(), 45*time.Minute)
			defer cancel()
			state.err = e.runPull(background, ctx, artifact, targetID, total)
			e.pullMu.Lock()
			delete(e.pulls, key)
			e.pullMu.Unlock()
			close(state.done)
		}()
	}
	select {
	case <-state.done:
		return state.err
	case <-ctx.Done():
		return fmt.Errorf("runner %s is still fetching %s from the archive; the fetch continues and the next attempt finds it: %w", targetID, artifact.Snapshot.Ref, ctx.Err())
	}
}

// runPull is the fetch itself: the stream lives on the background context,
// progress goes to the caller that started it for as long as it listens.
func (e *RemoteEngine) runPull(background, caller context.Context, artifact domain.Artifact, targetID string, total int64) error {
	process, err := e.broker.openStream(background, targetID, methodPullSnapshot, snapshotPullPayload{Snapshot: artifact.Snapshot, Size: total})
	if err != nil {
		return err
	}
	report := func(stage, message string, current, size int64) {
		if caller.Err() == nil {
			capsule.ReportProgress(caller, stage, message, current, size)
		}
	}
	report("parents", "Runner haalt de basisimage uit het archief", 0, total)
	scanner := bufio.NewScanner(process)
	for scanner.Scan() {
		if received, size, ok := parsePullProgress(scanner.Text()); ok {
			if size > 0 {
				total = size
			}
			report("parents", "Runner haalt de basisimage uit het archief", received, total)
		}
	}
	report("load", "Runner laadt de image in Docker", 0, 0)
	execution, waitErr := process.Wait()
	return errors.Join(waitErr, executionError("snapshot pull", execution))
}

func executionError(operation string, execution capsule.Execution) error {
	if execution.ExitCode == 0 {
		return nil
	}
	return fmt.Errorf("%s exited with %d: %s", operation, execution.ExitCode, execution.Output)
}

// snapshotsAvailableOn tells whether a runner already holds every image a
// workspace needs, so starting there ships nothing.
func snapshotsAvailableOn(artifacts []domain.Artifact, clientID string) bool {
	for _, artifact := range artifacts {
		if !artifact.Snapshot.Restorable || artifact.Snapshot.Ref == "" || artifact.SnapshotPrunedAt != nil {
			continue
		}
		if !snapshotAvailableOn(artifact.Snapshot, clientID) {
			return false
		}
	}
	return true
}

func snapshotAvailableOn(snapshot domain.CapsuleSnapshot, clientID string) bool {
	return snapshot.ClientID == clientID || slices.Contains(snapshot.ReplicaClientIDs, clientID)
}

func snapshotHasPlacement(snapshot domain.CapsuleSnapshot) bool {
	if strings.TrimSpace(snapshot.ClientID) != "" {
		return true
	}
	for _, clientID := range snapshot.ReplicaClientIDs {
		if strings.TrimSpace(clientID) != "" {
			return true
		}
	}
	return false
}

func recordingAffinity(recording domain.Recording) string {
	if recording.Runtime == nil {
		return ""
	}
	return recording.Runtime.ClientID
}

func artifactAffinity(artifacts []domain.Artifact) (string, error) {
	affinity := ""
	for _, artifact := range artifacts {
		clientID := strings.TrimSpace(artifact.Snapshot.ClientID)
		if clientID == "" {
			continue
		}
		if affinity != "" && affinity != clientID {
			return "", fmt.Errorf("snapshots are pinned to different runners (%s and %s); publish them to a shared registry before composing", affinity, clientID)
		}
		affinity = clientID
	}
	return affinity, nil
}

func (b *Broker) snapshotSource(snapshot domain.CapsuleSnapshot, targetID string) string {
	if connected := b.connectedSnapshotSource(snapshot, targetID); connected != "" {
		return connected
	}
	candidates := append([]string{snapshot.ClientID}, snapshot.ReplicaClientIDs...)
	for _, clientID := range candidates {
		if clientID != "" && clientID != targetID {
			return clientID
		}
	}
	return ""
}

func (b *Broker) connectedSnapshotSource(snapshot domain.CapsuleSnapshot, targetID string) string {
	candidates := append([]string{snapshot.ClientID}, snapshot.ReplicaClientIDs...)
	b.mu.Lock()
	defer b.mu.Unlock()
	for _, clientID := range candidates {
		if clientID == "" || clientID == targetID {
			continue
		}
		if peer := b.peers[clientID]; peer != nil && peer.connectedForAffinity() {
			return clientID
		}
	}
	return ""
}

func (b *Broker) openStream(ctx context.Context, affinity, method string, request any) (*remoteProcess, error) {
	peer, err := b.choose(ctx, affinity)
	if err != nil {
		return nil, err
	}
	payload, err := json.Marshal(request)
	if err != nil {
		return nil, err
	}
	id := b.requestID("str")
	process := newRemoteProcess(peer, id)
	message := wireMessage{Version: ProtocolVersion, Type: messageRequest, ID: id, Method: method, Payload: payload}
	result := make(chan wireMessage, 1)
	peer.mu.Lock()
	peer.streams[id] = process
	peer.pending[id] = pendingCall{request: message, result: result}
	peer.mu.Unlock()
	if err := peer.enqueue(message); err != nil {
		peer.mu.Lock()
		delete(peer.streams, id)
		delete(peer.pending, id)
		peer.mu.Unlock()
		return nil, err
	}
	select {
	case <-ctx.Done():
		peer.mu.Lock()
		delete(peer.streams, id)
		delete(peer.pending, id)
		peer.mu.Unlock()
		_ = peer.enqueue(wireMessage{Version: ProtocolVersion, Type: messageCancel, ID: id})
		return nil, ctx.Err()
	case response := <-result:
		if response.Error != "" {
			peer.mu.Lock()
			delete(peer.streams, id)
			peer.mu.Unlock()
			return nil, errors.New(response.Error)
		}
		return process, nil
	}
}

type remoteProcess struct {
	peer     *runnerPeer
	id       string
	data     chan []byte
	done     chan struct{}
	doneOnce sync.Once
	// bulk marks a one-shot transfer (an image on its way to a runner). Its
	// stream messages are not replayed, so a lost connection means lost
	// bytes: the transfer fails at once rather than waiting for an end that
	// cannot come. A PTY stream, by contrast, survives the reconnect.
	bulk bool

	readMu sync.Mutex
	buffer bytes.Buffer
	result capsule.Execution
	err    error
}

func newRemoteProcess(peer *runnerPeer, id string) *remoteProcess {
	return &remoteProcess{peer: peer, id: id, data: make(chan []byte, 256), done: make(chan struct{})}
}

func (p *remoteProcess) deliver(data []byte) {
	copyOfData := append([]byte(nil), data...)
	select {
	case p.data <- copyOfData:
	case <-p.done:
	}
}

func (p *remoteProcess) finish(execution *capsule.Execution, message string) {
	p.doneOnce.Do(func() {
		if execution != nil {
			p.result = *execution
		} else {
			p.result.ExitCode = -1
		}
		if message != "" {
			p.err = errors.New(message)
		}
		close(p.done)
	})
}

func (p *remoteProcess) Read(target []byte) (int, error) {
	p.readMu.Lock()
	defer p.readMu.Unlock()
	for p.buffer.Len() == 0 {
		select {
		case data := <-p.data:
			_, _ = p.buffer.Write(data)
		case <-p.done:
			select {
			case data := <-p.data:
				_, _ = p.buffer.Write(data)
			default:
				return 0, io.EOF
			}
		}
	}
	return p.buffer.Read(target)
}

func (p *remoteProcess) Write(data []byte) (int, error) {
	select {
	case <-p.done:
		return 0, io.ErrClosedPipe
	default:
	}
	if err := p.peer.enqueue(wireMessage{Version: ProtocolVersion, Type: messageStreamInput, ID: p.id, Data: append([]byte(nil), data...)}); err != nil {
		return 0, err
	}
	return len(data), nil
}

func (p *remoteProcess) Resize(rows, cols uint16) error {
	return p.peer.enqueue(wireMessage{Version: ProtocolVersion, Type: messageStreamResize, ID: p.id, Rows: rows, Cols: cols})
}

func (p *remoteProcess) Close() error {
	return p.peer.enqueue(wireMessage{Version: ProtocolVersion, Type: messageStreamClose, ID: p.id})
}

func (p *remoteProcess) Wait() (capsule.Execution, error) {
	<-p.done
	return p.result, p.err
}

var (
	_ capsule.Engine                      = (*RemoteEngine)(nil)
	_ capsule.SecretMaterializer          = (*RemoteEngine)(nil)
	_ capsule.InteractiveEngine           = (*RemoteEngine)(nil)
	_ capsule.WorkspaceSyncer             = (*RemoteEngine)(nil)
	_ capsule.RepositoryBrowser           = (*RemoteEngine)(nil)
	_ capsule.RepositoryComparer          = (*RemoteEngine)(nil)
	_ capsule.LoginState                  = (*RemoteEngine)(nil)
	_ capsule.CapsuleInspector            = (*RemoteEngine)(nil)
	_ capsule.EnabledEngine               = (*RemoteEngine)(nil)
	_ capsule.EnabledProber               = (*RemoteEngine)(nil)
	_ capsule.WorkspaceInspector          = (*RemoteEngine)(nil)
	_ capsule.WorkspaceRangeInspector     = (*RemoteEngine)(nil)
	_ capsule.WorkspaceAttachmentInjector = (*RemoteEngine)(nil)
	_ capsule.WorkspaceAcceptor           = (*RemoteEngine)(nil)
	_ capsule.SnapshotRemover             = (*RemoteEngine)(nil)
)
