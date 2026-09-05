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
// setting it has, and END RECORD makes the result the version everything uses,
// including the layers that were built on the old one.
func TestEditCommandRecordsANewVersionThatEverythingFollows(t *testing.T) {
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

	run("derek", "RECORD tool:codex --scope=global --enable=acp --command=codex-acp")
	run("derek", "npm install -g codex-acp")
	first := run("derek", "END RECORD").Artifact
	run("derek", "RECORD credential:codex --scope=user --from=tool:codex")
	run("derek", "codex login")
	credential := run("derek", "END RECORD").Artifact

	// The edit starts from the current version and carries its settings.
	editing := run("derek", "EDIT tool:codex")
	if editing.Recording == nil || editing.Recording.Kind != domain.ArtifactTool || editing.Recording.Name != "codex" || editing.Recording.Scope != domain.ScopeGlobal {
		t.Fatalf("edit recording = %+v", editing.Recording)
	}
	if len(editing.Recording.Enables) != 1 || editing.Recording.Enables[0].Name != "acp" || editing.Recording.Enables[0].Command != "codex-acp" {
		t.Fatalf("edit lost the entrypoint: %+v", editing.Recording.Enables)
	}
	if len(editing.Recording.ParentArtifactIDs) != 1 || editing.Recording.ParentArtifactIDs[0] != first.ID || editing.Recording.ReplacesArtifactID != first.ID {
		t.Fatalf("edit does not build on the current version: %+v", editing.Recording)
	}
	if !strings.Contains(editing.Message, "EDITING tool:codex") {
		t.Fatalf("edit message = %q", editing.Message)
	}
	run("derek", "printf CODEX_CONFIG=... > /etc/spin/enabled/acp.env")
	second := run("derek", "END RECORD").Artifact
	if second == nil || second.ID == first.ID || second.Slot != "tool:codex" || len(second.Enables) != 1 {
		t.Fatalf("edited version = %+v", second)
	}

	// The old version stepped aside but still exists for what was built on it.
	if old := artifactByID(first.ID); old.SupersededBy != second.ID {
		t.Fatalf("old version = %+v", old)
	}
	listed := run("derek", "LIST tool")
	if len(listed.Artifacts) != 1 || listed.Artifacts[0].ID != second.ID {
		t.Fatalf("LIST after edit = %+v", listed.Artifacts)
	}
	if used := run("derek", "USE tool:codex"); used.Composition == nil || used.Composition.EntryArtifactID != second.ID {
		t.Fatalf("USE tool:codex after edit = %+v", used.Composition)
	}

	// A layer recorded from the old version follows the edit without being
	// recorded again: its composition binds the new version in the slot.
	used := run("derek", "USE credential:codex")
	if used.Composition == nil || used.Composition.SlotBindings["credential:codex"] != credential.ID {
		t.Fatalf("credential composition = %+v", used.Composition)
	}
	if used.Composition.SlotBindings["tool:codex"] != second.ID {
		t.Fatalf("slot tool:codex bound to %s, want the edited version %s", used.Composition.SlotBindings["tool:codex"], second.ID)
	}
	requested := strings.Join(used.Composition.RequestedArtifactIDs, ",")
	if !strings.Contains(requested, second.ID) || !strings.Contains(requested, credential.ID) {
		t.Fatalf("requested artifacts = %s", requested)
	}
	if len(used.Composition.Enabled) != 1 || used.Composition.Enabled[0].Command != "codex-acp" {
		t.Fatalf("enabled after edit = %+v", used.Composition.Enabled)
	}

	// A plain RECORD under the same name is a sibling, not an edit: nothing is
	// superseded and nothing follows it.
	run("derek", "RECORD tool:codex --scope=global --enable=acp --command=codex-acp")
	sibling := run("derek", "END RECORD").Artifact
	if edited := artifactByID(second.ID); edited.SupersededBy != "" {
		t.Fatalf("plain RECORD superseded the edited version: %+v", edited)
	}
	if again := run("derek", "USE credential:codex"); again.Composition.SlotBindings["tool:codex"] != second.ID {
		t.Fatalf("credential followed a sibling %s instead of its edit chain %s", sibling.ID, second.ID)
	}
}
