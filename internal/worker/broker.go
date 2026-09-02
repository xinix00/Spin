package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/store"
	"github.com/gorilla/websocket"
)

const (
	pingEvery = 10 * time.Second
	pongWait  = 35 * time.Second
	writeWait = 10 * time.Second
)

var errNoRunner = errors.New("no compatible runner connected")

type pendingCall struct {
	request wireMessage
	result  chan wireMessage
}

type runnerPeer struct {
	id           string
	instanceID   string
	name         string
	capabilities domain.ClientCapabilities

	mu         sync.Mutex
	connection *websocket.Conn
	generation uint64
	connected  bool
	draining   bool
	workloads  int
	outbox     []wireMessage
	wake       chan struct{}
	pending    map[string]pendingCall
	streams    map[string]*remoteProcess
}

func newRunnerPeer(client domain.Client) *runnerPeer {
	return &runnerPeer{
		id: client.ID, instanceID: client.InstanceID, name: client.Name, capabilities: client.Capabilities,
		draining: client.Draining, wake: make(chan struct{}, 1), pending: map[string]pendingCall{}, streams: map[string]*remoteProcess{},
	}
}

func (p *runnerPeer) attach(connection *websocket.Conn, client domain.Client) uint64 {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.connection != nil {
		_ = p.connection.Close()
	}
	p.connection = connection
	p.generation++
	p.connected = true
	p.draining = client.Draining
	p.instanceID = client.InstanceID
	p.name = client.Name
	p.capabilities = client.Capabilities
	// A request may have reached the old socket while its response did not.
	// Replaying the same ID is safe: the runner deduplicates in-flight and
	// completed calls.
	for _, pending := range p.pending {
		if !containsMessageID(p.outbox, pending.request.ID) {
			p.outbox = append(p.outbox, pending.request)
		}
	}
	p.signalLocked()
	return p.generation
}

func (p *runnerPeer) detach(generation uint64) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.generation != generation {
		return false
	}
	p.connection = nil
	p.connected = false
	for _, pending := range p.pending {
		if !containsMessageID(p.outbox, pending.request.ID) {
			p.outbox = append(p.outbox, pending.request)
		}
	}
	return true
}

func (p *runnerPeer) enqueue(message wireMessage) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if len(p.outbox) >= 8192 {
		return errors.New("runner control outbox is full")
	}
	p.outbox = append(p.outbox, message)
	p.signalLocked()
	return nil
}

func (p *runnerPeer) signalLocked() {
	select {
	case p.wake <- struct{}{}:
	default:
	}
}

func (p *runnerPeer) next(generation uint64) (wireMessage, bool, bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.generation != generation || !p.connected {
		return wireMessage{}, false, false
	}
	if len(p.outbox) == 0 {
		return wireMessage{}, false, true
	}
	return p.outbox[0], true, true
}

func (p *runnerPeer) acknowledge(message wireMessage, generation uint64) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.generation != generation || len(p.outbox) == 0 {
		return
	}
	first := p.outbox[0]
	if first.Type == message.Type && first.ID == message.ID && first.Method == message.Method {
		p.outbox = p.outbox[1:]
	}
}

func (p *runnerPeer) available() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	if !p.connected || p.draining || !p.capabilities.Engine.Available {
		return false
	}
	return p.capabilities.MaxWorkloads <= 0 || p.workloads < p.capabilities.MaxWorkloads
}

func (p *runnerPeer) connectedForAffinity() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.connected
}

func (p *runnerPeer) engineInfo() (domain.CapsuleEngineInfo, bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.capabilities.Engine, p.connected && !p.draining
}

func (p *runnerPeer) addWorkload(delta int) {
	p.mu.Lock()
	p.workloads += delta
	if p.workloads < 0 {
		p.workloads = 0
	}
	p.mu.Unlock()
}

func containsMessageID(messages []wireMessage, id string) bool {
	for _, message := range messages {
		if id != "" && message.ID == id {
			return true
		}
	}
	return false
}

// Broker owns the durable logical runner connections. A physical WebSocket
// may be replaced many times while the peer, its pending calls and streams stay
// intact. New workloads are distributed round-robin; existing runtimes always
// address their pinned ClientID.
type Broker struct {
	store  *store.Store
	logger *slog.Logger

	mu        sync.Mutex
	peers     map[string]*runnerPeer
	order     []string
	cursor    int
	available chan struct{}
	nextID    atomic.Uint64
}

