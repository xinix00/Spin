package persistence

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"path/filepath"
	"sync"
	"testing"

	"easyacp/internal/domain"
)

// A runner archives a snapshot as separate 1 MiB requests, in whatever order
// they land. The archive holds nothing visible until every chunk is verified.
func TestSnapshotUploadAssemblesChunksIntoTheArchive(t *testing.T) {
	database, err := Open(filepath.Join(t.TempDir(), "spin.db"), OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer database.Close()
	ctx := context.Background()
	content := make([]byte, 2*blobChunkSize+12345)
	if _, err := rand.Read(content); err != nil {
		t.Fatal(err)
	}
	snapshot := domain.CapsuleSnapshot{Driver: "docker", Ref: "spin/artifact:rec_test", Digest: "sha256:image", Restorable: true}

	upload, err := database.BeginSnapshotUpload(ctx, snapshot, int64(len(content)))
	if err != nil {
		t.Fatal(err)
	}
	if has, _ := database.HasSnapshot(ctx, snapshot); has {
		t.Fatal("an unfinished upload is visible as an archived snapshot")
	}
	// Misaligned chunks are refused: sequence and offset must stay one thing.
	if _, err := upload.WriteAt(ctx, 10, 100, bytes.NewReader(content[10:110])); err == nil {
		t.Fatal("misaligned chunk was accepted")
	}
	if _, err := upload.Complete(ctx); err == nil {
		t.Fatal("completed an empty upload")
	}
	// Chunks arrive concurrently, last one first.
	offsets := []int64{2 * blobChunkSize, 0, blobChunkSize}
	var group sync.WaitGroup
	errs := make(chan error, len(offsets))
	for _, offset := range offsets {
		end := min(offset+blobChunkSize, int64(len(content)))
		group.Add(1)
		go func() {
			defer group.Done()
			if _, err := upload.WriteAt(ctx, offset, end-offset, bytes.NewReader(content[offset:end])); err != nil {
				errs <- err
			}
		}()
	}
	group.Wait()
	close(errs)
	for err := range errs {
		t.Fatal(err)
	}
	// A retried chunk is a plain replace.
	if committed, err := upload.WriteAt(ctx, 0, blobChunkSize, bytes.NewReader(content[:blobChunkSize])); err != nil || committed != int64(len(content)) {
		t.Fatalf("retry committed=%d error=%v", committed, err)
	}
	info, err := upload.Complete(ctx)
	if err != nil || info.Size != int64(len(content)) {
		t.Fatalf("complete = %+v, %v", info, err)
	}
	if has, err := database.HasSnapshot(ctx, snapshot); err != nil || !has {
		t.Fatalf("archived snapshot missing: has=%t err=%v", has, err)
	}
	var restored bytes.Buffer
	if err := database.RestoreSnapshot(ctx, snapshot, &restored); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(restored.Bytes(), content) {
		t.Fatal("restored snapshot differs from what was uploaded")
	}
	if _, err := upload.Complete(ctx); err == nil {
		t.Fatal("completed the same upload twice")
	}

	// An abandoned upload leaves nothing behind.
	abandoned, err := database.BeginSnapshotUpload(ctx, domain.CapsuleSnapshot{Digest: "sha256:other"}, blobChunkSize)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := abandoned.WriteAt(ctx, 0, blobChunkSize, bytes.NewReader(content[:blobChunkSize])); err != nil {
		t.Fatal(err)
	}
	if err := abandoned.Close(); err != nil {
		t.Fatal(err)
	}
	var count int
	if err := database.db.QueryRow(`SELECT COUNT(*) FROM spin_object_chunks c JOIN spin_objects o ON o.id = c.object_id WHERE o.complete = 0`).Scan(&count); err != nil || count != 0 {
		t.Fatalf("orphan chunks after abort: %d, %v", count, err)
	}
	if _, err := abandoned.WriteAt(ctx, 0, 1, bytes.NewReader([]byte{1})); err == nil {
		t.Fatal("wrote into an abandoned upload")
	}
	if _, err := upload.WriteAt(ctx, 0, 1, bytes.NewReader([]byte{1})); !errors.Is(err, err) || err == nil {
		t.Fatal("wrote into a completed upload")
	}
}

// The digest is taken while the chunks arrive, so completing a snapshot of
// gigabytes does not read it back under the server's one connection. A chunk
// written twice ahead of the prefix makes that running digest unreliable, and
// Complete reads the rows back instead; both ways the digest is the content's.
func TestSnapshotUploadDigestIsTheContentsWhateverTheOrder(t *testing.T) {
	database, err := Open(filepath.Join(t.TempDir(), "spin.db"), OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer database.Close()
	ctx := context.Background()
	content := make([]byte, 3*blobChunkSize+777)
	if _, err := rand.Read(content); err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256(content)
	want := "sha256:" + hex.EncodeToString(sum[:])
	write := func(upload *SnapshotUpload, offset int64) {
		t.Helper()
		end := min(offset+blobChunkSize, int64(len(content)))
		if _, err := upload.WriteAt(ctx, offset, end-offset, bytes.NewReader(content[offset:end])); err != nil {
			t.Fatal(err)
		}
	}
	for name, order := range map[string][]int64{
		"running digest": {3 * blobChunkSize, blobChunkSize, 0, 2 * blobChunkSize},
		"read back":      {blobChunkSize, blobChunkSize, 0, 2 * blobChunkSize, 3 * blobChunkSize},
	} {
		upload, err := database.BeginSnapshotUpload(ctx, domain.CapsuleSnapshot{Driver: "docker", Ref: "spin/artifact:" + name, Digest: "sha256:" + name, Restorable: true}, int64(len(content)))
		if err != nil {
			t.Fatal(err)
		}
		for _, offset := range order {
			write(upload, offset)
		}
		_, running := upload.runningDigest()
		if running != (name == "running digest") {
			t.Fatalf("%s: running digest usable = %v", name, running)
		}
		info, err := upload.Complete(ctx)
		if err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		if info.Digest != want {
			t.Fatalf("%s: digest %s, content is %s", name, info.Digest, want)
		}
	}
}
