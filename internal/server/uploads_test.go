package server

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"sync"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
)

// A runner archives a snapshot through the same chunked upload a browser uses
// to restore a backup: short acknowledged requests, out of order, resumable.
func TestRunnerArchivesASnapshotThroughChunkedUploads(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	database, err := persistence.Open(filepath.Join(t.TempDir(), "spin.db"), persistence.OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer database.Close()
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{
		WorkerToken: "worker-secret", Database: database, SnapshotArchive: database,
	})
	handler := srv.Handler()
	content := make([]byte, 2*(1<<20)+777)
	if _, err := rand.Read(content); err != nil {
		t.Fatal(err)
	}
	snapshot := domain.CapsuleSnapshot{Driver: "docker", Ref: "spin/artifact:rec_live", Digest: "sha256:live", Restorable: true, ClientID: "cli_laptop"}
	send := func(method, path string, body []byte, headers map[string]string) (int, []byte) {
		t.Helper()
		request := httptest.NewRequest(method, path, bytes.NewReader(body))
		if body != nil {
			request.ContentLength = int64(len(body))
		}
		for key, value := range headers {
			request.Header.Set(key, value)
		}
		recorder := httptest.NewRecorder()
		handler.ServeHTTP(recorder, request)
		return recorder.Code, recorder.Body.Bytes()
	}
	bearer := map[string]string{"Authorization": "Bearer worker-secret", "Content-Type": "application/json"}
	createBody, _ := json.Marshal(map[string]any{"kind": "snapshot", "name": snapshot.Ref, "size": len(content), "snapshot": snapshot})

	// No token, no snapshot upload.
	if code, _ := send(http.MethodPost, "/api/uploads", createBody, map[string]string{"Content-Type": "application/json"}); code != http.StatusUnauthorized {
		t.Fatalf("anonymous snapshot upload status=%d", code)
	}
	code, body := send(http.MethodPost, "/api/uploads", createBody, bearer)
	var upload uploadResponse
	if code != http.StatusCreated || json.Unmarshal(body, &upload) != nil || upload.Kind != "snapshot" || upload.ChunkSize != 1<<20 || upload.Parallel < 1 {
		t.Fatalf("create snapshot upload status=%d body=%s", code, body)
	}
	if has, _ := database.HasSnapshot(context.Background(), snapshot); has {
		t.Fatal("snapshot visible before any chunk arrived")
	}

	// Chunks arrive concurrently, last first, and one is retried.
	offsets := []int64{2 << 20, 0, 1 << 20, 0}
	var group sync.WaitGroup
	failures := make(chan string, len(offsets))
	for _, offset := range offsets {
		end := min(offset+upload.ChunkSize, int64(len(content)))
		group.Add(1)
		go func() {
			defer group.Done()
			code, body := send(http.MethodPut, "/api/uploads/"+upload.ID, content[offset:end], map[string]string{
				"Authorization": "Bearer worker-secret", "Content-Type": "application/octet-stream", "X-Spin-Upload-Offset": fmt.Sprint(offset),
			})
			if code != http.StatusOK {
				failures <- fmt.Sprintf("chunk at %d: status %d body %s", offset, code, body)
			}
		}()
	}
	group.Wait()
	close(failures)
	for failure := range failures {
		t.Fatal(failure)
	}
	code, body = send(http.MethodGet, "/api/uploads/"+upload.ID, nil, bearer)
	var status uploadResponse
	if code != http.StatusOK || json.Unmarshal(body, &status) != nil || status.Offset != int64(len(content)) {
		t.Fatalf("status after chunks: %d %s", code, body)
	}
	code, body = send(http.MethodPost, "/api/uploads/"+upload.ID+"/complete", nil, bearer)
	var completed struct {
		Ref    string `json:"ref"`
		Digest string `json:"digest"`
		Size   int64  `json:"size"`
	}
	if code != http.StatusOK || json.Unmarshal(body, &completed) != nil || completed.Size != int64(len(content)) || completed.Ref != "snapshot:sha256:live" {
		t.Fatalf("complete status=%d body=%s", code, body)
	}
	has, err := database.HasSnapshot(context.Background(), snapshot)
	if err != nil || !has {
		t.Fatalf("archived snapshot: has=%t err=%v", has, err)
	}
	var restored bytes.Buffer
	if err := database.RestoreSnapshot(context.Background(), snapshot, &restored); err != nil || !bytes.Equal(restored.Bytes(), content) {
		t.Fatalf("restored snapshot differs (err=%v)", err)
	}
	if code, _ := send(http.MethodGet, "/api/uploads/"+upload.ID, nil, bearer); code != http.StatusNotFound {
		t.Fatalf("completed upload still addressable: %d", code)
	}
}