func NewBroker(st *store.Store, logger *slog.Logger) *Broker {
	if logger == nil {
		logger = slog.Default()
	}
	return &Broker{store: st, logger: logger, peers: map[string]*runnerPeer{}, available: make(chan struct{}, 1)}
}

func (b *Broker) Handler(w http.ResponseWriter, r *http.Request) {
	upgrader := websocket.Upgrader{
		ReadBufferSize: 16 << 10, WriteBufferSize: 16 << 10,
		CheckOrigin: func(r *http.Request) bool {
			origin := strings.TrimSpace(r.Header.Get("Origin"))
			if origin == "" {
				return true
			}
			parsed, err := url.Parse(origin)
			return err == nil && strings.EqualFold(parsed.Host, r.Host)
		},
	}
	connection, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		b.logger.Debug("upgrade runner WebSocket", "error", err)
		return
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

	var hello wireMessage
	if err := connection.ReadJSON(&hello); err != nil || hello.Type != messageHello || hello.Version != ProtocolVersion || strings.TrimSpace(hello.InstanceID) == "" {
		_ = connection.WriteJSON(wireMessage{Version: ProtocolVersion, Type: messageResponse, Error: "first runner message must be a supported hello"})
		return
	}
	client, err := b.store.RegisterClient(domain.RegisterClientRequest{InstanceID: hello.InstanceID, Name: hello.Name, Capabilities: hello.Capabilities})
	if err != nil {
		_ = connection.WriteJSON(wireMessage{Version: ProtocolVersion, Type: messageResponse, Error: err.Error()})
		return
	}
	peer := b.peer(client)
	if err := connection.WriteJSON(wireMessage{Version: ProtocolVersion, Type: messageWelcome, Client: &client}); err != nil {
		return
	}
	generation := peer.attach(connection, client)
	b.notifyAvailable()
	b.logger.Info("runner connected", "client_id", client.ID, "instance_id", client.InstanceID, "name", client.Name)

	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()
	writerDone := make(chan error, 1)
	go func() { writerDone <- b.writeLoop(ctx, peer, connection, generation) }()

	readErr := b.readLoop(ctx, peer, connection)
	cancel()
	select {
	case <-writerDone:
	case <-time.After(time.Second):
	}
	if peer.detach(generation) {
		_, _ = b.store.SetClientStatus(client.ID, "offline")
		b.logger.Info("runner disconnected; affinity retained", "client_id", client.ID, "error", readErr)
	}
}

func (b *Broker) peer(client domain.Client) *runnerPeer {
	b.mu.Lock()
	defer b.mu.Unlock()
	peer := b.peers[client.ID]
	if peer == nil {
		peer = newRunnerPeer(client)
		b.peers[client.ID] = peer
		b.order = append(b.order, client.ID)
	}
	return peer
}

func (b *Broker) readLoop(ctx context.Context, peer *runnerPeer, connection *websocket.Conn) error {
	for {
		var message wireMessage
		if err := connection.ReadJSON(&message); err != nil {
			return err
		}
		_ = connection.SetReadDeadline(time.Now().Add(pongWait))
		switch message.Type {
		case messageResponse:
			peer.mu.Lock()
			pending, ok := peer.pending[message.ID]
			if ok {
				delete(peer.pending, message.ID)
			}
			peer.mu.Unlock()
			if ok {
				select {
				case pending.result <- message:
				case <-ctx.Done():
				}
			}
		case messageStreamData, messageStreamExit:
			peer.mu.Lock()
			stream := peer.streams[message.ID]
			if message.Type == messageStreamExit {
				delete(peer.streams, message.ID)
			}
			peer.mu.Unlock()
			if stream != nil {
				if message.Type == messageStreamData {
					stream.deliver(message.Data)
				} else {
					stream.finish(message.Execution, message.Error)
				}
			}
		case messageGoodbye:
			peer.mu.Lock()
			peer.draining = true
			peer.mu.Unlock()
			status := "draining"
			if message.Idle {
				status = "offline"
			}
			_, _ = b.store.SetClientStatus(peer.id, status)
			return errors.New("runner sent graceful goodbye")
		}
	}
}

