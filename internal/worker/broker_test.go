package worker

import (
	"context"
	"io"
	"log/slog"
	"testing"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

func TestDrainingRunnerKeepsAffinityButLeavesRoundRobin(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	capabilities := domain.ClientCapabilities{Engine: domain.CapsuleEngineInfo{Driver: "docker", Available: true}}
	first, err := st.RegisterClient(domain.RegisterClientRequest{InstanceID: "first", Name: "Laptop", Capabilities: capabilities})
	if err != nil {
		t.Fatal(err)
	}
	second, err := st.RegisterClient(domain.RegisterClientRequest{InstanceID: "second", Name: "Server", Capabilities: capabilities})
	if err != nil {
		t.Fatal(err)
	}
	broker := NewBroker(st, slog.New(slog.NewTextHandler(io.Discard, nil)))
	firstPeer := broker.peer(first)
	secondPeer := broker.peer(second)
	firstPeer.mu.Lock()
	firstPeer.connected = true
	firstPeer.mu.Unlock()
	secondPeer.mu.Lock()
	secondPeer.connected = true
	secondPeer.mu.Unlock()

	drained, err := broker.SetDraining(first.ID, true)
	if err != nil {
		t.Fatal(err)
	}
	if !drained.Draining || drained.Status != "draining" {
		t.Fatalf("drained client = %+v", drained)
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	placed, err := broker.choose(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	if placed.id != second.ID {
		t.Fatalf("new placement chose drained client %s instead of %s", placed.id, second.ID)
	}
	affinity, err := broker.choose(ctx, first.ID)
	if err != nil {
		t.Fatal(err)
	}
	if affinity.id != first.ID {
		t.Fatalf("existing affinity moved from %s to %s", first.ID, affinity.id)
	}
	if _, available := firstPeer.engineInfo(); available {
		t.Fatal("drained runner still advertises its engine for new placement")
	}
	if _, err := broker.SetDraining(first.ID, false); err != nil {
		t.Fatal(err)
	}
	placed, err = broker.choose(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	if placed.id != first.ID {
		t.Fatalf("resumed runner did not rejoin round robin: got %s want %s", placed.id, first.ID)
	}
}
