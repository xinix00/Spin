package replica

import (
	"bytes"
	"context"
	"encoding/xml"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"sort"
	"strings"
	"sync"
	"testing"

	"easyacp/internal/persistence"

	"github.com/ncruces/go-sqlite3/vfs"
)

// fakeBucket is an S3 endpoint in memory: put, get, delete, list-type=2.
type fakeBucket struct {
	mu      sync.Mutex
	objects map[string][]byte
	puts    int
}

func (b *fakeBucket) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if !strings.HasPrefix(r.Header.Get("Authorization"), "AWS4-HMAC-SHA256 Credential=access/") {
		w.WriteHeader(http.StatusForbidden)
		return
	}
	key := strings.TrimPrefix(r.URL.Path, "/bucket/")
	switch {
	case r.Method == http.MethodPut:
		body, _ := io.ReadAll(r.Body)
		if sha256hex(body) != r.Header.Get("x-amz-content-sha256") {
			w.WriteHeader(http.StatusBadRequest)
			return
		}
		b.objects[key] = body
		b.puts++
	case r.Method == http.MethodDelete:
		delete(b.objects, key)
		w.WriteHeader(http.StatusNoContent)
	case r.Method == http.MethodGet && r.URL.Query().Get("list-type") == "2":
		prefix := r.URL.Query().Get("prefix")
		type content struct {
			Key  string `xml:"Key"`
			Size int64  `xml:"Size"`
		}
		var listing struct {
			XMLName  xml.Name  `xml:"ListBucketResult"`
			Contents []content `xml:"Contents"`
		}
		keys := make([]string, 0, len(b.objects))
		for candidate := range b.objects {
			if strings.HasPrefix(candidate, prefix) {
				keys = append(keys, candidate)
			}
		}
		sort.Strings(keys)
		for _, candidate := range keys {
			listing.Contents = append(listing.Contents, content{Key: candidate, Size: int64(len(b.objects[candidate]))})
		}
		_ = xml.NewEncoder(w).Encode(listing)
	case r.Method == http.MethodGet:
		data, ok := b.objects[key]
		if !ok {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		_, _ = w.Write(data)
	default:
		w.WriteHeader(http.StatusMethodNotAllowed)
	}
}

func testConfig(server *httptest.Server) Config {
	return Config{Endpoint: server.URL, Bucket: "bucket", AccessKey: "access", SecretKey: "secret", Prefix: "spin", SegmentBytes: 256 << 10}
}

func openReplicated(t *testing.T, config Config, domain, path string) (*Replica, *persistence.SQLite) {
	t.Helper()
	replica, err := New(config, domain, path, vfs.Find(""), slog.New(slog.NewTextHandler(io.Discard, nil)))
	if err != nil {
		t.Fatal(err)
	}
	if err := replica.Prepare(context.Background()); err != nil {
		t.Fatal(err)
	}
	database, err := persistence.Open(path, persistence.OpenOptions{VFS: replica.VFSName(), FSPath: path})
	if err != nil {
		t.Fatal(err)
	}
	replica.Attach(database)
	return replica, database
}

// Pages written to the database reach the bucket as segments; a fresh
// server without the file rebuilds it from them and reads the same data.
func TestReplicaShipsPagesAndRestoresTheDatabase(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	dir := t.TempDir()

	source, database := openReplicated(t, config, "one.example.test", dir+"/one.db")
	if err := database.WriteFile("state", []byte(`{"version":1}`)); err != nil {
		t.Fatal(err)
	}
	large := bytes.Repeat([]byte("page-data-"), 60000) // ~600 KiB: several segments of 256 KiB
	if err := database.WriteFile("large", large); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	status := source.Status()
	if !status.Complete || status.Generation == "" || status.PendingPages != 0 || bucket.puts < 3 {
		t.Fatalf("status after first sync = %+v, puts = %d", status, bucket.puts)
	}
	if err := database.WriteFile("state", []byte(`{"version":2}`)); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}

	source.Close()

	// A new server without the file: restored from the bucket.
	if err := os.MkdirAll(dir+"/restored", 0o755); err != nil {
		t.Fatal(err)
	}
	restoredReplica, restored := openReplicated(t, config, "one.example.test", dir+"/restored/one.db")
	if !restoredReplica.Status().Restored {
		t.Fatalf("status = %+v; the database was not restored", restoredReplica.Status())
	}
	state, err := restored.ReadFile("state")
	if err != nil || string(state) != `{"version":2}` {
		t.Fatalf("restored state = %q, %v", state, err)
	}
	copied, err := restored.ReadFile("large")
	if err != nil || !bytes.Equal(copied, large) {
		t.Fatalf("restored large file: %d bytes, %v", len(copied), err)
	}
	// It continues the generation it was restored from.
	if err := restored.WriteFile("state", []byte(`{"version":3}`)); err != nil {
		t.Fatal(err)
	}
	if err := restoredReplica.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if restoredReplica.Status().Generation != status.Generation {
		t.Fatalf("restored server started a new generation: %+v", restoredReplica.Status())
	}
	// A write that never synced before a stop: the next start knows and
	// begins a new generation, which again holds everything.
	if err := restored.WriteFile("state", []byte(`{"version":4}`)); err != nil {
		t.Fatal(err)
	}
	if err := restored.Close(); err != nil {
		t.Fatal(err)
	}
	restoredReplica.Close()
	again, againDB := openReplicated(t, config, "one.example.test", dir+"/restored/one.db")
	if err := again.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if again.Status().Generation == status.Generation || !again.Status().Complete {
		t.Fatalf("unclean start did not begin a new generation: %+v", again.Status())
	}
	_ = againDB.Close()
	again.Close()
	current := string(bucket.objects["spin/one.example.test/current"])
	if current != again.Status().Generation {
		t.Fatalf("current = %q, want %q", current, again.Status().Generation)
	}
	for key := range bucket.objects {
		if strings.Contains(key, status.Generation) {
			t.Fatalf("old generation %s still in the bucket: %s", status.Generation, key)
		}
	}
}

func TestSegmentRoundTrip(t *testing.T) {
	seg := segment{PageSize: 512, DBSize: 2048, Pages: []uint32{1, 4}, Data: [][]byte{bytes.Repeat([]byte{1}, 512), bytes.Repeat([]byte{4}, 512)}}
	decoded, err := decodeSegment(encodeSegment(seg))
	if err != nil {
		t.Fatal(err)
	}
	if decoded.DBSize != 2048 || len(decoded.Pages) != 2 || decoded.Pages[1] != 4 || !bytes.Equal(decoded.Data[1], seg.Data[1]) {
		t.Fatalf("decoded = %+v", decoded)
	}
	damaged := encodeSegment(seg)
	damaged[30] ^= 0xff
	if _, err := decodeSegment(damaged); err == nil {
		t.Fatal("a damaged segment decoded")
	}
}
