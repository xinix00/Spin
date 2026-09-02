package worker

import (
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

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

// RemoteEngine preserves the existing capsule.Engine boundary while moving
// every Docker operation behind the runner broker. Runtime and snapshot
// ClientIDs are the only placement knowledge that leaks into durable state.
type RemoteEngine struct {
	broker  *Broker
	archive capsule.SnapshotArchive
}

func NewRemoteEngine(broker *Broker, archive ...capsule.SnapshotArchive) *RemoteEngine {
	engine := &RemoteEngine{broker: broker}
	if len(archive) > 0 {
		engine.archive = archive[0]
	}
	return engine
}

func (e *RemoteEngine) Info() domain.CapsuleEngineInfo { return e.broker.info() }

func (e *RemoteEngine) StartRecording(ctx context.Context, recording domain.Recording, parents []domain.Artifact) (domain.CapsuleRuntime, error) {
	target, err := e.broker.choose(ctx, "")
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	for index := range parents {
		parent := &parents[index]
		if !parent.Snapshot.Restorable || parent.Snapshot.Ref == "" || snapshotAvailableOn(parent.Snapshot, target.id) {
			continue
		}
		if err := e.ensureSnapshotOn(ctx, *parent, target.id); err != nil {
			return domain.CapsuleRuntime{}, fmt.Errorf("provide parent %s to runner %s: %w", parent.ID, target.id, err)
		}
		parent.Snapshot.ReplicaClientIDs = append(parent.Snapshot.ReplicaClientIDs, target.id)
	}
	var runtime domain.CapsuleRuntime
	peer, err := e.broker.call(ctx, target.id, methodStartRecording, startRecordingPayload{Recording: recording, Parents: parents}, &runtime)
	if err != nil {
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

func (e *RemoteEngine) Cancel(ctx context.Context, recording domain.Recording) error {
	affinity := recordingAffinity(recording)
	peer, err := e.broker.call(ctx, affinity, methodCancelRecording, recordingPayload{Recording: recording}, nil)
	if err == nil {
		peer.addWorkload(-1)
		e.broker.notifyAvailable()
	}
	return err
}

func (e *RemoteEngine) Materialize(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact) (domain.CapsuleRuntime, error) {
	return e.materialize(ctx, composition, artifacts, nil)
}

func (e *RemoteEngine) MaterializeWithGitAuthentication(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact, authentication *capsule.GitAuthentication) (domain.CapsuleRuntime, error) {
	return e.materialize(ctx, composition, artifacts, authentication)
}

func (e *RemoteEngine) materialize(ctx context.Context, composition domain.Composition, artifacts []domain.Artifact, authentication *capsule.GitAuthentication) (domain.CapsuleRuntime, error) {
	target, err := e.broker.choose(ctx, "")
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	for index := range artifacts {
		artifact := &artifacts[index]
		if !artifact.Snapshot.Restorable || artifact.Snapshot.Ref == "" || snapshotAvailableOn(artifact.Snapshot, target.id) {
			continue
		}
		if err := e.ensureSnapshotOn(ctx, *artifact, target.id); err != nil {
			return domain.CapsuleRuntime{}, fmt.Errorf("provide %s to runner %s: %w", artifact.ID, target.id, err)
		}
		artifact.Snapshot.ReplicaClientIDs = append(artifact.Snapshot.ReplicaClientIDs, target.id)
	}
	var runtime domain.CapsuleRuntime
	peer, err := e.broker.call(ctx, target.id, methodMaterialize, materializePayload{Composition: composition, Artifacts: artifacts, Authentication: authentication}, &runtime)
	if err != nil {
		return domain.CapsuleRuntime{}, err
	}
	runtime.ClientID = peer.id
	peer.addWorkload(1)
	return runtime, nil
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
	if err != nil {
		return err
	}
	exportProcess, err := e.broker.openStream(ctx, sourceID, methodExportSnapshot, snapshotPayload{Snapshot: artifact.Snapshot})
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

func (e *RemoteEngine) ensureSnapshotOn(ctx context.Context, artifact domain.Artifact, targetID string) error {
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
	process, err := e.broker.openStream(ctx, targetID, methodImportSnapshot, snapshotPayload{Snapshot: artifact.Snapshot})
	if err != nil {
		return errors.Join(replicaErr, err)
	}
	restoreErr := e.archive.RestoreSnapshot(ctx, artifact.Snapshot, process)
	closeErr := process.Close()
	execution, waitErr := process.Wait()
	if err := errors.Join(restoreErr, closeErr, waitErr, executionError("snapshot import", execution)); err != nil {
		return errors.Join(replicaErr, err)
	}
	_, err = e.broker.store.AddSnapshotReplica(artifact.ID, targetID)
	return err
}

func executionError(operation string, execution capsule.Execution) error {
	if execution.ExitCode == 0 {
		return nil
	}
	return fmt.Errorf("%s exited with %d: %s", operation, execution.ExitCode, execution.Output)
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
		if peer := b.peers[clientID]; peer != nil && peer.connectedNow() {
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
	id := fmt.Sprintf("str_%x", b.nextID.Add(1))
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
	_ capsule.EnabledEngine               = (*RemoteEngine)(nil)
	_ capsule.EnabledProber               = (*RemoteEngine)(nil)
	_ capsule.WorkspaceInspector          = (*RemoteEngine)(nil)
	_ capsule.WorkspaceRangeInspector     = (*RemoteEngine)(nil)
	_ capsule.WorkspaceAttachmentInjector = (*RemoteEngine)(nil)
	_ capsule.WorkspaceAcceptor           = (*RemoteEngine)(nil)
	_ capsule.SnapshotRemover             = (*RemoteEngine)(nil)
)
