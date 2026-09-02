package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"math/rand/v2"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"github.com/gorilla/websocket"
)

type Config struct {
	ServerURL    string
	InstanceID   string
	Name         string
	Tools        []string
	MaxWorkloads int
	ReconnectMin time.Duration
	ReconnectMax time.Duration
	Dialer       *websocket.Dialer
	Token        string
	Engine       capsule.Engine
}

type localStream struct {
	process capsule.EnabledProcess
	resize  func(uint16, uint16) error
	cancel  context.CancelFunc
}

type Worker struct {
	config Config
	logger *slog.Logger
	engine capsule.Engine
	dialer *websocket.Dialer

	mu            sync.Mutex
	clientID      string
	outbox        []wireMessage
	wake          chan struct{}
	space         chan struct{}
	cached        map[string]wireMessage
	cacheOrder    []string
	inFlight      map[string]context.CancelFunc
	streams       map[string]localStream
	liveWorkloads int
	connections   uint64
}

func New(config Config, logger *slog.Logger) *Worker {
	if logger == nil {
		logger = slog.Default()
	}
	if config.ReconnectMin <= 0 {
		config.ReconnectMin = 500 * time.Millisecond
	}
	if config.ReconnectMax < config.ReconnectMin {
		config.ReconnectMax = 15 * time.Second
	}
	if config.MaxWorkloads <= 0 {
		config.MaxWorkloads = 4
	}
	dialer := config.Dialer
	if dialer == nil {
		copyOfDefault := *websocket.DefaultDialer
		dialer = &copyOfDefault
	}
	return &Worker{
		config: config, logger: logger, engine: config.Engine, dialer: dialer,
		wake: make(chan struct{}, 1), space: make(chan struct{}, 1), cached: map[string]wireMessage{}, inFlight: map[string]context.CancelFunc{}, streams: map[string]localStream{},
	}
}

func (w *Worker) Run(ctx context.Context) error {
	if w.engine == nil {
		return errors.New("runner requires a capsule engine")
	}
	if strings.TrimSpace(w.config.InstanceID) == "" {
		return errors.New("runner instance ID is required")
	}
	delay := w.config.ReconnectMin
	for {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		before := w.connectionCount()
		err := w.runConnection(ctx)
		if ctx.Err() != nil {
			return ctx.Err()
		}
		if w.connectionCount() > before {
			delay = w.config.ReconnectMin
		}
		w.logger.Warn("runner connection lost; local workloads retained", "error", err, "retry_in", delay)
		jitter := time.Duration(rand.Int64N(max(int64(delay/3), 1)))
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(delay + jitter):
		}
		delay = min(delay*2, w.config.ReconnectMax)
	}
}

func (w *Worker) runConnection(ctx context.Context) error {
	endpoint, err := runnerWebSocketURL(w.config.ServerURL)
	if err != nil {
		return err
	}
	header := http.Header{}
	if w.config.Token != "" {
		header.Set("Authorization", "Bearer "+w.config.Token)
	}
	connection, response, err := w.dialer.DialContext(ctx, endpoint, header)
	if err != nil {
		if response != nil {
			return fmt.Errorf("connect runner WebSocket: %s: %w", response.Status, err)
		}
		return fmt.Errorf("connect runner WebSocket: %w", err)
	}
	defer connection.Close()
	connection.SetReadLimit(24 << 20)
	_ = connection.SetReadDeadline(time.Now().Add(pongWait))
	connection.SetPongHandler(func(string) error {
		return connection.SetReadDeadline(time.Now().Add(pongWait))
	})
	connection.SetPingHandler(func(payload string) error {
		_ = connection.SetReadDeadline(time.Now().Add(pongWait))
		return connection.WriteControl(websocket.PongMessage, []byte(payload), time.Now().Add(writeWait))
	})

	hello := wireMessage{
		Version: ProtocolVersion, Type: messageHello, InstanceID: w.config.InstanceID, Name: w.config.Name,
		Capabilities: domain.ClientCapabilities{
			OS: runtime.GOOS, Arch: runtime.GOARCH, Tools: append([]string(nil), w.config.Tools...), SnapshotModes: []string{"docker-image"},
			Engine: w.engine.Info(), MaxWorkloads: w.config.MaxWorkloads,
		},
	}
	if err := connection.WriteJSON(hello); err != nil {
		return err
	}
	var welcome wireMessage
	if err := connection.ReadJSON(&welcome); err != nil {
		return err
	}
	if welcome.Type != messageWelcome || welcome.Client == nil {
		return fmt.Errorf("runner handshake rejected: %s", welcome.Error)
	}
	w.mu.Lock()
	w.clientID = welcome.Client.ID
	w.connections++
	w.mu.Unlock()
	w.logger.Info("runner connected", "client_id", welcome.Client.ID, "server", w.config.ServerURL, "engine", w.engine.Info().Driver)

	// Keep the socket writer alive briefly after process shutdown starts so the
	// best-effort goodbye can actually leave the container.
	connectionCtx, cancel := context.WithCancel(context.Background())
	defer cancel()
	readerDone := make(chan error, 1)
	writerDone := make(chan error, 1)
	// Request lifetimes belong to the runner process, not to this physical
	// socket. A reconnect must not kill Docker/ACP work already in progress.
	go func() { readerDone <- w.readLoop(ctx, connection) }()
	go func() { writerDone <- w.writeLoop(connectionCtx, connection) }()

	select {
	case <-ctx.Done():
		w.enqueue(wireMessage{Version: ProtocolVersion, Type: messageGoodbye, Idle: w.idle()})
		select {
		case <-time.After(350 * time.Millisecond):
		case err := <-writerDone:
			return err
		}
		return ctx.Err()
	case err := <-readerDone:
		return err
	case err := <-writerDone:
		return err
	}
}

