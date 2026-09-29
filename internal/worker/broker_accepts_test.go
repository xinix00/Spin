package worker

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"

	"github.com/gorilla/websocket"
)

// fakeRunner connects to a broker and answers whether it accepts a capsule.
type fakeRunner struct {
	client  domain.Client
	accepts atomic.Bool
	asked   atomic.Int32
}

func connectFakeRunner(t *testing.T, address, instance string, capsules *capsule.LiveCapsules) *fakeRunner {
	t.Helper()
	connection, _, err := websocket.DefaultDialer.Dial(address, nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { connection.Close() })
	engine := domain.CapsuleEngineInfo{Driver: "docker", Available: true}
	hello := wireMessage{Version: ProtocolVersion, Type: messageHello, InstanceID: instance, Name: instance, Process: instance,
		Capabilities: domain.ClientCapabilities{Engine: engine, MaxWorkloads: 1}, Capsules: capsules}
	if err := connection.WriteJSON(hello); err != nil {
		t.Fatal(err)
	}
	var welcome wireMessage
	if err := connection.ReadJSON(&welcome); err != nil || welcome.Client == nil {
		t.Fatalf("welcome = %+v, %v", welcome, err)
	}
	runner := &fakeRunner{client: *welcome.Client}
	go func() {
		for {
			var message wireMessage
			if err := connection.ReadJSON(&message); err != nil {
				return
			}
			if message.Type != messageRequest || message.Method != methodAccepts {
				continue
			}
			runner.asked.Add(1)
			payload, _ := json.Marshal(acceptsReply{Accepts: runner.accepts.Load()})
			_ = connection.WriteJSON(wireMessage{Version: ProtocolVersion, Type: messageResponse, ID: message.ID, Payload: payload})
		}
	}()
	return runner
}

func newAcceptsBroker(t *testing.T) (*Broker, *store.Store, string) {
	t.Helper()
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	broker := NewBroker(st, slog.New(slog.NewTextHandler(io.Discard, nil)))
	server := httptest.NewServer(http.HandlerFunc(broker.Handler))
	t.Cleanup(server.Close)
	return broker, st, "ws" + strings.TrimPrefix(server.URL, "http")
}

// Whether a capsule fits is the runner's to say. The server asks, skips the
// one that says no, and takes the one that says yes; it counts nothing.
func TestTheRunnerDecidesWhetherACapsuleFits(t *testing.T) {
	broker, _, address := newAcceptsBroker(t)
	full := connectFakeRunner(t, address, "full", nil)
	room := connectFakeRunner(t, address, "room", nil)
	room.accepts.Store(true)

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	for range 3 {
		peer, err := broker.chooseAccepting(ctx, nil)
		if err != nil {
			t.Fatal(err)
		}
		if peer.id != room.client.ID {
			t.Fatalf("chose %s; only %s said yes", peer.name, room.client.Name)
		}
	}
	// A runner that said no is left alone for a moment, not asked each time.
	if asked := full.asked.Load(); asked != 1 {
		t.Fatalf("the full runner was asked %d times", asked)
	}
}

// When no runner accepts, the choice waits instead of failing at once, and
// takes a runner the moment a capsule stops there.
func TestAChoiceWaitsUntilARunnerHasRoom(t *testing.T) {
	broker, _, address := newAcceptsBroker(t)
	runner := connectFakeRunner(t, address, "busy", nil)
	chosen := make(chan error, 1)
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		_, err := broker.chooseAccepting(ctx, nil)
		chosen <- err
	}()
	select {
	case err := <-chosen:
		t.Fatalf("a choice with every runner full returned %v", err)
	case <-time.After(500 * time.Millisecond):
	}
	// A capsule stops on the runner: it has room and says so.
	runner.accepts.Store(true)
	peer := broker.peer(runner.client)
	peer.freed()
	broker.notifyAvailable()
	select {
	case err := <-chosen:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("the choice did not take the runner that had room again")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
	defer cancel()
	runner.accepts.Store(false)
	peer.freed()
	if _, err := broker.chooseAccepting(ctx, nil); !errors.Is(err, errNoRunner) || !strings.Contains(err.Error(), "accepts") {
		t.Fatalf("no runner with room: %v", err)
	}
}

