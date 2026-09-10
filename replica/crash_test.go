package replica

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

// The crash suite: a real process crash that leaves a hot rollback journal,
// a stale or damaged page index, two domains in one bucket, a restart that
// must ship nothing, and a restored database whose index Prepare rebuilt.
// Helpers are prefixed crash; the child of the process-crash test is
// TestCrashChild, selected by REPLICA_CRASH_CHILD.

const (
	crashChildEnv      = "REPLICA_CRASH_CHILD"
	crashDBEnv         = "REPLICA_CRASH_DB"
	crashEndpointEnv   = "REPLICA_CRASH_ENDPOINT"
	crashDomainEnv     = "REPLICA_CRASH_DOMAIN"
	crashGenerationEnv = "REPLICA_CRASH_GENERATION"
)

// crashSession is one open replica with its database, closed without a
// sync: after unsynced writes that is an unclean stop.
type crashSession struct {
	rep *Replica
	db  *testDatabase
}

func crashSessionOf(t *testing.T, rep *Replica, db *testDatabase) *crashSession {
	s := &crashSession{rep: rep, db: db}
	t.Cleanup(s.close)
	return s
}

// close is idempotent, so a test closes explicitly before it reopens the
// domain and the cleanup is a safety net.
func (s *crashSession) close() {
	_ = s.db.Close()
	s.rep.Close()
}

func crashOpen(t *testing.T, config Config, domain, path string) *crashSession {
	t.Helper()
	rep, db := openReplicated(t, config, domain, path)
	return crashSessionOf(t, rep, db)
}

func crashOpenWith(t *testing.T, config Config, domain, path string, options Options) *crashSession {
	t.Helper()
	rep, err := NewWithOptions(config, domain, path, vfs.Find(""), nil, options)
	if err != nil {
		t.Fatal(err)
	}
	if err := rep.Prepare(context.Background()); err != nil {
		rep.Close()
		t.Fatal(err)
	}
	db, err := openTestDatabase(path, rep.VFSName())
	if err != nil {
		rep.Close()
		t.Fatal(err)
	}
	rep.Attach(db)
	return crashSessionOf(t, rep, db)
}

func crashBucket(t *testing.T) (*fakeBucket, *httptest.Server, Config) {
	t.Helper()
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	t.Cleanup(server.Close)
	return bucket, server, testConfig(server)
}

func crashPuts(bucket *fakeBucket) int {
	bucket.mu.Lock()
	defer bucket.mu.Unlock()
	return bucket.puts
}

func crashCurrent(bucket *fakeBucket, config Config, domain string) string {
	bucket.mu.Lock()
	defer bucket.mu.Unlock()
	return string(bucket.objects[config.Prefix+"/"+domain+"/current"])
}

func crashBlob(fill byte, size int) []byte { return bytes.Repeat([]byte{fill}, size) }

func crashWrite(t *testing.T, s *crashSession, key string, value []byte) {
	t.Helper()
	if err := s.db.WriteFile(key, value); err != nil {
		t.Fatalf("write %s: %v", key, err)
	}
}

func crashSync(t *testing.T, s *crashSession) {
	t.Helper()
	if err := s.rep.Sync(context.Background()); err != nil {
		t.Fatalf("sync: %v", err)
	}
}

