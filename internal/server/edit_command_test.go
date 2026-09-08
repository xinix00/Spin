package server

import (
	"io"
	"log/slog"
	"strings"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// EDIT is how a layer changes: the current version is recorded again with every
// setting it has, and saving makes the result the version everything uses,
// including the layers that were built on the old one.
func TestEditRecordsANewVersionThatEverythingFollows(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := New(st, slog.New(slog.NewTextHandler(io.Discard, nil)))
	artifactByID := func(id string) domain.Artifact {
		t.Helper()
		for _, artifact := range st.Snapshot().Artifacts {
			if artifact.ID == id {
				return artifact
			}
		}
		t.Fatalf("artifact %s not found", id)
		return domain.Artifact{}
	}
	codex := toolLayer("codex")
	codex.Enables = []domain.Enablement{{Name: "acp", Command: "codex-acp"}}
	codex.Install = "npm install -g codex-acp"
	first := buildLayers(t, srv, "derek", codex)[0]
	credential := buildLayers(t, srv, "derek", layerSpec{Kind: domain.ArtifactCredential, Name: "codex", Scope: domain.ScopeUser, From: "tool:codex", Install: "codex login"})[0]

	// The edit starts from the current version and carries its settings.
	editing := editLayer(t, srv, "derek", "tool:codex")
	if editing.Kind != domain.ArtifactTool || editing.Name != "codex" || editing.Scope != domain.ScopeGlobal {
		t.Fatalf("edit recording = %+v", editing)
	}
	if len(editing.Enables) != 1 || editing.Enables[0].Name != "acp" || editing.Enables[0].Command != "codex-acp" {
		t.Fatalf("edit lost the entrypoint: %+v", editing.Enables)
	}
	if len(editing.ParentArtifactIDs) != 1 || editing.ParentArtifactIDs[0] != first.ID || editing.ReplacesArtifactID != first.ID {
		t.Fatalf("edit does not build on the current version: %+v", editing)
	}
	runInRecording(t, srv, "derek", "printf CODEX_CONFIG=... > /etc/spin/enabled/acp.env")
	second := saveLayer(t, srv, "derek")
	if second.ID == first.ID || second.Slot != "tool:codex" || len(second.Enables) != 1 {
		t.Fatalf("edited version = %+v", second)
	}

	// The old version stepped aside but still exists for what was built on it.
	if old := artifactByID(first.ID); old.SupersededBy != second.ID {
		t.Fatalf("old version = %+v", old)
	}
	if _, _, err := srv.editCapsuleArtifact("derek", first.ID); err == nil {
		t.Fatal("editing an older version was accepted")
	}
	current := 0
	for _, artifact := range st.Snapshot().Artifacts {
		if artifact.Kind == domain.ArtifactTool && artifact.SupersededBy == "" {
			current++
			if artifact.ID != second.ID {
				t.Fatalf("current tool layer = %+v, want %s", artifact, second.ID)
			}
		}
	}
	if current != 1 {
		t.Fatalf("current tool layers = %d", current)
	}
	if used := useLayers(t, srv, "derek", "tool:codex"); used.EntryArtifactID != second.ID {
		t.Fatalf("USE tool:codex after edit = %+v", used)
	}

	// A layer recorded from the old version follows the edit without being
	// recorded again: its composition binds the new version in the slot.
	used := useLayers(t, srv, "derek", "credential:codex")
	if used.SlotBindings["credential:codex"] != credential.ID {
		t.Fatalf("credential composition = %+v", used)
	}
	if used.SlotBindings["tool:codex"] != second.ID {
		t.Fatalf("slot tool:codex bound to %s, want the edited version %s", used.SlotBindings["tool:codex"], second.ID)
	}
	// The stack holds the edited version right under the credential.
	stack := strings.Join(used.Layers, ",")
	if !strings.Contains(stack, second.ID+","+credential.ID) {
		t.Fatalf("stack = %s, want %s right under %s", stack, second.ID, credential.ID)
	}
	if len(used.Enabled) != 1 || used.Enabled[0].Command != "codex-acp" {
		t.Fatalf("enabled after edit = %+v", used.Enabled)
	}

	// A plain RECORD under the same name is a sibling, not an edit: nothing is
	// superseded and nothing follows it.
	codex.Install = ""
	sibling := buildLayers(t, srv, "derek", codex)[0]
	if edited := artifactByID(second.ID); edited.SupersededBy != "" {
		t.Fatalf("plain RECORD superseded the edited version: %+v", edited)
	}
	if again := useLayers(t, srv, "derek", "credential:codex"); again.SlotBindings["tool:codex"] != second.ID {
		t.Fatalf("credential followed a sibling %s instead of its edit chain %s", sibling.ID, second.ID)
	}
}