func runnerWebSocketURL(serverURL string) (string, error) {
	parsed, err := url.Parse(strings.TrimSpace(serverURL))
	if err != nil {
		return "", err
	}
	switch parsed.Scheme {
	case "http":
		parsed.Scheme = "ws"
	case "https":
		parsed.Scheme = "wss"
	case "ws", "wss":
	default:
		return "", fmt.Errorf("unsupported server URL scheme %q", parsed.Scheme)
	}
	parsed.Path = strings.TrimRight(parsed.Path, "/") + "/api/runner/ws"
	parsed.RawQuery = ""
	parsed.Fragment = ""
	return parsed.String(), nil
}

func (w *Worker) readLoop(ctx context.Context, connection *websocket.Conn) error {
	for {
		var message wireMessage
		if err := connection.ReadJSON(&message); err != nil {
			return err
		}
		_ = connection.SetReadDeadline(time.Now().Add(pongWait))
		switch message.Type {
		case messageRequest:
			w.dispatch(ctx, message)
		case messageCancel:
			w.mu.Lock()
			cancel := w.inFlight[message.ID]
			w.mu.Unlock()
			if cancel != nil {
				cancel()
			}
		case messageStreamInput:
			if stream, ok := w.stream(message.ID); ok {
				_, _ = stream.process.Write(message.Data)
			}
		case messageStreamResize:
			if stream, ok := w.stream(message.ID); ok && stream.resize != nil {
				_ = stream.resize(message.Rows, message.Cols)
			}
		case messageStreamClose:
			if stream, ok := w.stream(message.ID); ok {
				_ = stream.process.Close()
			}
		}
	}
}

func (w *Worker) writeLoop(ctx context.Context, connection *websocket.Conn) error {
	ticker := time.NewTicker(pingEvery)
	defer ticker.Stop()
	for {
		if message, ok := w.nextOutbound(); ok {
			_ = connection.SetWriteDeadline(time.Now().Add(writeWait))
			if err := connection.WriteJSON(message); err != nil {
				return err
			}
			w.ackOutbound(message)
			continue
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-w.wake:
		case <-ticker.C:
			if err := connection.WriteControl(websocket.PingMessage, []byte("spin-runner"), time.Now().Add(writeWait)); err != nil {
				return err
			}
		}
	}
}