func crashRows(t *testing.T, db *testDatabase) map[string][]byte {
	t.Helper()
	rows, err := db.db.Query(`SELECT key, value FROM spin_kv ORDER BY key`)
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	out := map[string][]byte{}
	for rows.Next() {
		var key string
		var value []byte
		if err := rows.Scan(&key, &value); err != nil {
			t.Fatal(err)
		}
		out[key] = value
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	return out
}

func crashIntegrity(t *testing.T, db *testDatabase) {
	t.Helper()
	var result string
	if err := db.db.QueryRow(`PRAGMA integrity_check`).Scan(&result); err != nil || result != "ok" {
		t.Fatalf("integrity_check = %q, %v", result, err)
	}
}

// crashEqual compares rows by key and content; sizes, not contents, are
// reported.
func crashEqual(t *testing.T, what string, got, want map[string][]byte) {
	t.Helper()
	keys := map[string]bool{}
	for key := range got {
		keys[key] = true
	}
	for key := range want {
		keys[key] = true
	}
	sorted := make([]string, 0, len(keys))
	for key := range keys {
		sorted = append(sorted, key)
	}
	sort.Strings(sorted)
	mismatches := 0
	for _, key := range sorted {
		g, gotIt := got[key]
		w, wanted := want[key]
		switch {
		case !wanted:
			t.Errorf("%s: unexpected row %q (%d bytes)", what, key, len(g))
		case !gotIt:
			t.Errorf("%s: row %q (%d bytes) is missing", what, key, len(w))
		case !bytes.Equal(g, w):
			t.Errorf("%s: row %q differs: %d bytes starting %q, want %d bytes starting %q", what, key, len(g), g[:min(8, len(g))], len(w), w[:min(8, len(w))])
		default:
			continue
		}
		mismatches++
	}
	if mismatches > 0 {
		t.Fatalf("%s: %d of %d rows differ", what, mismatches, len(sorted))
	}
}

// crashRestore restores the domain into a fresh path, checks the SQLite
// integrity, and returns its rows; the replica is closed before returning
// so the domain can be opened again.
func crashRestore(t *testing.T, config Config, domain, path string) map[string][]byte {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	s := crashOpen(t, config, domain, path)
	defer s.close()
	if status := s.rep.Status(); !status.Restored || !status.Complete {
		t.Fatalf("no restore into %s: %+v", path, status)
	}
	crashIntegrity(t, s.db)
	return crashRows(t, s.db)
}

func crashMarker(t *testing.T, path string) marker {
	t.Helper()
	data, err := os.ReadFile(path + ".replica")
	if err != nil {
		t.Fatal(err)
	}
	var m marker
	if err := json.Unmarshal(data, &m); err != nil {
		t.Fatal(err)
	}
	return m
}

func crashFileSize(t *testing.T, path string) int64 {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Size()
}

func crashMerge(maps ...map[string][]byte) map[string][]byte {
	out := map[string][]byte{}
	for _, m := range maps {
		for key, value := range m {
			out[key] = value
		}
	}
	return out
}

// crashChildRows is what the crashing child commits: one batch it syncs,
// one it commits but never syncs (several hundred KiB).
func crashChildRows() (synced, unsynced map[string][]byte) {
	synced = map[string][]byte{"child-synced": crashBlob('s', 300<<10)}
	unsynced = map[string][]byte{
		"child-unsynced-1": crashBlob('1', 200<<10),
		"child-unsynced-2": crashBlob('2', 200<<10),
		"child-unsynced-3": crashBlob('3', 200<<10),
	}
	return synced, unsynced
}

// crashReport is what the child leaves next to the database just before it
// dies, so the parent can prove the uncommitted row reached the disk.
type crashReport struct {
	Generation    string `json:"generation"`
	CommittedSize int64  `json:"committed_size"`
	SpilledSize   int64  `json:"spilled_size"`
	JournalSize   int64  `json:"journal_size"`
}

// TestCrashChild is the process that crashes: it only runs when the parent
// starts it with REPLICA_CRASH_CHILD set.
func TestCrashChild(t *testing.T) {
	if os.Getenv(crashChildEnv) == "" {
		t.Skip("helper process of TestCrashMidTransactionLeavesHotJournalAndTheGenerationContinues")
	}
	crashChild()
}

func crashChild() {
	fail := func(format string, args ...any) {
		fmt.Fprintf(os.Stderr, "crash child: %s\n", fmt.Sprintf(format, args...))
		os.Exit(1)
	}
	ctx := context.Background()
	path, endpoint, domain, generation := os.Getenv(crashDBEnv), os.Getenv(crashEndpointEnv), os.Getenv(crashDomainEnv), os.Getenv(crashGenerationEnv)
	if path == "" || endpoint == "" || domain == "" || generation == "" {
		fail("missing environment: db=%q endpoint=%q domain=%q generation=%q", path, endpoint, domain, generation)
	}
	config := testConfig(&httptest.Server{URL: endpoint})
	rep, err := New(config, domain, path, vfs.Find(""), slog.New(slog.NewTextHandler(os.Stderr, nil)))
	if err != nil {
		fail("new: %v", err)
	}
	if err := rep.Prepare(ctx); err != nil {
		fail("prepare: %v", err)
	}
	db, err := openTestDatabase(path, rep.VFSName())
	if err != nil {
		fail("open: %v", err)
	}
	rep.Attach(db)
	if status := rep.Status(); status.Generation != generation || status.PendingPages != 0 {
		fail("clean start did not continue generation %s: %+v", generation, status)
	}
	synced, unsynced := crashChildRows()
	for key, value := range synced {
		if err := db.WriteFile(key, value); err != nil {
			fail("write %s: %v", key, err)
		}
	}
	if err := rep.Sync(ctx); err != nil {
		fail("sync: %v", err)
	}
	if status := rep.Status(); status.Generation != generation || status.PendingPages != 0 {
		fail("sync moved to another generation or left pages: %+v", status)
	}
	for key, value := range unsynced {
		if err := db.WriteFile(key, value); err != nil {
			fail("write %s: %v", key, err)
		}
	}
	committed, err := os.Stat(path)
	if err != nil {
		fail("stat: %v", err)
	}
	// A small page cache makes SQLite spill the uncommitted row into the
	// database file before the commit, so the crash leaves uncommitted
	// pages on disk and a hot journal that undoes them.
	if _, err := db.db.Exec(`PRAGMA cache_size=-256`); err != nil {
		fail("cache_size: %v", err)
	}
	tx, err := db.db.BeginTx(ctx, nil)
	if err != nil {
		fail("begin: %v", err)
	}
	if _, err := tx.ExecContext(ctx, `INSERT INTO spin_kv(key,value) VALUES(?,?)`, "child-uncommitted", crashBlob('u', 1<<20)); err != nil {
		fail("insert: %v", err)
	}
	journal, err := os.Stat(path + "-journal")
	if err != nil {
		fail("no rollback journal while the transaction is open: %v", err)
	}
	spilled, err := os.Stat(path)
	if err != nil {
		fail("stat: %v", err)
	}
	report := crashReport{Generation: rep.Status().Generation, CommittedSize: committed.Size(), SpilledSize: spilled.Size(), JournalSize: journal.Size()}
	data, _ := json.Marshal(report)
	if err := os.WriteFile(path+".crash-report", data, 0o600); err != nil {
		fail("report: %v", err)
	}
	fmt.Fprintf(os.Stderr, "crash child: exiting with the transaction open: %+v\n", report)
	os.Exit(3)
}

// A process dies in the middle of a write transaction, after uncommitted
// pages spilled into the database file and after committed writes that
// never synced. The hot journal undoes the transaction at the next open,
// the page index lets the generation continue with the difference, and
// the restore holds exactly what was committed.
func TestCrashMidTransactionLeavesHotJournalAndTheGenerationContinues(t *testing.T) {
	bucket, server, config := crashBucket(t)
	dir := t.TempDir()
	path := dir + "/crash.db"
	domain := "crash.example.test"

	parent := crashOpen(t, config, domain, path)
	parentRows := map[string][]byte{"parent-1": crashBlob('p', 100<<10), "parent-2": []byte(`{"version":1}`)}
	for key, value := range parentRows {
		crashWrite(t, parent, key, value)
	}
	crashSync(t, parent)
	generation := parent.rep.Status().Generation
	if generation == "" || !parent.rep.Status().Complete {
		t.Fatalf("no generation after the first sync: %+v", parent.rep.Status())
	}
	if seq, _, _ := crashDirtyLog(t, path); seq != 1 {
		t.Fatalf("dirty log after the first sync is at sequence %d", seq)
	}
	parent.close()
	if current := crashCurrent(bucket, config, domain); current != generation {
		t.Fatalf("current = %q, want %q", current, generation)
	}

	child := exec.Command(os.Args[0], "-test.run=^TestCrashChild$", "-test.v")
	child.Env = append(os.Environ(),
		crashChildEnv+"=1",
		crashDBEnv+"="+path,
		crashEndpointEnv+"="+server.URL,
		crashDomainEnv+"="+domain,
		crashGenerationEnv+"="+generation,
	)
	output, err := child.CombinedOutput()
	var exit *exec.ExitError
	if !errors.As(err, &exit) || exit.ExitCode() != 3 {
		t.Fatalf("child exited with %v, want exit status 3\n%s", err, output)
	}
	t.Logf("child:\n%s", output)

	journal, err := os.Stat(path + "-journal")
	if err != nil {
		t.Fatalf("no hot journal after the crash: %v", err)
	}
	if journal.Size() == 0 {
		t.Fatal("the hot journal is empty")
	}
	var report crashReport
	if data, err := os.ReadFile(path + ".crash-report"); err != nil {
		t.Fatal(err)
	} else if err := json.Unmarshal(data, &report); err != nil {
		t.Fatal(err)
	}
	if report.Generation != generation {
		t.Fatalf("the child ran generation %q, want %q", report.Generation, generation)
	}
	if report.SpilledSize <= report.CommittedSize {
		t.Fatalf("the uncommitted row never reached the database file (%d bytes before, %d at the crash); the crash proves nothing", report.CommittedSize, report.SpilledSize)
	}
	if size := crashFileSize(t, path); size != report.SpilledSize {
		t.Fatalf("database is %d bytes after the crash, the child saw %d", size, report.SpilledSize)
	}
	if current := crashCurrent(bucket, config, domain); current != generation {
		t.Fatalf("current = %q after the child, want %q", current, generation)
	}
	if m := crashMarker(t, path); m.Clean || m.Generation != generation {
		t.Fatalf("marker after the crash = %+v", m)
	}

	// The restart: Prepare compares the file (with the spilled pages) to
	// the index; the open rolls the hot journal back through the tracking
	// VFS; both leave dirty pages that the next sync ships.
	again := crashOpen(t, config, domain, path)
	if _, err := os.Stat(path + "-journal"); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("hot journal not rolled back at the open: %v", err)
	}
	status := again.rep.Status()
	if status.Generation != generation {
		t.Fatalf("the generation did not continue after the crash: %+v", status)
	}
	if status.PendingPages == 0 {
		t.Fatalf("no pending pages after the crash: %+v", status)
	}
	synced, unsynced := crashChildRows()
	want := crashMerge(parentRows, synced, unsynced)
	crashIntegrity(t, again.db)
	crashEqual(t, "database after the rollback", crashRows(t, again.db), want)
	crashSync(t, again)
	status = again.rep.Status()
	if status.Generation != generation || status.PendingPages != 0 || status.LastError != "" || !status.Complete {
		t.Fatalf("status after the sync = %+v", status)
	}
	again.close()

	got := crashRestore(t, config, domain, dir+"/restored/crash.db")
	if _, leaked := got["child-uncommitted"]; leaked {
		t.Fatal("the restore holds the row of the transaction that never committed")
	}
	crashEqual(t, "restore after the crash", got, want)
}

