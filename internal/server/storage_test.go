package server

import (
	"context"
	"io"
	"log/slog"
	"path/filepath"
	"testing"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
)

// exportingTestEngine can hand its snapshots to the archive, so a server
// with an archive seals layers the way production does.
type exportingTestEngine struct{ testEngine }

func (e *exportingTestEngine) ExportSnapshot(_ context.Context, snapshot domain.CapsuleSnapshot, destination io.Writer) error {
	_, err := io.WriteString(destination, "image "+snapshot.Digest)
	return err
}

// An EDIT supersedes a version; once nothing uses it, its archived snapshot
// goes and the storage line no longer counts it.
func TestSupersededSnapshotsArePruned(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	database, err := persistence.Open(filepath.Join(t.TempDir(), "spin.db"), persistence.OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer database.Close()
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &exportingTestEngine{}, ServerOptions{DisableAuthentication: true, Database: database, SnapshotArchive: database})
	first := buildLayers(t, srv, "derek", toolLayer("codex"))[0]
	ctx := context.Background()
	if has, err := database.HasSnapshot(ctx, first.Snapshot); err != nil || !has {
		t.Fatalf("sealed snapshot not archived: has=%v err=%v", has, err)
	}
	if pruned := srv.pruneSupersededSnapshots(ctx); pruned != 0 {
		t.Fatalf("a current version was pruned: %d", pruned)
	}
	editLayer(t, srv, "derek", "tool:codex")
	if pruned := srv.pruneSupersededSnapshots(ctx); pruned != 0 {
		t.Fatalf("a version under an open EDIT was pruned: %d", pruned)
	}
	// Saving the EDIT prunes in the background; calling it again here must
	// not matter.
	second := saveLayer(t, srv, "derek")
	if info := srv.storageInfo(ctx); info.DatabaseBytes == 0 {
		t.Fatalf("storage = %+v", info)
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		srv.pruneSupersededSnapshots(ctx)
		has, err := database.HasSnapshot(ctx, first.Snapshot)
		if err != nil {
			t.Fatal(err)
		}
		if !has {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("old snapshot still archived")
		}
		time.Sleep(10 * time.Millisecond)
	}
	if has, err := database.HasSnapshot(ctx, second.Snapshot); err != nil || !has {
		t.Fatalf("current snapshot was pruned: has=%v err=%v", has, err)
	}
	old, err := st.Artifact(first.ID)
	if err != nil || old.SnapshotPrunedAt == nil || old.SupersededBy != second.ID {
		t.Fatalf("old version after pruning = %+v, %v", old, err)
	}
	if info := srv.storageInfo(ctx); info.Prunable != 0 {
		t.Fatalf("storage after pruning = %+v", info)
	}
	if pruned := srv.pruneSupersededSnapshots(ctx); pruned != 0 {
		t.Fatalf("pruning is not idempotent: %d", pruned)
	}
}
