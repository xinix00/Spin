package tenancy

import (
	"bytes"
	"context"
	"encoding/json"
	"encoding/xml"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"easyacp/internal/replicaconfig"
	spinserver "easyacp/internal/server"
)

// Exercise the actual environment -> tenant -> persistence -> replica -> server
// wiring. The HTTP endpoint is the only fake; SQLite and Spin's schema are real.
func TestTenantReplicationRestoresFromEmptyDataDirectory(t *testing.T) {
	ctx := context.Background()
	var mu sync.Mutex
	objects := map[string][]byte{}
	bucket := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		defer mu.Unlock()
		key := strings.TrimPrefix(r.URL.Path, "/bucket/")
		switch {
		case r.Method == http.MethodGet && r.URL.Query().Get("list-type") == "2":
			type item struct {
				Key  string
				Size int
			}
			listing := struct {
				XMLName  xml.Name `xml:"ListBucketResult"`
				Contents []item   `xml:"Contents"`
			}{}
			for key, data := range objects {
				if strings.HasPrefix(key, r.URL.Query().Get("prefix")) {
					listing.Contents = append(listing.Contents, item{key, len(data)})
				}
			}
			if err := xml.NewEncoder(w).Encode(listing); err != nil {
				t.Error(err)
			}
		case r.Method == http.MethodPut:
			data, err := io.ReadAll(r.Body)
			if err != nil {
				t.Error(err)
				w.WriteHeader(500)
				return
			}
			objects[key] = data
		case r.Method == http.MethodDelete:
			delete(objects, key)
			w.WriteHeader(204)
		case r.Method == http.MethodGet:
			data, ok := objects[key]
			if !ok {
				w.WriteHeader(404)
				return
			}
			w.Write(data)
		default:
			w.WriteHeader(405)
		}
	}))
	defer bucket.Close()
	values := map[string]string{"SPIN_S3_ENDPOINT": bucket.URL, "SPIN_S3_BUCKET": "bucket", "SPIN_S3_ACCESS_KEY": "access", "SPIN_S3_SECRET_KEY": "secret"}
	replication, enabled, err := replicaconfig.FromEnvironment(func(key string) string { return values[key] })
	if err != nil || !enabled {
		t.Fatalf("configuration: %v", err)
	}
	replication.Interval = time.Hour // Drive the same attached replica deterministically.
	options := func(string) spinserver.ServerOptions { return spinserver.ServerOptions{DisableAuthentication: true} }
	first := New(Config{DataDir: t.TempDir(), Replication: &replication, Options: options})
	defer first.Close()
	domains, err := first.Discover(ctx)
	if err != nil || len(domains) != 0 {
		t.Fatalf("fresh bucket unexpectedly discovered tenants: %v %v", domains, err)
	}
	tenant, err := first.Open(ctx, "wired.test")
	if err != nil {
		t.Fatal(err)
	}
	if tenant.Replica == nil {
		t.Fatal("tenant has no replica")
	}
	token, err := tenant.Store.RotateWorkerToken()
	if err != nil {
		t.Fatal(err)
	}
	if err := tenant.Database.WriteFile("wiring-proof", []byte("persistent")); err != nil {
		t.Fatal(err)
	}
	if err := tenant.Replica.Sync(ctx); err != nil {
		t.Fatal(err)
	}
	// A backup import uses SQLite's online restore API. Check that it also
	// passes through the tenant's tracking VFS and reaches the remote replica.
	var backup bytes.Buffer
	if err := tenant.Database.WriteBackup(ctx, &backup, "test-backup-key"); err != nil {
		t.Fatal(err)
	}
	if err := tenant.Database.WriteFile("wiring-proof", []byte("before import")); err != nil {
		t.Fatal(err)
	}
	if err := tenant.Replica.Sync(ctx); err != nil {
		t.Fatal(err)
	}
	staged, err := tenant.Database.StageBackup(ctx, bytes.NewReader(backup.Bytes()), int64(backup.Len()))
	if err != nil {
		t.Fatal(err)
	}
	defer staged.Close()
	if err := tenant.Database.RestoreFrom(ctx, staged); err != nil {
		t.Fatal(err)
	}
	if tenant.Replica.Status().PendingPages == 0 {
		t.Fatal("backup import bypassed replication tracking")
	}
	if err := tenant.Replica.Sync(ctx); err != nil {
		t.Fatal(err)
	}
	if !tenant.Replica.Status().Complete {
		t.Fatal("attached database never produced a complete snapshot")
	}
	if err := first.Close(); err != nil {
		t.Fatal(err)
	}
	second := New(Config{DataDir: t.TempDir(), Replication: &replication, Options: options})
	defer second.Close()
	domains, err = second.Discover(ctx)
	if err != nil || len(domains) != 1 || domains[0] != "wired.test" {
		t.Fatalf("discovery: %v %v", domains, err)
	}
	restored, err := second.Open(ctx, "wired.test")
	if err != nil {
		t.Fatal(err)
	}
	if !restored.Replica.Status().Restored || restored.Store.WorkerToken() != token {
		t.Fatal("tenant did not recover its Spin state")
	}
	data, err := restored.Database.ReadFile("wiring-proof")
	if err != nil || string(data) != "persistent" {
		t.Fatalf("restored file: %q %v", data, err)
	}
	request := httptest.NewRequest(http.MethodGet, "http://wired.test/healthz", nil)
	response := httptest.NewRecorder()
	second.ServeHTTP(response, request)
	var health map[string]any
	if response.Code != 200 || json.Unmarshal(response.Body.Bytes(), &health) != nil || !strings.Contains(response.Body.String(), `"restored":true`) {
		t.Fatalf("server does not expose the wired replica: %d %s", response.Code, response.Body.String())
	}

	request = httptest.NewRequest(http.MethodGet, "http://wired.test/api/replica/points", nil)
	response = httptest.NewRecorder()
	second.ServeHTTP(response, request)
	var points struct {
		Points []struct {
			Generation string `json:"generation"`
		} `json:"points"`
	}
	if response.Code != 200 || json.Unmarshal(response.Body.Bytes(), &points) != nil || len(points.Points) == 0 || points.Points[0].Generation != restored.Replica.Status().Generation {
		t.Fatalf("restore UI API is not connected: %d %s", response.Code, response.Body.String())
	}
}