// crashDirtyLog reads the newest dirty log of the marker's generation next
// to a database.
func crashDirtyLog(t *testing.T, path string) (int64, []uint32, string) {
	t.Helper()
	marker := crashMarker(t, path)
	log := newDirtyLog(OSStorage(), path)
	bestSeq, bestPages, bestPath := int64(-1), []uint32(nil), ""
	for _, candidate := range log.paths {
		if _, err := os.Stat(candidate); err != nil {
			continue
		}
		generation, seq, pages, err := log.readFile(candidate)
		if err != nil {
			t.Fatalf("dirty log %s: %v", candidate, err)
		}
		if generation == marker.Generation && seq > bestSeq {
			bestSeq, bestPages, bestPath = seq, pages, candidate
		}
	}
	if bestPath == "" {
		t.Fatalf("no dirty log of generation %q next to %s", marker.Generation, path)
	}
	return bestSeq, bestPages, bestPath
}

// The rewrite of the dirty log after a committed sync fails: the previous
// log stays, names the pages of that sync and everything after, and an
// unclean start continues the generation with those, a few pages shipped
// twice at most. A page that returned to its old bytes ships too.
func TestUncleanStopWithStaleDirtyLogShipsAgainWithoutHarm(t *testing.T) {
	for _, revert := range []bool{false, true} {
		name := "fresh-pages"
		if revert {
			name = "reverted-page"
		}
		t.Run(name, func(t *testing.T) {
			bucket, _, config := crashBucket(t)
			dir := t.TempDir()
			path := dir + "/stale.db"
			domain := "stale-" + name + ".example.test"
			var failRewrite atomic.Bool
			storage := faultStorage{Storage: OSStorage(), open: func(name string, create bool) error {
				if create && strings.Contains(name, ".replica-dirty-") && failRewrite.CompareAndSwap(true, false) {
					return errInjected
				}
				return nil
			}}
			s := crashOpenWith(t, config, domain, path, Options{Storage: storage})
			first, second := crashBlob('a', 64), crashBlob('b', 64)
			crashWrite(t, s, "filler", crashBlob('f', 100<<10))
			crashWrite(t, s, "state", first)
			crashSync(t, s)
			generation := s.rep.Status().Generation
			// The second sync commits, then the rewrite of the log fails.
			crashWrite(t, s, "state", second)
			if !revert {
				crashWrite(t, s, "second", crashBlob('2', 40<<10))
			}
			failRewrite.Store(true)
			crashSync(t, s)
			if failRewrite.Load() {
				t.Fatal("the sync never rewrote the dirty log")
			}
			if seq, pages, _ := crashDirtyLog(t, path); seq != 1 || len(pages) == 0 {
				t.Fatalf("after the failed rewrite the log is at sequence %d with %d pages; expected the previous log, at 1, naming the pages of sync 2", seq, len(pages))
			}
			if m := crashMarker(t, path); !m.Clean || m.Seq != 2 || m.Generation != generation {
				t.Fatalf("marker after the second sync = %+v", m)
			}
			// Writes that never sync, then an unclean stop. In the revert
			// case a page returns to its content of sync 1.
			if revert {
				crashWrite(t, s, "state", first)
			} else {
				crashWrite(t, s, "third", crashBlob('3', 50<<10))
			}
			want := crashRows(t, s.db)
			s.close()
			if m := crashMarker(t, path); m.Clean {
				t.Fatalf("stop after unsynced writes left a clean marker: %+v", m)
			}

			again := crashOpen(t, config, domain, path)
			status := again.rep.Status()
			if status.Generation != generation || status.PendingPages == 0 {
				t.Fatalf("the generation did not continue from the previous log: %+v", status)
			}
			t.Logf("the generation continues with %d pages", status.PendingPages)
			crashSync(t, again)
			status = again.rep.Status()
			if status.PendingPages != 0 || !status.Complete || status.Generation != generation {
				t.Fatalf("status after the sync = %+v", status)
			}
			if current := crashCurrent(bucket, config, domain); current != status.Generation {
				t.Fatalf("current = %q, want %q", current, status.Generation)
			}
			again.close()
			got := crashRestore(t, config, domain, dir+"/restored/stale.db")
			crashEqual(t, "restore after the stale dirty log", got, want)
		})
	}
}

