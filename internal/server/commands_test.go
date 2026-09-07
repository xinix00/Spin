package server

import (
	"context"
	"io"
	"log/slog"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// A tool layer, a user's credential layer on top of it, and a composition
// that picks the credential and carries the tool's ACP entrypoint.
func TestRecordingAndUseFlow(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := New(st, slog.New(slog.NewTextHandler(io.Discard, nil)))

	codex := toolLayer("codex")
	codex.Enables = []domain.Enablement{{Name: "acp", Command: "codex-acp"}}
	codex.Install = "npm install -g @openai/codex"
	tool := buildLayers(t, srv, "derek", codex)[0]
	if tool.Slot != "tool:codex" {
		t.Fatalf("unexpected tool: %+v", tool)
	}

	credential := buildLayers(t, srv, "derek", layerSpec{Kind: domain.ArtifactCredential, Name: "codex", Scope: domain.ScopeUser, From: "tool:codex", Install: "codex /login"})[0]
	if credential.Subject != "derek" || credential.Slot != "credential:codex" {
		t.Fatalf("unexpected credential: %+v", credential)
	}

	used := useLayers(t, srv, "derek", "credential:codex")
	if used.SlotBindings["credential:codex"] != credential.ID {
		t.Fatalf("unexpected composition: %+v", used)
	}
	if len(used.Enabled) != 1 || used.Enabled[0].Name != "acp" || used.Enabled[0].Command != "codex-acp" {
		t.Fatalf("unexpected enabled capabilities: %+v", used.Enabled)
	}
	credentials := 0
	for _, artifact := range st.Snapshot().Artifacts {
		if artifact.Kind == domain.ArtifactCredential && artifact.SupersededBy == "" {
			credentials++
		}
	}
	if credentials != 1 {
		t.Fatalf("credential layers = %d", credentials)
	}
}

func TestUseStacksWithLayersWithoutChangingTheEntryTool(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := New(st, slog.New(slog.NewTextHandler(io.Discard, nil)))
	codex := toolLayer("codex")
	codex.Install = ""
	dotnet := toolLayer("dotnet")
	dotnet.From, dotnet.Install = "tool:codex", ""
	layers := buildLayers(t, srv, "derek", codex, dotnet)
	composition := useLayers(t, srv, "derek", "tool:codex", "tool:dotnet", "tool:dotnet")
	if composition.Tool != "codex" || len(composition.WithSelectors) != 1 || composition.WithSelectors[0] != "tool:dotnet" {
		t.Fatalf("entry/WITH contract = %+v", composition)
	}
	if len(composition.RequestedArtifactIDs) != 2 || composition.RequestedArtifactIDs[0] != layers[0].ID || composition.RequestedArtifactIDs[1] != layers[1].ID {
		t.Fatalf("requested artifacts = %+v", composition.RequestedArtifactIDs)
	}
	if composition.SlotBindings["tool:codex"] != layers[0].ID || composition.SlotBindings["tool:dotnet"] != layers[1].ID {
		t.Fatalf("slot bindings = %+v", composition.SlotBindings)
	}
	if _, err := srv.useCapsule(context.Background(), domain.UseRequest{Operator: "derek", Selector: "tool:codex", WithSelectors: []string{"nonsense"}}); err == nil {
		t.Fatal("a WITH layer without kind:name unexpectedly succeeded")
	}
}
