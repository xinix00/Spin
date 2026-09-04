package persistence

import (
	"bytes"
	"context"
	"errors"
	"io/fs"
	"path/filepath"
	"strings"
	"sync"
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

func TestSQLiteBackupUploadResumesAtCommittedOffset(t *testing.T) {
	ctx := context.Background()
	source, err := Open(filepath.Join(t.TempDir(), "source.db"), OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if err := source.WriteFile("state", []byte(`{"chunked":true}`)); err != nil {
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
	upload, err := destination.BeginBackupUpload(int64(backup.Len()) + backupUploadWindow + 2)
	if err != nil {
		t.Fatal(err)
	}
	defer upload.Close()
	size := int64(backup.Len())
	const chunk = 997
	// Out of order: the second chunk lands first and waits ahead of the prefix.
	if committed, err := upload.WriteAt(ctx, chunk, chunk, bytes.NewReader(backup.Bytes()[chunk:2*chunk])); err != nil || committed != 0 {
		t.Fatalf("ahead write committed=%d error=%v", committed, err)
	}
	if _, err := upload.Stage(size); err == nil || !strings.Contains(err.Error(), "incomplete") {
		t.Fatalf("stage incomplete upload error = %v", err)
	}
	if committed, err := upload.WriteAt(ctx, 0, chunk, bytes.NewReader(backup.Bytes()[:chunk])); err != nil || committed != 2*chunk {
		t.Fatalf("first write committed=%d error=%v", committed, err)
	}
	// Retrying a committed chunk is idempotent; straddling the prefix or leaving the window is refused.
	if committed, err := upload.WriteAt(ctx, 0, chunk, bytes.NewReader(bytes.Repeat([]byte{0xff}, chunk))); err != nil || committed != 2*chunk {
		t.Fatalf("duplicate write committed=%d error=%v", committed, err)
	}
	if committed, err := upload.WriteAt(ctx, chunk, 2*chunk, bytes.NewReader(backup.Bytes()[chunk:3*chunk])); !errors.Is(err, ErrBackupUploadOffset) || committed != 2*chunk {
		t.Fatalf("straddling write committed=%d error=%v", committed, err)
	}
	if _, err := upload.WriteAt(ctx, 2*chunk+backupUploadWindow+1, 1, bytes.NewReader([]byte{1})); !errors.Is(err, ErrBackupUploadOffset) {
		t.Fatalf("write beyond window error = %v", err)
	}
	// The remaining chunks stream concurrently in whatever order the scheduler picks.
	var group sync.WaitGroup
	writeErrors := make(chan error, int(size/chunk)+1)
	for offset := int64(2 * chunk); offset < size; offset += chunk {
		start, end := offset, offset+chunk
		if end > size {
			end = size
		}
		group.Add(1)
		go func() {
			defer group.Done()
			if _, err := upload.WriteAt(ctx, start, end-start, bytes.NewReader(backup.Bytes()[start:end])); err != nil {
				writeErrors <- err
			}
		}()
	}
	group.Wait()
	close(writeErrors)
	for err := range writeErrors {
		t.Fatal(err)
	}
	if upload.Offset() != size {
		t.Fatalf("committed prefix = %d, want %d", upload.Offset(), size)
	}
	staged, err := upload.Stage(int64(backup.Len()))
	if err != nil {
		t.Fatal(err)
	}
	defer staged.Close()
	if staged.MasterKey != "portable-secret" {
		t.Fatalf("master key = %q", staged.MasterKey)
	}
	state, err := staged.Database.ReadFile("state")
	if err != nil || string(state) != `{"chunked":true}` {
		t.Fatalf("state = %q, %v", state, err)
	}
}