// After an unclean stop a dirty log that is damaged, past the marker or
// missing cannot say which pages changed: a new generation starts, and it
// restores everything the database held. A log at an older sequence, the
// one a crash between the marker write and the rewrite leaves, continues.
func TestDirtyLogDamageStartsANewGenerationAndAnOlderLogContinues(t *testing.T) {
	cases := []struct {
		name      string
		continues bool
		damage    func(t *testing.T, path string)
	}{
		{"header-cut", false, func(t *testing.T, path string) {
			_, _, current := crashDirtyLog(t, path)
			if err := os.Truncate(current, 10); err != nil {
				t.Fatal(err)
			}
		}},
		{"flipped-record-byte", false, func(t *testing.T, path string) {
			_, _, current := crashDirtyLog(t, path)
			data, err := os.ReadFile(current)
			if err != nil {
				t.Fatal(err)
			}
			header := dirtyLogHeaderSize(crashMarker(t, path).Generation)
			if len(data) <= header {
				t.Fatalf("the log holds no records to damage: %d bytes", len(data))
			}
			data[header+2] ^= 0xff
			if err := os.WriteFile(current, data, 0o600); err != nil {
				t.Fatal(err)
			}
		}},
		{"flipped-header-byte", false, func(t *testing.T, path string) {
			_, _, current := crashDirtyLog(t, path)
			data, err := os.ReadFile(current)
			if err != nil {
				t.Fatal(err)
			}
			data[len(dirtyLogMagic)+7] ^= 0x01
			if err := os.WriteFile(current, data, 0o600); err != nil {
				t.Fatal(err)
			}
		}},
		{"past-the-marker", false, func(t *testing.T, path string) {
			_, pages, _ := crashDirtyLog(t, path)
			log := newDirtyLog(OSStorage(), path)
			if err := log.rewrite(crashMarker(t, path).Generation, 99, pages); err != nil {
				t.Fatal(err)
			}
			log.close()
		}},
		{"deleted", false, func(t *testing.T, path string) {
			for _, candidate := range newDirtyLog(OSStorage(), path).paths {
				if err := os.Remove(candidate); err != nil && !os.IsNotExist(err) {
					t.Fatal(err)
				}
			}
		}},
		{"only-the-older-log", true, func(t *testing.T, path string) {
			// The crash came between the marker write of sync 1 and the
			// rewrite: the older log names the pages of sync 1 and after.
			_, pages, current := crashDirtyLog(t, path)
			log := newDirtyLog(OSStorage(), path)
			if err := log.rewrite(crashMarker(t, path).Generation, 0, pages); err != nil {
				t.Fatal(err)
			}
			log.close()
			if err := os.Remove(current); err != nil {
				t.Fatal(err)
			}
		}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			bucket, _, config := crashBucket(t)
			dir := t.TempDir()
			path := dir + "/torn.db"
			domain := "torn-" + tc.name + ".example.test"
			s := crashOpen(t, config, domain, path)
			crashWrite(t, s, "filler", crashBlob('f', 150<<10))
			crashWrite(t, s, "state", []byte(`{"version":1}`))
			crashSync(t, s)
			generation := s.rep.Status().Generation
			if generation == "" {
				t.Fatalf("no generation: %+v", s.rep.Status())
			}
			crashWrite(t, s, "state", []byte(`{"version":2}`))
			crashWrite(t, s, "later", crashBlob('l', 30<<10))
			want := crashRows(t, s.db)
			s.close()
			if m := crashMarker(t, path); m.Clean || m.Generation != generation {
				t.Fatalf("marker after the unclean stop = %+v", m)
			}
			tc.damage(t, path)

			again := crashOpen(t, config, domain, path)
			status := again.rep.Status()
			if tc.continues {
				if status.Generation != generation || status.PendingPages == 0 {
					t.Fatalf("the older log did not continue the generation: %+v", status)
				}
			} else {
				if status.Generation == generation {
					t.Fatalf("the generation continued on a %s dirty log: %+v", tc.name, status)
				}
				if !again.rep.SnapshotDue() {
					t.Fatal("no snapshot due after the dirty log became unusable")
				}
			}
			crashSync(t, again)
			status = again.rep.Status()
			if status.Generation == "" || (status.Generation == generation) != tc.continues || !status.Complete || status.PendingPages != 0 {
				t.Fatalf("status after the sync = %+v, previous generation %s", status, generation)
			}
			if current := crashCurrent(bucket, config, domain); current != status.Generation {
				t.Fatalf("current = %q, want %q", current, status.Generation)
			}
			if seq, _, _ := crashDirtyLog(t, path); seq != crashMarker(t, path).Seq {
				t.Fatalf("the dirty log is at sequence %d, the marker at %d", seq, crashMarker(t, path).Seq)
			}
			again.close()
			got := crashRestore(t, config, domain, dir+"/restored/torn.db")
			crashEqual(t, "restore after the "+tc.name+" dirty log", got, want)
		})
	}
}