// A runner says which capsules really run there. What the state still had
// running on it and is not among them stops, and its logins are free again;
// what runs stays, and so does a capsule that has no runtime yet.
func TestARunnerReportsWhatRunsAndTheRestStops(t *testing.T) {
	broker, st, address := newAcceptsBroker(t)
	first := connectFakeRunner(t, address, "laptop", nil)
	for _, id := range []string{"cmp_live", "cmp_gone"} {
		if err := st.PutCompositionForTest(domain.Composition{ID: id, Operator: "derek", Runtime: &domain.CapsuleRuntime{ClientID: first.client.ID, Status: "ready", StopPending: id == "cmp_gone"}}); err != nil {
			t.Fatal(err)
		}
	}
	if err := st.PutCompositionForTest(domain.Composition{ID: "cmp_building", Operator: "derek"}); err != nil {
		t.Fatal(err)
	}
	_ = broker
	connectFakeRunner(t, address, "laptop", &capsule.LiveCapsules{Compositions: []string{"cmp_live"}, Recordings: []string{}})
	for deadline := time.Now().Add(5 * time.Second); ; time.Sleep(10 * time.Millisecond) {
		gone, _ := st.Composition("cmp_gone")
		if gone.Runtime != nil && gone.Runtime.Status == "stopped" {
			if gone.Runtime.StopPending {
				t.Fatal("a capsule that is gone still waits for its stop")
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("a capsule the runner does not run stayed %+v", gone.Runtime)
		}
	}
	if live, _ := st.Composition("cmp_live"); live.Runtime.Status != "ready" {
		t.Fatalf("a capsule that runs was stopped: %+v", live.Runtime)
	}
	if building, _ := st.Composition("cmp_building"); building.Runtime != nil {
		t.Fatalf("a capsule being built was touched: %+v", building.Runtime)
	}
}

// listingEngine is an engine that says which capsules run.
type listingEngine struct {
	capsule.Journal
	live atomic.Pointer[capsule.LiveCapsules]
}

func (e *listingEngine) LiveCapsules(context.Context) (capsule.LiveCapsules, error) {
	return *e.live.Load(), nil
}

// The runner measures itself: what runs by Docker's count and what is being
// started, against its own maximum. A capsule that already runs here is let
// through at any count, and two starts at once do not both take the last
// place.
func TestTheRunnerAdmitsByWhatRunsThere(t *testing.T) {
	engine := &listingEngine{}
	engine.live.Store(&capsule.LiveCapsules{Compositions: []string{"cmp_a"}, Recordings: []string{"rec_b"}})
	w := New(Config{Engine: engine, MaxWorkloads: 2}, slog.New(slog.NewTextHandler(io.Discard, nil)))
	ctx := context.Background()
	if w.hasRoom(ctx, "", "", 1) {
		t.Fatal("a full runner accepts one more")
	}
	if release, ok := w.admit(ctx, "cmp_a", ""); !ok {
		t.Fatal("a capsule that already runs here was refused at its own start")
	} else {
		release()
	}
	if _, ok := w.admit(ctx, "cmp_new", ""); ok {
		t.Fatal("a full runner admitted a new capsule")
	}
	engine.live.Store(&capsule.LiveCapsules{Compositions: []string{"cmp_a"}})
	if !w.hasRoom(ctx, "", "", 1) {
		t.Fatal("a runner with a free place does not accept")
	}
	first, ok := w.admit(ctx, "cmp_one", "")
	if !ok {
		t.Fatal("the last place was not given")
	}
	if _, ok := w.admit(ctx, "cmp_two", ""); ok {
		t.Fatal("the last place went to two starts at once")
	}
	first()
	if !w.hasRoom(ctx, "", "", 1) {
		t.Fatal("a start that ended still holds its place")
	}
}