func (b *Broker) writeLoop(ctx context.Context, peer *runnerPeer, connection *websocket.Conn, generation uint64) error {
	ticker := time.NewTicker(pingEvery)
	defer ticker.Stop()
	for {
		message, hasMessage, alive := peer.next(generation)
		if !alive {
			return errors.New("runner connection was replaced")
		}
		if hasMessage {
			_ = connection.SetWriteDeadline(time.Now().Add(writeWait))
			if err := connection.WriteJSON(message); err != nil {
				return err
			}
			peer.acknowledge(message, generation)
			continue
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-peer.wake:
			continue
		case <-ticker.C:
			if err := connection.WriteControl(websocket.PingMessage, []byte("spin-server"), time.Now().Add(writeWait)); err != nil {
				return err
			}
		}
	}
}

func (b *Broker) notifyAvailable() {
	select {
	case b.available <- struct{}{}:
	default:
	}
}

// SetDraining keeps existing affinity usable but removes the runner from new
// round-robin placement until an administrator resumes it.
func (b *Broker) SetDraining(clientID string, draining bool) (domain.Client, error) {
	client, err := b.store.Client(clientID)
	if err != nil {
		return domain.Client{}, err
	}
	peer := b.peer(client)
	peer.mu.Lock()
	peer.draining = draining
	connected := peer.connected
	peer.mu.Unlock()
	updated, err := b.store.SetClientDraining(client.ID, draining, connected)
	if err != nil {
		return domain.Client{}, err
	}
	if !draining && connected {
		b.notifyAvailable()
	}
	return updated, nil
}

func (b *Broker) choose(ctx context.Context, affinity string) (*runnerPeer, error) {
	for {
		b.mu.Lock()
		if affinity != "" {
			peer := b.peers[affinity]
			b.mu.Unlock()
			if peer != nil && peer.connectedForAffinity() {
				return peer, nil
			}
		} else {
			count := len(b.order)
			for offset := 0; offset < count; offset++ {
				index := (b.cursor + offset) % count
				peer := b.peers[b.order[index]]
				if peer != nil && peer.available() {
					b.cursor = (index + 1) % count
					b.mu.Unlock()
					return peer, nil
				}
			}
			b.mu.Unlock()
		}
		select {
		case <-ctx.Done():
			return nil, fmt.Errorf("%w: %v", errNoRunner, ctx.Err())
		case <-b.available:
		case <-time.After(5 * time.Second):
		}
	}
}

func (b *Broker) call(ctx context.Context, affinity, method string, request, response any) (*runnerPeer, error) {
	peer, err := b.choose(ctx, affinity)
	if err != nil {
		return nil, err
	}
	payload, err := json.Marshal(request)
	if err != nil {
		return nil, err
	}
	id := fmt.Sprintf("rpc_%x", b.nextID.Add(1))
	message := wireMessage{Version: ProtocolVersion, Type: messageRequest, ID: id, Method: method, Payload: payload}
	result := make(chan wireMessage, 1)
	peer.mu.Lock()
	peer.pending[id] = pendingCall{request: message, result: result}
	peer.mu.Unlock()
	if err := peer.enqueue(message); err != nil {
		peer.mu.Lock()
		delete(peer.pending, id)
		peer.mu.Unlock()
		return nil, err
	}
	select {
	case <-ctx.Done():
		peer.mu.Lock()
		delete(peer.pending, id)
		peer.mu.Unlock()
		_ = peer.enqueue(wireMessage{Version: ProtocolVersion, Type: messageCancel, ID: id})
		return nil, ctx.Err()
	case message := <-result:
		if message.Error != "" {
			return peer, errors.New(message.Error)
		}
		if response != nil && len(message.Payload) != 0 {
			if err := json.Unmarshal(message.Payload, response); err != nil {
				return peer, err
			}
		}
		return peer, nil
	}
}

func (b *Broker) info() domain.CapsuleEngineInfo {
	b.mu.Lock()
	defer b.mu.Unlock()
	for _, id := range b.order {
		peer := b.peers[id]
		if peer == nil {
			continue
		}
		if info, connected := peer.engineInfo(); connected && info.Available {
			info.Driver = "runner/" + info.Driver
			info.Detail = "remote runner fleet · " + info.Detail
			return info
		}
	}
	return domain.CapsuleEngineInfo{Driver: "runner", Available: false, Detail: "waiting for a connected Docker runner"}
}