// Two domains share one bucket: interleaved writes, syncs and unclean
// restarts keep their generations apart, each restore holds only its own
// rows, and Domains lists both.
func TestTwoDomainsInOneBucketDoNotInterfere(t *testing.T) {
	bucket, _, config := crashBucket(t)
	dir := t.TempDir()
	alphaDomain, betaDomain := "alpha.example.test", "beta.example.test"
	alphaPath, betaPath := dir+"/alpha.db", dir+"/beta.db"

	alpha := crashOpen(t, config, alphaDomain, alphaPath)
	beta := crashOpen(t, config, betaDomain, betaPath)
	crashWrite(t, alpha, "alpha-1", crashBlob('a', 120<<10))
	crashWrite(t, beta, "beta-1", crashBlob('b', 80<<10))
	crashSync(t, alpha)
	crashWrite(t, beta, "beta-2", crashBlob('B', 60<<10))
	crashSync(t, beta)
	crashWrite(t, alpha, "alpha-2", crashBlob('A', 70<<10))
	crashSync(t, alpha)
	alphaGeneration, betaGeneration := alpha.rep.Status().Generation, beta.rep.Status().Generation
	if alphaGeneration == "" || betaGeneration == "" || alphaGeneration == betaGeneration {
		t.Fatalf("generations alpha=%q beta=%q", alphaGeneration, betaGeneration)
	}
	if crashCurrent(bucket, config, alphaDomain) != alphaGeneration || crashCurrent(bucket, config, betaDomain) != betaGeneration {
		t.Fatal("current generations are mixed up")
	}
	// Unsynced writes in both, then both stop uncleanly.
	crashWrite(t, alpha, "alpha-3", crashBlob('3', 20<<10))
	crashWrite(t, beta, "beta-3", crashBlob('4', 25<<10))
	wantAlpha, wantBeta := crashRows(t, alpha.db), crashRows(t, beta.db)
	alpha.close()
	beta.close()

	alpha = crashOpen(t, config, alphaDomain, alphaPath)
	beta = crashOpen(t, config, betaDomain, betaPath)
	if status := alpha.rep.Status(); status.Generation != alphaGeneration || status.PendingPages == 0 {
		t.Fatalf("alpha after the unclean start = %+v", status)
	}
	if status := beta.rep.Status(); status.Generation != betaGeneration || status.PendingPages == 0 {
		t.Fatalf("beta after the unclean start = %+v", status)
	}
	crashSync(t, beta)
	crashSync(t, alpha)
	if status := alpha.rep.Status(); status.Generation != alphaGeneration || status.PendingPages != 0 {
		t.Fatalf("alpha after the sync = %+v", status)
	}
	if status := beta.rep.Status(); status.Generation != betaGeneration || status.PendingPages != 0 {
		t.Fatalf("beta after the sync = %+v", status)
	}
	alpha.close()
	beta.close()

	// Every object lives under its own domain, and generation ids do not
	// cross domains.
	bucket.mu.Lock()
	for key := range bucket.objects {
		underAlpha := strings.HasPrefix(key, config.Prefix+"/"+alphaDomain+"/")
		underBeta := strings.HasPrefix(key, config.Prefix+"/"+betaDomain+"/")
		if underAlpha == underBeta || (underAlpha && strings.Contains(key, betaGeneration)) || (underBeta && strings.Contains(key, alphaGeneration)) {
			bucket.mu.Unlock()
			t.Fatalf("object %s belongs to no single domain", key)
		}
	}
	bucket.mu.Unlock()

	gotAlpha := crashRestore(t, config, alphaDomain, dir+"/restored-alpha/alpha.db")
	crashEqual(t, "alpha restore", gotAlpha, wantAlpha)
	gotBeta := crashRestore(t, config, betaDomain, dir+"/restored-beta/beta.db")
	crashEqual(t, "beta restore", gotBeta, wantBeta)

	domains, err := Domains(context.Background(), config)
	if err != nil {
		t.Fatal(err)
	}
	sort.Strings(domains)
	if len(domains) != 2 || domains[0] != alphaDomain || domains[1] != betaDomain {
		t.Fatalf("Domains = %v, want [%s %s]", domains, alphaDomain, betaDomain)
	}
}