func (w *Worker) dispatch(root context.Context, request wireMessage) {
	w.mu.Lock()
	if cached, ok := w.cached[request.ID]; ok {
		w.mu.Unlock()
		w.enqueue(cached)
		return
	}
	if _, ok := w.inFlight[request.ID]; ok {
		w.mu.Unlock()
		return
	}
	ctx, cancel := context.WithCancel(root)
	w.inFlight[request.ID] = cancel
	w.mu.Unlock()

	go func() {
		response, keepsContext := w.executeRequest(ctx, request)
		if !keepsContext {
			cancel()
		}
		w.mu.Lock()
		delete(w.inFlight, request.ID)
		w.cached[request.ID] = response
		w.cacheOrder = append(w.cacheOrder, request.ID)
		for len(w.cacheOrder) > 256 {
			delete(w.cached, w.cacheOrder[0])
			w.cacheOrder = w.cacheOrder[1:]
		}
		w.mu.Unlock()
		w.enqueue(response)
	}()
}

func (w *Worker) executeRequest(ctx context.Context, request wireMessage) (wireMessage, bool) {
	response := wireMessage{Version: ProtocolVersion, Type: messageResponse, ID: request.ID}
	result, keepsContext, err := w.invoke(ctx, request)
	if err != nil {
		response.Error = err.Error()
		return response, false
	}
	if result != nil {
		response.Payload, err = json.Marshal(result)
		if err != nil {
			response.Error = err.Error()
			return response, false
		}
	}
	return response, keepsContext
}

func (w *Worker) invoke(ctx context.Context, request wireMessage) (any, bool, error) {
	switch request.Method {
	case methodStartRecording:
		var payload startRecordingPayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		runtime, err := w.engine.StartRecording(ctx, payload.Recording, payload.Parents)
		if err == nil {
			w.adjustLiveWorkloads(1)
		}
		return runtime, false, err
	case methodExecute:
		var payload executePayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		result, err := w.engine.Execute(ctx, payload.Recording, payload.Input)
		return result, false, err
	case methodSeal:
		var payload recordingPayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		result, err := w.engine.Seal(ctx, payload.Recording)
		if err == nil {
			w.adjustLiveWorkloads(-1)
		}
		return result, false, err
	case methodCancelRecording:
		var payload recordingPayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		err := w.engine.Cancel(ctx, payload.Recording)
		if err == nil {
			w.adjustLiveWorkloads(-1)
		}
		return nil, false, err
	case methodMaterialize:
		var payload materializePayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		var result domain.CapsuleRuntime
		var err error
		if payload.Authentication != nil {
			materializer, ok := w.engine.(capsule.SecretMaterializer)
			if !ok {
				return nil, false, errors.New("runner engine cannot receive transient Git authentication")
			}
			result, err = materializer.MaterializeWithGitAuthentication(ctx, payload.Composition, payload.Artifacts, payload.Authentication)
		} else {
			result, err = w.engine.Materialize(ctx, payload.Composition, payload.Artifacts)
		}
		if err == nil {
			w.adjustLiveWorkloads(1)
		}
		return result, false, err
	case methodStop:
		var payload runtimePayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		err := w.engine.Stop(ctx, payload.Runtime)
		if err == nil {
			w.adjustLiveWorkloads(-1)
		}
		return nil, false, err
	case methodProbeEnabled:
		var payload enabledPayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		prober, ok := w.engine.(capsule.EnabledProber)
		if !ok {
			return nil, false, errors.New("runner engine cannot probe enabled entrypoints")
		}
		result, err := prober.ProbeEnabled(ctx, payload.Runtime, payload.Enablement, payload.Request)
		return result, false, err
	case methodStartEnabled:
		var payload enabledPayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		engine, ok := w.engine.(capsule.EnabledEngine)
		if !ok {
			return nil, false, errors.New("runner engine cannot stream enabled entrypoints")
		}
		process, err := engine.StartEnabled(ctx, payload.Runtime, payload.Enablement)
		if err != nil {
			return nil, false, err
		}
		w.bindStream(request.ID, localStream{process: process})
		go w.pumpStream(request.ID, process)
		return streamResponse{StreamID: request.ID}, true, nil
	case methodStartInteractive:
		var payload interactivePayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		engine, ok := w.engine.(capsule.InteractiveEngine)
		if !ok {
			return nil, false, errors.New("runner engine has no interactive terminal")
		}
		process, err := engine.StartInteractive(ctx, payload.Recording, payload.Input, payload.Rows, payload.Cols)
		if err != nil {
			return nil, false, err
		}
		w.bindStream(request.ID, localStream{process: process, resize: process.Resize})
		go w.pumpStream(request.ID, process)
		return streamResponse{StreamID: request.ID}, true, nil
	case methodInspectWorkspace:
		var payload runtimePayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		inspector, ok := w.engine.(capsule.WorkspaceInspector)
		if !ok {
			return nil, false, errors.New("runner engine cannot inspect workspaces")
		}
		result, err := inspector.InspectWorkspace(ctx, payload.Runtime)
		return result, false, err
	case methodInspectRange:
		var payload inspectRangePayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		inspector, ok := w.engine.(capsule.WorkspaceRangeInspector)
		if !ok {
			return nil, false, errors.New("runner engine cannot compare workspaces")
		}
		result, err := inspector.InspectWorkspaceRange(ctx, payload.Runtime, payload.Comparison)
		return result, false, err
	case methodInjectAttachments:
		var payload injectAttachmentsPayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		injector, ok := w.engine.(capsule.WorkspaceAttachmentInjector)
		if !ok {
			return nil, false, errors.New("runner engine cannot inject Job attachments")
		}
		return nil, false, w.injectAttachments(ctx, injector, payload)
	case methodAcceptWorkspace:
		var payload acceptWorkspacePayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		acceptor, ok := w.engine.(capsule.WorkspaceAcceptor)
		if !ok {
			return nil, false, errors.New("runner engine cannot accept workspaces")
		}
		result, err := acceptor.AcceptWorkspace(ctx, payload.Runtime, payload.Acceptance)
		return result, false, err
	case methodRemoveSnapshot:
		var payload snapshotPayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		remover, ok := w.engine.(capsule.SnapshotRemover)
		if !ok {
			return nil, false, errors.New("runner engine cannot remove snapshots")
		}
		return nil, false, remover.RemoveSnapshot(ctx, payload.Snapshot)
	case methodExportSnapshot:
		var payload snapshotPayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		exporter, ok := w.engine.(capsule.SnapshotExporter)
		if !ok {
			return nil, false, errors.New("runner engine cannot export snapshots")
		}
		process := newSnapshotExportProcess(ctx, exporter, payload.Snapshot)
		w.bindStream(request.ID, localStream{process: process})
		go w.pumpStream(request.ID, process)
		return streamResponse{StreamID: request.ID}, true, nil
	case methodImportSnapshot:
		var payload snapshotPayload
		if err := json.Unmarshal(request.Payload, &payload); err != nil {
			return nil, false, err
		}
		importer, ok := w.engine.(capsule.SnapshotImporter)
		if !ok {
			return nil, false, errors.New("runner engine cannot import snapshots")
		}
		process := newSnapshotImportProcess(ctx, importer, payload.Snapshot)
		w.bindStream(request.ID, localStream{process: process})
		go w.pumpStream(request.ID, process)
		return streamResponse{StreamID: request.ID}, true, nil
	default:
		return nil, false, fmt.Errorf("unsupported runner method %q", request.Method)
	}
}

