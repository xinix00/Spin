package worker

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"easyacp/internal/domain"
)

// The runner's uploader speaks the same contract a browser does: create, chunks
// at offsets with the committed prefix coming back, complete. A chunk that
// fails once is retried; nothing is lost and nothing is sent twice for good.
func TestUploadSnapshotSendsChunksInParallelAndRetriesOnce(t *testing.T) {
	content := make([]byte, 3*(1<<20)+4321)
	if _, err := rand.Read(content); err != nil {
		t.Fatal(err)
	}
	var (
		mu        sync.Mutex
		chunks    = map[int64][]byte{}
		failed    atomic.Bool
		completed atomic.Bool
		puts      atomic.Int32
	)
	prefix := func() int64 {
		var offset int64
		for {
			chunk, ok := chunks[offset]
			if !ok {
				return offset
			}
			offset += int64(len(chunk))
		}
	}
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer worker-secret" {
			http.Error(w, `{"error":"no token"}`, http.StatusUnauthorized)
			return
		}
		switch {
		case r.Method == http.MethodPost && r.URL.Path == "/api/uploads":
			var request struct {
				Kind string `json:"kind"`
				Size int64  `json:"size"`
			}
			_ = json.NewDecoder(r.Body).Decode(&request)
			if request.Kind != "snapshot" || request.Size != int64(len(content)) {
				http.Error(w, `{"error":"bad create"}`, http.StatusBadRequest)
				return
			}
			w.WriteHeader(http.StatusCreated)
			_ = json.NewEncoder(w).Encode(map[string]any{"id": "up_1", "offset": 0, "chunk_size": 1 << 20, "parallel": 3, "size": request.Size})
		case r.Method == http.MethodPut && r.URL.Path == "/api/uploads/up_1":
			puts.Add(1)
			offset, _ := strconv.ParseInt(r.Header.Get("X-Spin-Upload-Offset"), 10, 64)
			data, _ := io.ReadAll(r.Body)
			if offset == 1<<20 && failed.CompareAndSwap(false, true) {
				http.Error(w, `{"error":"tunnel hiccup"}`, http.StatusBadGateway)
				return
			}
			mu.Lock()
			chunks[offset] = data
			committed := prefix()
			mu.Unlock()
			_ = json.NewEncoder(w).Encode(map[string]any{"id": "up_1", "offset": committed, "size": len(content)})
		case r.Method == http.MethodPost && r.URL.Path == "/api/uploads/up_1/complete":
			mu.Lock()
			committed := prefix()
			mu.Unlock()
			if committed != int64(len(content)) {
				http.Error(w, `{"error":"incomplete"}`, http.StatusConflict)
				return
			}
			completed.Store(true)
			_ = json.NewEncoder(w).Encode(map[string]any{"ref": "snapshot:sha256:live", "digest": "sha256:content", "size": len(content)})
		default:
			http.Error(w, `{"error":"unexpected `+r.Method+` `+r.URL.Path+`"}`, http.StatusNotFound)
		}
	}))
	defer server.Close()

	client, err := newUploadClient(strings.Replace(server.URL, "http", "ws", 1), "worker-secret")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasSuffix(client.base, "/api/uploads") || !strings.HasPrefix(client.base, "http://") {
		t.Fatalf("upload base = %s", client.base)
	}
	result, err := uploadSnapshot(context.Background(), client, domain.CapsuleSnapshot{Ref: "spin/artifact:rec_live", Digest: "sha256:live"}, bytes.NewReader(content), int64(len(content)))
	if err != nil {
		t.Fatal(err)
	}
	if result.Ref != "snapshot:sha256:live" || result.Size != int64(len(content)) || !completed.Load() {
		t.Fatalf("archive result = %+v completed=%t", result, completed.Load())
	}
	var assembled []byte
	for offset := int64(0); offset < int64(len(content)); {
		assembled = append(assembled, chunks[offset]...)
		offset += int64(len(chunks[offset]))
	}
	if !bytes.Equal(assembled, content) {
		t.Fatal("server assembled different bytes than the runner exported")
	}
	if !failed.Load() || puts.Load() != 5 {
		t.Fatalf("expected 4 chunks plus one retry, saw %d puts (failed=%t)", puts.Load(), failed.Load())
	}
}