// A clean stop and a restart without any write: the sync after it puts
// nothing in the bucket and the index stays as it was.
func TestRestartWhileNothingChangedShipsNothing(t *testing.T) {
	bucket, _, config := crashBucket(t)
	dir := t.TempDir()
	path := dir + "/idle.db"
	domain := "idle.example.test"
	// A fixed clock keeps compaction windows from elapsing between the
	// two runs, so the count of puts is deterministic.
	clock := &testClock{at: time.Date(2026, 9, 9, 8, 0, 30, 0, time.UTC)}
	options := Options{Now: clock.Now}

	s := crashOpenWith(t, config, domain, path, options)
	crashWrite(t, s, "filler", crashBlob('f', 90<<10))
	crashWrite(t, s, "state", []byte(`{"version":1}`))
	crashSync(t, s)
	crashWrite(t, s, "state", []byte(`{"version":2}`))
	crashSync(t, s)
	generation := s.rep.Status().Generation
	want := crashRows(t, s.db)
	s.close()
	if m := crashMarker(t, path); !m.Clean || m.Generation != generation {
		t.Fatalf("marker after the clean stop = %+v", m)
	}
	if seq, pages, _ := crashDirtyLog(t, path); seq != 2 || len(pages) != 0 {
		t.Fatalf("dirty log after the clean stop: sequence %d, %d pages", seq, len(pages))
	}
	putsBefore := crashPuts(bucket)

	again := crashOpenWith(t, config, domain, path, options)
	status := again.rep.Status()
	if status.Generation != generation || status.PendingPages != 0 || !status.Complete {
		t.Fatalf("status after the clean restart = %+v", status)
	}
	if again.rep.SnapshotDue() {
		t.Fatal("a snapshot is due after a clean restart")
	}
	crashSync(t, again)
	if puts := crashPuts(bucket); puts != putsBefore {
		t.Fatalf("the sync after an idle restart put %d objects", puts-putsBefore)
	}
	status = again.rep.Status()
	if status.Generation != generation || status.PendingPages != 0 || status.LastError != "" {
		t.Fatalf("status after the idle sync = %+v", status)
	}
	if m := crashMarker(t, path); !m.Clean || m.Seq != 2 {
		t.Fatalf("marker after the idle sync = %+v", m)
	}
	if seq, pages, _ := crashDirtyLog(t, path); seq != 2 || len(pages) != 0 {
		t.Fatalf("dirty log after the idle sync: sequence %d, %d pages", seq, len(pages))
	}
	again.close()
	got := crashRestore(t, config, domain, dir+"/restored/idle.db")
	crashEqual(t, "restore after the idle restart", got, want)
}