func (w *Worker) bindStream(id string, stream localStream) {
	w.mu.Lock()
	stream.cancel = w.inFlight[id]
	w.streams[id] = stream
	w.mu.Unlock()
}

func (w *Worker) pumpStream(id string, process capsule.EnabledProcess) {
	buffer := make([]byte, 32<<10)
	for {
		count, err := process.Read(buffer)
		if count > 0 {
			w.enqueue(wireMessage{Version: ProtocolVersion, Type: messageStreamData, ID: id, Data: append([]byte(nil), buffer[:count]...)})
		}
		if err != nil {
			if !errors.Is(err, io.EOF) {
				w.logger.Debug("read runner process", "stream_id", id, "error", err)
			}
			break
		}
	}
	execution, err := process.Wait()
	w.mu.Lock()
	stream := w.streams[id]
	delete(w.streams, id)
	w.mu.Unlock()
	if stream.cancel != nil {
		stream.cancel()
	}
	message := wireMessage{Version: ProtocolVersion, Type: messageStreamExit, ID: id, Execution: &execution}
	if err != nil {
		message.Error = err.Error()
	}
	w.enqueue(message)
}

func (w *Worker) injectAttachments(ctx context.Context, injector capsule.WorkspaceAttachmentInjector, payload injectAttachmentsPayload) error {
	directory, err := os.MkdirTemp("", "spin-runner-attachments-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(directory)
	attachments := make([]capsule.WorkspaceAttachment, 0, len(payload.Attachments))
	for index, attachment := range payload.Attachments {
		path := filepath.Join(directory, fmt.Sprintf("%d", index))
		if err := os.WriteFile(path, attachment.Data, 0o600); err != nil {
			return err
		}
		attachments = append(attachments, capsule.WorkspaceAttachment{SourcePath: path, TargetPath: attachment.TargetPath})
	}
	return injector.InjectWorkspaceAttachments(ctx, payload.Runtime, attachments)
}

func (w *Worker) stream(id string) (localStream, bool) {
	w.mu.Lock()
	defer w.mu.Unlock()
	stream, ok := w.streams[id]
	return stream, ok
}

func (w *Worker) adjustLiveWorkloads(delta int) {
	w.mu.Lock()
	w.liveWorkloads += delta
	if w.liveWorkloads < 0 {
		w.liveWorkloads = 0
	}
	w.mu.Unlock()
}

func (w *Worker) idle() bool {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.liveWorkloads == 0 && len(w.streams) == 0 && len(w.inFlight) == 0
}

func (w *Worker) enqueue(message wireMessage) {
	for {
		w.mu.Lock()
		if len(w.outbox) < 2048 {
			w.outbox = append(w.outbox, message)
			w.mu.Unlock()
			select {
			case w.wake <- struct{}{}:
			default:
			}
			return
		}
		w.mu.Unlock()
		<-w.space
	}
}

func (w *Worker) nextOutbound() (wireMessage, bool) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if len(w.outbox) == 0 {
		return wireMessage{}, false
	}
	return w.outbox[0], true
}

