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
	"time"

	"database/sql"
	_ "github.com/ncruces/go-sqlite3/driver"
	"net/url"

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

func openReplicated(t *testing.T, config Config, domain, path string) (*Replica, *testDatabase) {
	t.Helper()
	replica, err := New(config, domain, path, vfs.Find(""), slog.New(slog.NewTextHandler(io.Discard, nil)))
	if err != nil {
		t.Fatal(err)
	}
	if err := replica.Prepare(context.Background()); err != nil {
		t.Fatal(err)
	}
	database, err := openTestDatabase(path, replica.VFSName())
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
	// A write that never synced before a stop: the next start reads the
	// dirty log and continues the generation with the pages it names.
	if err := restored.WriteFile("state", []byte(`{"version":4}`)); err != nil {
		t.Fatal(err)
	}
	if err := restored.Close(); err != nil {
		t.Fatal(err)
	}
	restoredReplica.Close()
	continued, continuedDB := openReplicated(t, config, "one.example.test", dir+"/restored/one.db")
	if continued.Status().Generation != status.Generation || continued.Status().PendingPages == 0 || continued.Status().PendingPages > 8 {
		t.Fatalf("unclean start did not continue the generation with the difference: %+v", continued.Status())
	}
	if err := continued.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if continued.Status().Generation != status.Generation || continued.Status().PendingPages != 0 {
		t.Fatalf("status after continuing = %+v", continued.Status())
	}
	if err := continuedDB.Close(); err != nil {
		t.Fatal(err)
	}
	continued.Close()
	if err := os.MkdirAll(dir+"/check", 0o755); err != nil {
		t.Fatal(err)
	}
	checkReplica, check := openReplicated(t, config, "one.example.test", dir+"/check/one.db")
	if state, err := check.ReadFile("state"); err != nil || string(state) != `{"version":4}` {
		t.Fatalf("state restored after the continued generation = %q, %v", state, err)
	}
	_ = check.Close()
	checkReplica.Close()
	// Without a dirty log the start cannot say what changed: a new
	// generation holds everything again.
	clean, cleanDB := openReplicated(t, config, "one.example.test", dir+"/restored/one.db")
	if clean.Status().Generation != status.Generation {
		t.Fatalf("clean start did not continue the generation: %+v", clean.Status())
	}
	if err := cleanDB.WriteFile("state", []byte(`{"version":5}`)); err != nil {
		t.Fatal(err)
	}
	if err := cleanDB.Close(); err != nil {
		t.Fatal(err)
	}
	clean.Close()
	for _, candidate := range newDirtyLog(OSStorage(), dir+"/restored/one.db").paths {
		if err := os.Remove(candidate); err != nil && !os.IsNotExist(err) {
			t.Fatal(err)
		}
	}
	again, againDB := openReplicated(t, config, "one.example.test", dir+"/restored/one.db")
	if err := again.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if again.Status().Generation == status.Generation || !again.Status().Complete {
		t.Fatalf("start without a dirty log did not begin a new generation: %+v", again.Status())
	}
	_ = againDB.Close()
	again.Close()
	current := string(bucket.objects["spin/one.example.test/current"])
	if current != again.Status().Generation {
		t.Fatalf("current = %q, want %q", current, again.Status().Generation)
	}
	// The old generation stays within the retention: its snapshot is a
	// restore point, fetched into another file and read.
	points, err := again.Points(context.Background())
	if err != nil || len(points) < 2 {
		keys := []string{}
		for key := range bucket.objects {
			keys = append(keys, key)
		}
		sort.Strings(keys)
		t.Fatalf("points = %+v, %v; bucket = %v", points, err, keys)
	}
	var oldest Point
	for _, point := range points {
		if point.Generation == status.Generation {
			oldest = point
		}
	}
	if oldest.Generation == "" || points[0].Generation != again.Status().Generation || !points[0].Current {
		t.Fatalf("points = %+v", points)
	}
	if err := again.Fetch(context.Background(), oldest.Generation, oldest.At, dir+"/point.db"); err != nil {
		t.Fatal(err)
	}
	point, err := openTestDatabase(dir+"/point.db", "")
	if err != nil {
		t.Fatal(err)
	}
	defer point.Close()
	if state, err := point.ReadFile("state"); err != nil || string(state) != `{"version":1}` {
		t.Fatalf("fetched point in time = %q, %v", state, err)
	}
	// Outside the retention it goes.
	again.config.Retention = time.Nanosecond
	again.now = func() time.Time { return time.Now().UTC().Add(time.Hour) }
	again.pruneGenerations(again.Status().Generation)
	for key := range bucket.objects {
		if strings.Contains(key, status.Generation) {
			t.Fatalf("expired generation %s still in the bucket: %s", status.Generation, key)
		}
	}
}