// Prepare starts the dirty log next to a database it restored, so a write
// and a crash right after the restore continue the generation with only
// the pages that changed.
func TestPrepareStartsTheDirtyLogForARestoredDatabase(t *testing.T) {
	_, _, config := crashBucket(t)
	dir := t.TempDir()
	domain := "rebuilt.example.test"

	source := crashOpen(t, config, domain, dir+"/source.db")
	crashWrite(t, source, "filler", crashBlob('f', 200<<10))
	crashWrite(t, source, "state", []byte(`{"version":1}`))
	crashSync(t, source)
	generation := source.rep.Status().Generation
	source.close()

	restoredPath := dir + "/restored/rebuilt.db"
	if err := os.MkdirAll(filepath.Dir(restoredPath), 0o755); err != nil {
		t.Fatal(err)
	}
	restored := crashOpen(t, config, domain, restoredPath)
	if status := restored.rep.Status(); !status.Restored || status.Generation != generation {
		t.Fatalf("status after the restore = %+v", status)
	}
	m := crashMarker(t, restoredPath)
	if !m.Clean || m.Generation != generation || m.PageSize == 0 {
		t.Fatalf("marker after the restore = %+v", m)
	}
	pages := int(crashFileSize(t, restoredPath) / int64(m.PageSize))
	if seq, logged, _ := crashDirtyLog(t, restoredPath); seq != m.Seq || len(logged) != 0 {
		t.Fatalf("dirty log of the restored database: sequence %d with %d pages; the marker is at %d", seq, len(logged), m.Seq)
	}
	// A write without a sync, then the stop: unclean.
	crashWrite(t, restored, "state", []byte(`{"version":2}`))
	want := crashRows(t, restored.db)
	restored.close()
	if m := crashMarker(t, restoredPath); m.Clean {
		t.Fatalf("marker after the unclean stop = %+v", m)
	}

	again := crashOpen(t, config, domain, restoredPath)
	status := again.rep.Status()
	if status.Generation != generation {
		t.Fatalf("the restored generation did not continue: %+v", status)
	}
	if status.PendingPages == 0 || status.PendingPages >= pages {
		t.Fatalf("pending %d pages of %d; the dirty log did not limit the difference", status.PendingPages, pages)
	}
	crashSync(t, again)
	status = again.rep.Status()
	if status.Generation != generation || status.PendingPages != 0 || !status.Complete {
		t.Fatalf("status after the sync = %+v", status)
	}
	again.close()
	got := crashRestore(t, config, domain, dir+"/check/rebuilt.db")
	crashEqual(t, "restore after the continued generation", got, want)
}
