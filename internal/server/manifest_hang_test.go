package server

import (
	"bytes"
	"encoding/base64"
	"io"
	"log/slog"
	"path/filepath"
	"testing"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
)

func TestManifestRoundTripThroughTheDatabase(t *testing.T) {
	database, err := persistence.Open(filepath.Join(t.TempDir(), "spin.db"), persistence.OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer database.Close()
	st, err := store.OpenWithBackend("state", store.OpenOptions{MasterKey: base64.RawStdEncoding.EncodeToString(bytes.Repeat([]byte{0x11}, 32))}, database)
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), capsule.Journal{}, ServerOptions{DisableAuthentication: true, Database: database})
	entries := []domain.ContentEntry{}
	for i := 0; i < 10; i++ {
		entries = append(entries, domain.ContentEntry{Path: "/root/.claude/file" + string(rune('a'+i)), Bytes: 12})
	}
	done := make(chan []domain.ContentEntry, 1)
	go func() {
		srv.detachManifest("artifact:x", &domain.LayerContents{Entries: entries})
		done <- srv.manifestEntries("artifact:x")
	}()
	select {
	case got := <-done:
		if len(got) != 10 {
			t.Fatalf("got %d entries", len(got))
		}
	case <-time.After(10 * time.Second):
		t.Fatal("manifest round trip hangs")
	}
}