// Restore points thin out with age: raw segments merge into quarter-hour
// windows, those into hours; each window's end is a point that restores
// exactly the state of that moment, and covered files go once their keep
// has passed.
func TestReplicaMergesWindowsIntoTieredRestorePoints(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	config.Schedule = []Level{{Window: 15 * time.Minute, Keep: 2 * time.Hour}, {Window: time.Hour, Keep: 24 * time.Hour}}
	dir := t.TempDir()
	rep, database := openReplicated(t, config, "tiers.example.test", dir+"/tiers.db")
	defer rep.Close()
	defer database.Close()
	clock := time.Date(2026, 9, 9, 8, 0, 30, 0, time.UTC)
	rep.now = func() time.Time { return clock }
	write := func(value string) {
		if err := database.WriteFile("state", []byte(value)); err != nil {
			t.Fatal(err)
		}
		if err := rep.Sync(context.Background()); err != nil {
			t.Fatal(err)
		}
	}
	write(`{"minute":0}`)
	if !rep.Status().Complete {
		t.Fatalf("status = %+v", rep.Status())
	}
	// Two syncs in the first quarter, one in the second, one in the fifth.
	clock = clock.Add(5 * time.Minute)
	write(`{"minute":5}`)
	clock = clock.Add(5 * time.Minute)
	write(`{"minute":10}`)
	clock = clock.Add(10 * time.Minute)
	write(`{"minute":20}`)
	clock = clock.Add(50 * time.Minute)
	write(`{"minute":70}`)
	// Everything up to 09:00 is more than an hour old: quarter windows for
	// 08:00 and 08:15 exist, the hour window 08:00-09:00 too.
	clock = clock.Add(20 * time.Minute)
	rep.lastCompact = time.Time{}
	if err := rep.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	lay, err := rep.loadLayout(context.Background(), rep.Status().Generation)
	if err != nil {
		t.Fatal(err)
	}
	if len(lay.windows[1]) < 2 || len(lay.windows[2]) != 1 {
		t.Fatalf("windows = L1:%d L2:%d", len(lay.windows[1]), len(lay.windows[2]))
	}
	// The end of the first quarter restores the state as of minute 10; the
	// end of the hour restores minute 20.
	quarter := time.Date(2026, 9, 9, 8, 15, 0, 0, time.UTC)
	if err := rep.Fetch(context.Background(), rep.Status().Generation, quarter, dir+"/quarter.db"); err != nil {
		t.Fatal(err)
	}
	quarterDB, err := openTestDatabase(dir+"/quarter.db", "")
	if err != nil {
		t.Fatal(err)
	}
	defer quarterDB.Close()
	if state, _ := quarterDB.ReadFile("state"); string(state) != `{"minute":10}` {
		t.Fatalf("state at the end of the first quarter = %s", state)
	}
	hour := time.Date(2026, 9, 9, 9, 0, 0, 0, time.UTC)
	if err := rep.Fetch(context.Background(), rep.Status().Generation, hour, dir+"/hour.db"); err != nil {
		t.Fatal(err)
	}
	hourDB, err := openTestDatabase(dir+"/hour.db", "")
	if err != nil {
		t.Fatal(err)
	}
	defer hourDB.Close()
	if state, _ := hourDB.ReadFile("state"); string(state) != `{"minute":20}` {
		t.Fatalf("state at the end of the hour = %s", state)
	}
	// Raw segments older than a quarter and covered by a window are gone;
	// the quarter windows stay (their keep is two hours); latest is intact.
	for _, raw := range lay.raw {
		if raw.at.Before(clock.Add(-15 * time.Minute)) {
			t.Fatalf("raw segment %s still there", raw.key)
		}
	}
	points, err := rep.Points(context.Background())
	if err != nil || len(points) < 4 || points[0].At.Before(points[1].At) {
		t.Fatalf("points = %+v, %v", points, err)
	}
	if err := rep.Fetch(context.Background(), rep.Status().Generation, time.Time{}, dir+"/latest.db"); err != nil {
		t.Fatal(err)
	}
	latest, err := openTestDatabase(dir+"/latest.db", "")
	if err != nil {
		t.Fatal(err)
	}
	defer latest.Close()
	if state, _ := latest.ReadFile("state"); string(state) != `{"minute":70}` {
		t.Fatalf("latest state = %s", state)
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

// testDatabase deliberately has no dependency on Spin's schema or persistence.
type testDatabase struct{ db *sql.DB }

func openTestDatabase(path, vfsName string) (*testDatabase, error) {
	u := url.URL{Scheme: "file", Path: path}
	q := url.Values{}
	if vfsName != "" {
		q.Set("vfs", vfsName)
	}
	u.RawQuery = q.Encode()
	db, err := sql.Open("sqlite3", u.String())
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)
	if _, err = db.Exec(`PRAGMA journal_mode=DELETE; CREATE TABLE IF NOT EXISTS spin_kv(key TEXT PRIMARY KEY, value BLOB)`); err != nil {
		db.Close()
		return nil, err
	}
	return &testDatabase{db: db}, nil
}
func (d *testDatabase) WithReadTransaction(ctx context.Context, fn func() error) error {
	tx, err := d.db.BeginTx(ctx, &sql.TxOptions{ReadOnly: true})
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if _, err := tx.ExecContext(ctx, `SELECT count(*) FROM sqlite_schema`); err != nil {
		return err
	}
	return fn()
}
func (d *testDatabase) ReadFile(key string) ([]byte, error) {
	var data []byte
	err := d.db.QueryRow(`SELECT value FROM spin_kv WHERE key=?`, key).Scan(&data)
	return data, err
}
func (d *testDatabase) WriteFile(key string, data []byte) error {
	_, err := d.db.Exec(`INSERT INTO spin_kv(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value`, key, data)
	return err
}
func (d *testDatabase) Close() error { return d.db.Close() }