func (w *Worker) ackOutbound(message wireMessage) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if len(w.outbox) == 0 {
		return
	}
	first := w.outbox[0]
	if first.Type == message.Type && first.ID == message.ID && first.Method == message.Method {
		w.outbox = w.outbox[1:]
		select {
		case w.space <- struct{}{}:
		default:
		}
	}
}

func (w *Worker) connectionCount() uint64 {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.connections
}

type snapshotTransferProcess struct {
	reader *io.PipeReader
	writer *io.PipeWriter
	cancel context.CancelFunc
	done   chan struct{}

	mu        sync.Mutex
	execution capsule.Execution
	err       error
}

func newSnapshotExportProcess(ctx context.Context, exporter capsule.SnapshotExporter, snapshot domain.CapsuleSnapshot) *snapshotTransferProcess {
	processCtx, cancel := context.WithCancel(ctx)
	reader, writer := io.Pipe()
	process := &snapshotTransferProcess{reader: reader, cancel: cancel, done: make(chan struct{})}
	go func() {
		err := exporter.ExportSnapshot(processCtx, snapshot, writer)
		_ = writer.CloseWithError(err)
		process.finish(err)
	}()
	return process
}

func newSnapshotImportProcess(ctx context.Context, importer capsule.SnapshotImporter, snapshot domain.CapsuleSnapshot) *snapshotTransferProcess {
	processCtx, cancel := context.WithCancel(ctx)
	reader, writer := io.Pipe()
	process := &snapshotTransferProcess{writer: writer, cancel: cancel, done: make(chan struct{})}
	go func() {
		err := importer.ImportSnapshot(processCtx, snapshot, reader)
		_ = reader.CloseWithError(err)
		process.finish(err)
	}()
	return process
}

func (p *snapshotTransferProcess) finish(err error) {
	p.mu.Lock()
	p.err = err
	if err != nil {
		p.execution.ExitCode = 1
	}
	p.mu.Unlock()
	close(p.done)
}

func (p *snapshotTransferProcess) Read(target []byte) (int, error) {
	if p.reader != nil {
		return p.reader.Read(target)
	}
	<-p.done
	return 0, io.EOF
}

func (p *snapshotTransferProcess) Write(data []byte) (int, error) {
	if p.writer == nil {
		return 0, io.ErrClosedPipe
	}
	return p.writer.Write(data)
}

func (p *snapshotTransferProcess) Close() error {
	if p.writer != nil {
		return p.writer.Close()
	}
	p.cancel()
	if p.reader != nil {
		return p.reader.Close()
	}
	return nil
}

func (p *snapshotTransferProcess) Wait() (capsule.Execution, error) {
	<-p.done
	p.cancel()
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.execution, p.err
}
