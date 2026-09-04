package worker

import (
	"errors"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/store"

	"github.com/gorilla/websocket"
)

func TestDisconnectAllMakesRunnersRegisterAgain(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	broker := NewBroker(st, slog.New(slog.NewTextHandler(io.Discard, nil)))
	server := httptest.NewServer(http.HandlerFunc(broker.Handler))
	defer server.Close()
	address := "ws" + strings.TrimPrefix(server.URL, "http")
	capabilities := domain.ClientCapabilities{Engine: domain.CapsuleEngineInfo{Driver: "docker", Available: true}}
	connect := func() (*websocket.Conn, domain.Client) {
		t.Helper()
		connection, _, err := websocket.DefaultDialer.Dial(address, nil)
		if err != nil {
			t.Fatal(err)
		}
		hello := wireMessage{Version: ProtocolVersion, Type: messageHello, InstanceID: "laptop-1", Name: "Laptop", Capabilities: capabilities}
		if err := connection.WriteJSON(hello); err != nil {
			t.Fatal(err)
		}
		var welcome wireMessage
		if err := connection.ReadJSON(&welcome); err != nil || welcome.Type != messageWelcome || welcome.Client == nil {
			t.Fatalf("welcome = %+v, %v", welcome, err)
		}
		return connection, *welcome.Client
	}

	first, client := connect()
	defer first.Close()
	if client.Status != "online" {
		t.Fatalf("registered client = %+v", client)
	}
	if closed := broker.DisconnectAll("restored"); closed != 1 {
		t.Fatalf("closed %d runner sockets, want 1", closed)
	}
	_ = first.SetReadDeadline(time.Now().Add(5 * time.Second))
	var stray wireMessage
	err = first.ReadJSON(&stray)
	var closeErr *websocket.CloseError
	if !errors.As(err, &closeErr) || closeErr.Code != websocket.CloseServiceRestart {
		t.Fatalf("runner observed %v (%+v), want a service-restart close", err, stray)
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		current, err := st.Client(client.ID)
		if err != nil {
			t.Fatal(err)
		}
		if current.Status == "offline" {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("client status after disconnect = %q", current.Status)
		}
		time.Sleep(5 * time.Millisecond)
	}

	second, again := connect()
	defer second.Close()
	if again.ID != client.ID || again.Status != "online" {
		t.Fatalf("re-registered client = %+v, want %s online", again, client.ID)
	}
	if closed := broker.DisconnectAll("again"); closed != 1 {
		t.Fatalf("closed %d runner sockets after reconnect, want 1", closed)
	}
}

func TestOnRunnerConnectedFiresForEveryHello(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	broker := NewBroker(st, slog.New(slog.NewTextHandler(io.Discard, nil)))
	connected := make(chan struct{}, 4)
	broker.OnRunnerConnected(func() { connected <- struct{}{} })
	server := httptest.NewServer(http.HandlerFunc(broker.Handler))
	defer server.Close()
	address := "ws" + strings.TrimPrefix(server.URL, "http")

	for attempt := 1; attempt <= 2; attempt++ {
		connection, _, err := websocket.DefaultDialer.Dial(address, nil)
		if err != nil {
			t.Fatal(err)
		}
		hello := wireMessage{Version: ProtocolVersion, Type: messageHello, InstanceID: "laptop-1", Name: "Laptop"}
		if err := connection.WriteJSON(hello); err != nil {
			t.Fatal(err)
		}
		var welcome wireMessage
		if err := connection.ReadJSON(&welcome); err != nil || welcome.Type != messageWelcome {
			t.Fatalf("welcome %d = %+v, %v", attempt, welcome, err)
		}
		select {
		case <-connected:
		case <-time.After(5 * time.Second):
			t.Fatalf("no connect notification for hello %d", attempt)
		}
		connection.Close()
	}
}
