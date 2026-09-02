package persistence

import (
	"bytes"
	"context"
	"errors"
	"io/fs"
	"path/filepath"
	"testing"
)

func TestSQLitePersistsStateAndDeduplicatedBlobs(t *testing.T) {
	path := filepath.Join(t.TempDir(), "spin.db")
	database, err := Open(path, OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if err := database.WriteFile("state", []byte(`{"jobs":1}`)); err != nil {
		t.Fatal(err)
	}
	payload := bytes.Repeat([]byte("spin-binary\x00"), 200000)
	first, err := database.PutBlob(context.Background(), "snapshot:first", "docker-snapshot", bytes.NewReader(payload))
	if err != nil {
		t.Fatal(err)
	}
	second, err := database.PutBlob(context.Background(), "snapshot:second", "docker-snapshot", bytes.NewReader(payload))
	if err != nil {
		t.Fatal(err)
	}
	if first.Digest != second.Digest || first.Size != int64(len(payload)) {
		t.Fatalf("blob metadata = %+v / %+v", first, second)
	}
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}

	database, err = Open(path, OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer database.Close()
	state, err := database.ReadFile("state")
	if err != nil || string(state) != `{"jobs":1}` {
		t.Fatalf("state = %q, %v", state, err)
	}
	var restored bytes.Buffer
	if _, err := database.WriteBlobTo(context.Background(), "snapshot:second", &restored); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(restored.Bytes(), payload) {
		t.Fatal("restored blob differs")
	}
	if err := database.DeleteBlob(context.Background(), "snapshot:first"); err != nil {
		t.Fatal(err)
	}
	if _, err := database.BlobInfo(context.Background(), "snapshot:second"); err != nil {
		t.Fatalf("shared object was removed: %v", err)
	}
	if err := database.DeleteBlob(context.Background(), "snapshot:second"); err != nil {
		t.Fatal(err)
	}
	if _, err := database.BlobInfo(context.Background(), "snapshot:second"); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("deleted blob error = %v", err)
	}
}

func TestSQLiteFileStore(t *testing.T) {
	database, err := Open(filepath.Join(t.TempDir(), "spin.db"), OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer database.Close()
	files := database.Files("attachment:", "attachment", 32)
	if err := files.WriteFile("one", []byte("hello")); err != nil {
		t.Fatal(err)
	}
	got, err := files.ReadFile("one")
	if err != nil || string(got) != "hello" {
		t.Fatalf("read = %q, %v", got, err)
	}
	if err := files.WriteFile("large", bytes.Repeat([]byte("x"), 33)); err == nil {
		t.Fatal("expected size error")
	}
	if err := files.Remove("one"); err != nil {
		t.Fatal(err)
	}
}

func TestSQLiteBackupAndRestoreIsOneDatabase(t *testing.T) {
	ctx := context.Background()
	source, err := Open(filepath.Join(t.TempDir(), "source.db"), OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if err := source.WriteFile("state", []byte(`{"source":true}`)); err != nil {
		t.Fatal(err)
	}
	payload := bytes.Repeat([]byte("snapshot"), 200000)
	if _, err := source.PutBlob(ctx, "snapshot:one", "docker-snapshot", bytes.NewReader(payload)); err != nil {
		t.Fatal(err)
	}
	var backup bytes.Buffer
	if err := source.WriteBackup(ctx, &backup, "portable-secret"); err != nil {
		t.Fatal(err)
	}
	if err := source.Close(); err != nil {
		t.Fatal(err)
	}

	destination, err := Open(filepath.Join(t.TempDir(), "destination.db"), OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer destination.Close()
	if err := destination.WriteFile("state", []byte(`{"destination":true}`)); err != nil {
		t.Fatal(err)
	}
	staged, err := destination.StageBackup(ctx, bytes.NewReader(backup.Bytes()), int64(backup.Len()))
	if err != nil {
		t.Fatal(err)
	}
	defer staged.Close()
	if staged.MasterKey != "portable-secret" {
		t.Fatalf("master key = %q", staged.MasterKey)
	}
	state, err := staged.Database.ReadFile("state")
	if err != nil || string(state) != `{"source":true}` {
		t.Fatalf("staged state = %q, %v", state, err)
	}
	if err := destination.RestoreFrom(ctx, staged); err != nil {
		t.Fatal(err)
	}
	state, err = destination.ReadFile("state")
	if err != nil || string(state) != `{"source":true}` {
		t.Fatalf("restored state = %q, %v", state, err)
	}
	var restored bytes.Buffer
	if _, err := destination.WriteBlobTo(ctx, "snapshot:one", &restored); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(restored.Bytes(), payload) {
		t.Fatal("backup lost snapshot payload")
	}
	if _, err := destination.ReadFile(backupKeyKey); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("live database retained portable key: %v", err)
	}
}
