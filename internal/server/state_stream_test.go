package server

import (
	"io"
	"log/slog"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"easyacp/internal/store"
	"github.com/gorilla/websocket"
)

// The browser is pushed the state: whole on connect, again after a change,
// with a version that only goes up.
func TestStateStreamPushesTheStateOnChange(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true})
	server := httptest.NewServer(srv.Handler())
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http")+"/api/state/ws", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	read := func() stateResponse {
		t.Helper()
		_ = connection.SetReadDeadline(time.Now().Add(5 * time.Second))
		var state stateResponse
		if err := connection.ReadJSON(&state); err != nil {
			t.Fatalf("read state: %v", err)
		}
		return state
	}
	first := read()
	if len(first.Artifacts) != 0 {
		t.Fatalf("initial state = %+v", first)
	}
	recordLayer(t, srv, "derek", gitLayer())
	saveLayer(t, srv, "derek")
	deadline := time.Now().Add(5 * time.Second)
	for {
		next := read()
		if next.Version <= first.Version {
			t.Fatalf("version did not advance: %d then %d", first.Version, next.Version)
		}
		if len(next.Artifacts) == 1 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the recorded artifact never arrived over the stream")
		}
	}
}
