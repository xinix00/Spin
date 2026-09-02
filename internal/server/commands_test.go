package server

import (
	"io"
	"log/slog"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

func TestCommandRecordingAndUseFlow(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := New(st, slog.New(slog.NewTextHandler(io.Discard, nil)))

	run := func(operator, line string) domain.CommandResponse {
		t.Helper()
		response, err := srv.runCommand(domain.CommandRequest{Operator: operator, Line: line})
		if err != nil {
			t.Fatalf("%s: %v", line, err)
		}
		return response
	}

	run("derek", "RECORD tool:codex --scope=global --enable=acp --command=codex-acp")
	run("derek", "npm install -g @openai/codex")
	tool := run("derek", "END RECORD").Artifact
	if tool == nil || tool.Slot != "tool:codex" {
		t.Fatalf("unexpected tool: %+v", tool)
	}

	run("derek", "RECORD credential:codex --scope=user --from=tool:codex")
	run("derek", "codex /login")
	credential := run("derek", "END RECORD").Artifact
	if credential == nil || credential.Subject != "derek" || credential.Slot != "credential:codex" {
		t.Fatalf("unexpected credential: %+v", credential)
	}

	used := run("derek", "USE credential:codex")
	if used.Composition == nil || used.Composition.SlotBindings["credential:codex"] != credential.ID {
		t.Fatalf("unexpected composition: %+v", used.Composition)
	}
	if len(used.Composition.Enabled) != 1 || used.Composition.Enabled[0].Name != "acp" || used.Composition.Enabled[0].Command != "codex-acp" {
		t.Fatalf("unexpected enabled capabilities: %+v", used.Composition.Enabled)
	}
	listed := run("derek", "LIST credential")
	if len(listed.Artifacts) != 1 || listed.Artifacts[0].ID != credential.ID {
		t.Fatalf("unexpected list: %+v", listed.Artifacts)
	}
}

func TestUseCommandStacksWithSelectorsWithoutChangingTheEntryTool(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := New(st, slog.New(slog.NewTextHandler(io.Discard, nil)))
	run := func(line string) domain.CommandResponse {
		t.Helper()
		response, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line})
		if err != nil {
			t.Fatalf("%s: %v", line, err)
		}
		return response
	}

	run("RECORD tool:codex --scope=global")
	codex := run("END RECORD").Artifact
	run("RECORD tool:dotnet --scope=global --from=tool:codex")
	dotnet := run("END RECORD").Artifact
	used := run("USE tool:codex WITH tool:dotnet WITH tool:dotnet --profile=default")
	if used.Composition == nil {
		t.Fatal("USE returned no composition")
	}
	composition := *used.Composition
	if composition.Tool != "codex" || len(composition.WithSelectors) != 1 || composition.WithSelectors[0] != "tool:dotnet" {
		t.Fatalf("entry/WITH contract = %+v", composition)
	}
	if len(composition.RequestedArtifactIDs) != 2 || composition.RequestedArtifactIDs[0] != codex.ID || composition.RequestedArtifactIDs[1] != dotnet.ID {
		t.Fatalf("requested artifacts = %+v", composition.RequestedArtifactIDs)
	}
	if composition.SlotBindings["tool:codex"] != codex.ID || composition.SlotBindings["tool:dotnet"] != dotnet.ID {
		t.Fatalf("slot bindings = %+v", composition.SlotBindings)
	}

	if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "USE tool:codex WITH"}); err == nil {
		t.Fatal("incomplete WITH unexpectedly succeeded")
	}
}
