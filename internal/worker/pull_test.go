package worker

import (
	"bytes"
	"context"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"easyacp/internal/domain"
)

// A pull fetches the snapshot in 1 MiB pieces, survives a failing request
// by retrying that piece, reports progress on its stream, and loads the
// spooled bytes exactly once complete.
func TestSnapshotPullRetriesChunksAndLoadsTheWhole(t *testing.T) {
	payload := bytes.Repeat([]byte("layer-bytes-"), 200000) // ~2.3 MiB: three chunks
	var failures atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer secret" {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}
		if !strings.HasPrefix(r.URL.Path, "/api/snapshots/sha256:abc") {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		offset, _ := strconv.Atoi(r.URL.Query().Get("offset"))
		// The second piece fails once: the line hiccups.
		if offset == pullChunkBytes && failures.Add(1) == 1 {
			w.WriteHeader(http.StatusBadGateway)
			return
		}
		w.Header().Set("X-Spin-Size", strconv.Itoa(len(payload)))
		if offset >= len(payload) {
			w.WriteHeader(http.StatusRequestedRangeNotSatisfiable)
			return
		}
		end := min(offset+pullChunkBytes, len(payload))
		_, _ = w.Write(payload[offset:end])
	}))
	defer server.Close()
	client, err := newSnapshotClient(server.URL, "secret")
	if err != nil {
		t.Fatal(err)
	}
	importer := &recordingImporter{}
	process := newSnapshotPullProcess(context.Background(), importer, client, snapshotPullPayload{Snapshot: domain.CapsuleSnapshot{Driver: "docker", Ref: "spin/artifact:x", Digest: "sha256:abc"}, Size: int64(len(payload))})
	var progress []string
	buffer := make([]byte, 4096)
	for {
		count, err := process.Read(buffer)
		if count > 0 {
			progress = append(progress, string(buffer[:count]))
		}
		if err != nil {
			break
		}
	}
	done := make(chan struct{})
	go func() {
		execution, err := process.Wait()
		if err != nil || execution.ExitCode != 0 {
			t.Errorf("pull = %+v, %v", execution, err)
		}
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(30 * time.Second):
		t.Fatal("pull did not finish")
	}
	if !bytes.Equal(importer.got, payload) {
		t.Fatalf("loaded %d bytes, want %d", len(importer.got), len(payload))
	}
	joined := strings.Join(progress, "")
	last := strings.TrimSpace(joined[strings.LastIndex(strings.TrimSpace(joined), "\n")+1:])
	if received, total, ok := parsePullProgress(last); !ok || received != int64(len(payload)) || total != int64(len(payload)) {
		t.Fatalf("last progress line = %q", last)
	}
	if failures.Load() != 2 {
		t.Fatalf("the failing chunk was requested %d times, want the failure and one retry", failures.Load())
	}
}
