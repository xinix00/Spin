package worker

import (
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

// A restarted server takes up the processes it knew by their stream: what
// the runner kept arrives once it reconnects, and a process the runner no
// longer runs ends instead of being waited on for ever.
func TestAdoptedStreamsFollowWhatTheRunnerStillRuns(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	broker := NewBroker(st, slog.New(slog.NewTextHandler(io.Discard, nil)))
	attached := make(chan string, 4)
	broker.OnRunnerAttached(func(clientID string) { attached <- clientID })
	server := httptest.NewServer(http.HandlerFunc(broker.Handler))
	defer server.Close()
	address := "ws" + strings.TrimPrefix(server.URL, "http")
	connect := func(streams ...string) (*websocket.Conn, domain.Client) {
		t.Helper()
		connection, _, err := websocket.DefaultDialer.Dial(address, nil)
		if err != nil {
			t.Fatal(err)
		}
		hello := wireMessage{Version: ProtocolVersion, Type: messageHello, InstanceID: "laptop-1", Name: "Laptop", Process: "p1", Streams: streams, StreamsReported: true}
		if err := connection.WriteJSON(hello); err != nil {
			t.Fatal(err)
		}
		var welcome wireMessage
		if err := connection.ReadJSON(&welcome); err != nil || welcome.Type != messageWelcome || welcome.Client == nil {
			t.Fatalf("welcome = %+v, %v", welcome, err)
		}
		select {
		case id := <-attached:
			if id != welcome.Client.ID {
				t.Fatalf("attached %q, want %q", id, welcome.Client.ID)
			}
		case <-time.After(5 * time.Second):
			t.Fatal("no attach notification")
		}
		return connection, *welcome.Client
	}

	first, client := connect()
	first.Close()
	for deadline := time.Now().Add(5 * time.Second); broker.Connected(client.ID); time.Sleep(5 * time.Millisecond) {
		if time.Now().After(deadline) {
			t.Fatal("the runner stayed connected")
		}
	}

	kept, err := broker.adoptStream(client.ID, "str_agent")
	if err != nil {
		t.Fatal(err)
	}
	lost, err := broker.adoptStream(client.ID, "str_gone")
	if err != nil {
		t.Fatal(err)
	}
	again, _ := broker.adoptStream(client.ID, "str_agent")
	if again != kept {
		t.Fatal("adopting a stream twice made two processes")
	}

	finished, err := broker.adoptStream(client.ID, "str_finished")
	if err != nil {
		t.Fatal(err)
	}
	// This process finished offline, but its final bytes and exit still
	// wait in the runner's outbox. Hello must not prematurely close it.
	runner := New(Config{}, slog.New(slog.NewTextHandler(io.Discard, nil)))
	runner.enqueue(wireMessage{Version: ProtocolVersion, Type: messageStreamData, ID: "str_finished", Data: []byte("final reply\n")})
	runner.enqueue(wireMessage{Version: ProtocolVersion, Type: messageStreamExit, ID: "str_finished"})
	second, _ := connect(append(runner.streamIDs(), "str_agent")...)
	defer second.Close()
	select {
	case <-finished.done:
		t.Fatal("finished stream was closed before its buffered reply arrived")
	default:
	}
	for _, message := range runner.outbox {
		if err := second.WriteJSON(message); err != nil {
			t.Fatal(err)
		}
	}
	output, err := io.ReadAll(finished)
	if err != nil || string(output) != "final reply\n" {
		t.Fatalf("finished stream output = %q, %v", output, err)
	}
	if _, err := lost.Wait(); err == nil || !strings.Contains(err.Error(), "no longer runs") {
		t.Fatalf("a process the runner lost ended with %v", err)
	}
	if err := second.WriteJSON(wireMessage{Version: ProtocolVersion, Type: messageStreamData, ID: "str_agent", Data: []byte("kept while away\n")}); err != nil {
		t.Fatal(err)
	}
	buffer := make([]byte, 64)
	count, err := kept.Read(buffer)
	if err != nil || string(buffer[:count]) != "kept while away\n" {
		t.Fatalf("adopted stream read %q, %v", buffer[:count], err)
	}
}
