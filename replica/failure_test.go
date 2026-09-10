package replica

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

var errInjected = errors.New("injected failure")

// The hook runs outside the backend lock, so tests can pause an upload while
// another goroutine writes SQLite, fetches a point, or simulates a lost reply.
type memoryObjects struct {
	mu   sync.Mutex
	data map[string][]byte
	hook func(method, key string, data []byte) error
}

func newMemoryObjects() *memoryObjects { return &memoryObjects{data: map[string][]byte{}} }
func (b *memoryObjects) call(method, key string, data []byte) error {
	b.mu.Lock()
	hook := b.hook
	b.mu.Unlock()
	if hook != nil {
		return hook(method, key, data)
	}
	return nil
}
func (b *memoryObjects) setHook(hook func(string, string, []byte) error) {
	b.mu.Lock()
	b.hook = hook
	b.mu.Unlock()
}
func (b *memoryObjects) Put(ctx context.Context, key string, data []byte) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := b.call("put", key, data); err != nil {
		return err
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	b.data[key] = bytes.Clone(data)
	return nil
}
func (b *memoryObjects) Get(ctx context.Context, key string) ([]byte, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if err := b.call("get", key, nil); err != nil {
		return nil, err
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	data, ok := b.data[key]
	if !ok {
		return nil, ErrNotFound
	}
	return bytes.Clone(data), nil
}
func (b *memoryObjects) Delete(ctx context.Context, key string) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := b.call("delete", key, nil); err != nil {
		return err
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	delete(b.data, key)
	return nil
}
func (b *memoryObjects) List(ctx context.Context, prefix string) ([]Object, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if err := b.call("list", prefix, nil); err != nil {
		return nil, err
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	var out []Object
	for key, data := range b.data {
		if strings.HasPrefix(key, prefix) {
			out = append(out, Object{Key: key, Size: int64(len(data))})
		}
	}
	return out, nil
}

type testClock struct {
	mu sync.Mutex
	at time.Time
}

func (c *testClock) Now() time.Time      { c.mu.Lock(); defer c.mu.Unlock(); return c.at }
func (c *testClock) Add(d time.Duration) { c.mu.Lock(); c.at = c.at.Add(d); c.mu.Unlock() }

type fixture struct {
	rep     *Replica
	db      *testDatabase
	objects *memoryObjects
	clock   *testClock
	dir     string
}

func newFixture(t *testing.T) *fixture {
	t.Helper()
	f := &fixture{objects: newMemoryObjects(), clock: &testClock{at: time.Date(2026, 9, 9, 8, 0, 30, 0, time.UTC)}, dir: t.TempDir()}
	config := Config{SegmentBytes: 4096, Schedule: []Level{{Window: 15 * time.Minute, Keep: 2 * time.Hour}, {Window: time.Hour, Keep: 24 * time.Hour}}}
	var err error
	f.rep, err = NewWithOptions(config, t.Name(), f.dir+"/source.db", vfs.Find(""), nil, Options{Objects: f.objects, Now: f.clock.Now})
	if err != nil {
		t.Fatal(err)
	}
	if err := f.rep.Prepare(context.Background()); err != nil {
		t.Fatal(err)
	}
	f.db, err = openTestDatabase(f.rep.path, f.rep.VFSName())
	if err != nil {
		t.Fatal(err)
	}
	f.rep.Attach(f.db)
	t.Cleanup(func() { f.db.Close(); f.rep.Close() })
	return f
}
func (f *fixture) write(t *testing.T, value []byte) {
	t.Helper()
	if err := f.db.WriteFile("state", value); err != nil {
		t.Fatal(err)
	}
}
func (f *fixture) sync(t *testing.T) {
	t.Helper()
	if err := f.rep.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
}
func (f *fixture) check(t *testing.T, generation string, at time.Time, want []byte) {
	t.Helper()
	path := fmt.Sprintf("%s/check-%d.db", f.dir, time.Now().UnixNano())
	if err := f.rep.Fetch(context.Background(), generation, at, path); err != nil {
		t.Fatal(err)
	}
	db, err := openTestDatabase(path, "")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	var integrity string
	if err := db.db.QueryRow(`PRAGMA integrity_check`).Scan(&integrity); err != nil || integrity != "ok" {
		t.Fatalf("integrity = %q, %v", integrity, err)
	}
	got, err := db.ReadFile("state")
	if err != nil || !bytes.Equal(got, want) {
		t.Fatalf("restored %d bytes, want %d, error %v", len(got), len(want), err)
	}
}

func TestIncompleteBatchNeverBecomesRestorePoint(t *testing.T) {
	for _, failure := range []string{"part", "manifest", "lost-manifest-reply"} {
		t.Run(failure, func(t *testing.T) {
			f := newFixture(t)
			old := bytes.Repeat([]byte("a"), 64<<10)
			next := bytes.Repeat([]byte("b"), 64<<10)
			f.write(t, old)
			f.sync(t)
			generation := f.rep.Status().Generation
			f.clock.Add(time.Minute)
			f.write(t, next)
			parts := 0
			f.objects.setHook(func(method, key string, data []byte) error {
				if method != "put" {
					return nil
				}
				if strings.HasSuffix(key, ".seg") {
					parts++
					if failure == "part" && parts == 2 {
						return errInjected
					}
				}
				if strings.Contains(key, "/L0/") && failure != "part" {
					if failure == "lost-manifest-reply" {
						f.objects.mu.Lock()
						f.objects.data[key] = bytes.Clone(data)
						f.objects.mu.Unlock()
					}
					return errInjected
				}
				return nil
			})
			if err := f.rep.Sync(context.Background()); !errors.Is(err, errInjected) {
				t.Fatalf("sync = %v", err)
			}
			f.objects.setHook(nil)
			want := old
			if failure == "lost-manifest-reply" {
				want = next
			}
			f.check(t, generation, time.Time{}, want)
			f.sync(t)
			f.check(t, f.rep.Status().Generation, time.Time{}, next)
		})
	}
}

func TestWritesDuringUploadDoNotMixCapturedPagesOrBecomeClean(t *testing.T) {
	f := newFixture(t)
	old := bytes.Repeat([]byte("a"), 128<<10)
	next := bytes.Repeat([]byte("b"), 128<<10)
	f.write(t, old)
	entered, release := make(chan struct{}), make(chan struct{})
	var once sync.Once
	f.objects.setHook(func(method, key string, _ []byte) error {
		if method == "put" && strings.HasSuffix(key, ".seg") {
			once.Do(func() { close(entered); <-release })
		}
		return nil
	})
	done := make(chan error, 1)
	go func() { done <- f.rep.Sync(context.Background()) }()
	select {
	case <-entered:
	case <-time.After(5 * time.Second):
		t.Fatal("upload did not start")
	}
	written := make(chan error, 1)
	go func() { written <- f.db.WriteFile("state", next) }()
	select {
	case err := <-written:
		if err != nil {
			close(release)
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		close(release)
		t.Fatal("upload holds database transaction")
	}
	close(release)
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	f.objects.setHook(nil)
	stored, err := f.rep.readMarker()
	if err != nil || stored.Clean {
		t.Fatalf("new write marked clean: %+v %v", stored, err)
	}
	f.check(t, f.rep.Status().Generation, time.Time{}, old)
	f.clock.Add(time.Minute)
	f.sync(t)
	f.check(t, f.rep.Status().Generation, time.Time{}, next)
}

func TestPartialWindowRetryPreservesSourceAndPublishesAllParts(t *testing.T) {
	for _, failure := range []string{"part", "manifest", "lost-manifest-reply", "delete"} {
		t.Run(failure, func(t *testing.T) {
			f := newFixture(t)
			f.write(t, []byte("snapshot"))
			f.sync(t)
			snapshot := f.rep.getMarker()
			f.clock.Add(5 * time.Minute)
			value := bytes.Repeat([]byte("z"), 80<<10)
			f.write(t, value)
			f.sync(t)
			f.clock.Add(20 * time.Minute)
			parts := 0
			failed := false
			f.objects.setHook(func(method, key string, data []byte) error {
				if failed {
					return nil
				}
				if method == "put" && strings.HasSuffix(key, ".seg") {
					parts++
					if failure == "part" && parts == 2 {
						failed = true
						return errInjected
					}
				}
				if method == "put" && strings.HasSuffix(key, "/complete") && (failure == "manifest" || failure == "lost-manifest-reply") {
					if failure == "lost-manifest-reply" {
						f.objects.mu.Lock()
						f.objects.data[key] = bytes.Clone(data)
						f.objects.mu.Unlock()
					}
					failed = true
					return errInjected
				}
				if method == "delete" && failure == "delete" {
					failed = true
					return errInjected
				}
				return nil
			})
			if err := f.rep.Sync(context.Background()); !errors.Is(err, errInjected) {
				t.Fatalf("expected %s failure: %v", failure, err)
			}
			f.objects.setHook(nil)
			f.check(t, snapshot.Generation, time.Time{}, value)
			f.sync(t)
			f.check(t, snapshot.Generation, time.Time{}, value)
			f.check(t, snapshot.Generation, snapshot.At, []byte("snapshot"))
			l, err := f.rep.loadLayout(context.Background(), snapshot.Generation)
			if err != nil {
				t.Fatal(err)
			}
			if len(l.raw) != 0 || len(l.windows[1]) != 1 || len(l.windows[1][0].parts) < 2 {
				t.Fatalf("unexpected completed layout: raw=%d windows=%v", len(l.raw), l.windows)
			}
		})
	}
}

func TestEveryAdvertisedPointSurvivesCompaction(t *testing.T) {
	f := newFixture(t)
	expected := map[time.Time][]byte{}
	for i := 0; i < 8; i++ {
		value := bytes.Repeat([]byte{byte('a' + i)}, (i+1)*8192)
		f.write(t, value)
		f.sync(t)
		expected[f.rep.getMarker().At] = value
		f.clock.Add(20 * time.Minute)
	}
	f.clock.Add(3 * time.Hour)
	f.sync(t)
	points, err := f.rep.Points(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(points) < 2 {
		t.Fatal("expected snapshot and coarse points")
	}
	for _, point := range points {
		var chosen time.Time
		var want []byte
		for at, value := range expected {
			if !at.After(point.At) && at.After(chosen) {
				chosen = at
				want = value
			}
		}
		f.check(t, point.Generation, point.At, want)
	}
}

// Local wrappers inject errors independently of SQLite's real VFS.
type faultStorage struct {
	Storage
	open   func(string, bool) error
	wrap   func(string, File) File
	remove func(string) error
}

func (s faultStorage) Open(path string, create bool) (File, error) {
	if s.open != nil {
		if err := s.open(path, create); err != nil {
			return nil, err
		}
	}
	f, err := s.Storage.Open(path, create)
	if err == nil && s.wrap != nil {
		f = s.wrap(path, f)
	}
	return f, err
}
func (s faultStorage) Remove(path string) error {
	if s.remove != nil {
		if err := s.remove(path); err != nil {
			return err
		}
	}
	return s.Storage.Remove(path)
}

type faultFile struct {
	File
	write func([]byte, int64) (int, error)
	sync  func() error
}

func (f faultFile) WriteAt(data []byte, offset int64) (int, error) {
	if f.write != nil {
		return f.write(data, offset)
	}
	return f.File.WriteAt(data, offset)
}
func (f faultFile) Sync() error {
	if f.sync != nil {
		return f.sync()
	}
	return f.File.Sync()
}

func TestRestoreFailureLeavesOriginalAndRetryRepairsPublication(t *testing.T) {
	for _, failure := range []string{"download", "checksum", "copy", "intent-removal"} {
		t.Run(failure, func(t *testing.T) {
			f := newFixture(t)
			value := bytes.Repeat([]byte("q"), 80<<10)
			f.write(t, value)
			f.sync(t)
			path := f.dir + "/target.db"
			target, err := NewWithOptions(f.rep.config, t.Name()+"-target", path, vfs.Find(""), nil, Options{Objects: f.objects, Now: f.clock.Now})
			if err != nil {
				t.Fatal(err)
			}
			defer target.Close()
			path = target.path
			// Simulate a previously interrupted publication for an existing target.
			if failure == "copy" || failure == "intent-removal" {
				target.files = faultStorage{Storage: OSStorage(), wrap: func(name string, file File) File {
					if name == path && failure == "copy" {
						return faultFile{File: file, write: func(data []byte, offset int64) (int, error) {
							file.WriteAt(data[:min(512, len(data))], offset)
							return 0, errInjected
						}}
					}
					return file
				}, remove: func(name string) error {
					if failure == "intent-removal" && strings.HasSuffix(name, ".replica-restoring") {
						return errInjected
					}
					return nil
				}}
			} else {
				f.objects.setHook(func(method, key string, _ []byte) error {
					if method == "get" && strings.HasSuffix(key, ".seg") {
						if failure == "checksum" {
							f.objects.mu.Lock()
							f.objects.data[key] = []byte("corrupt")
							f.objects.mu.Unlock()
							return nil
						}
						return errInjected
					}
					return nil
				})
			}
			// Both replicas use the source namespace; registration identity is separate.
			target.domain = f.rep.domain
			if err := target.Prepare(context.Background()); err == nil {
				t.Fatal("expected restore failure")
			}
			if failure == "download" || failure == "checksum" {
				if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
					t.Fatalf("download touched destination: %v", err)
				}
			} else {
				if exists, _ := target.files.Exists(path + ".replica-restoring"); !exists {
					t.Fatal("publication lost durable intent")
				}
			}
			if failure == "checksum" {
				return
			}
			f.objects.setHook(nil)
			target.Close()
			retry, err := NewWithOptions(f.rep.config, t.Name()+"-target", path, vfs.Find(""), nil, Options{Objects: f.objects, Now: f.clock.Now})
			if err != nil {
				t.Fatal(err)
			}
			defer retry.Close()
			retry.domain = f.rep.domain
			if err := retry.Prepare(context.Background()); err != nil {
				t.Fatal(err)
			}
			db, err := openTestDatabase(path, "")
			if err != nil {
				t.Fatal(err)
			}
			defer db.Close()
			got, err := db.ReadFile("state")
			if err != nil || !bytes.Equal(got, value) {
				t.Fatalf("retry lost database: %v", err)
			}
		})
	}
}

func TestMarkerFailureBlocksSQLiteWriteAndRetries(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("before"))
	f.sync(t)
	original, err := os.ReadFile(f.rep.path)
	if err != nil {
		t.Fatal(err)
	}
	f.rep.files = faultStorage{Storage: OSStorage(), open: func(path string, _ bool) error {
		if path == f.rep.markerPath() {
			return errInjected
		}
		return nil
	}}
	if err := f.db.WriteFile("state", []byte("after")); err == nil {
		t.Fatal("SQLite write succeeded without durable unclean marker")
	}
	after, err := os.ReadFile(f.rep.path)
	if err != nil || !bytes.Equal(original, after) {
		t.Fatal("failed marker allowed main database write")
	}
	f.rep.files = OSStorage()
	f.write(t, []byte("after"))
	f.sync(t)
	f.check(t, f.rep.Status().Generation, time.Time{}, []byte("after"))
}

func TestSettleSerializesDurableMarkerWithFirstWrite(t *testing.T) {
	tracker := newTracker(newDirtyLog(OSStorage(), t.TempDir()+"/tracker.db"))
	tracker.clean = false
	entered, release := make(chan struct{}), make(chan struct{})
	var mu sync.Mutex
	var events []string
	tracker.onUnclean = func() error { mu.Lock(); events = append(events, "unclean"); mu.Unlock(); return nil }
	settled := make(chan error, 1)
	go func() {
		settled <- tracker.settle(0, func() error {
			close(entered)
			<-release
			mu.Lock()
			events = append(events, "clean")
			mu.Unlock()
			return nil
		})
	}()
	<-entered
	written := make(chan error, 1)
	go func() { written <- tracker.write(0, bytes.Repeat([]byte{0}, 512)) }()
	close(release)
	if err := <-settled; err != nil {
		t.Fatal(err)
	}
	if err := <-written; err != nil {
		t.Fatal(err)
	}
	mu.Lock()
	defer mu.Unlock()
	if strings.Join(events, ",") != "clean,unclean" {
		t.Fatalf("marker order %v", events)
	}
}

func TestSpoolFailureKeepsDirtyPagesAndDoesNotPublish(t *testing.T) {
	f := newFixture(t)
	f.write(t, bytes.Repeat([]byte("x"), 80<<10))
	f.sync(t)
	before := f.rep.getMarker()
	f.write(t, bytes.Repeat([]byte("y"), 80<<10))
	f.rep.files = faultStorage{Storage: OSStorage(), wrap: func(path string, file File) File {
		if strings.Contains(path, ".replica-spool") {
			return faultFile{File: file, sync: func() error { return errInjected }}
		}
		return file
	}}
	if err := f.rep.Sync(context.Background()); !errors.Is(err, errInjected) {
		t.Fatalf("sync %v", err)
	}
	if f.rep.tracker.pendingPages() == 0 || f.rep.getMarker().Seq != before.Seq {
		t.Fatal("spool failure lost pending pages or advanced commit")
	}
	f.rep.files = OSStorage()
	f.sync(t)
	f.check(t, f.rep.Status().Generation, time.Time{}, bytes.Repeat([]byte("y"), 80<<10))
}

func TestTruncateOnlyChangeAndSegmentValidation(t *testing.T) {
	tracker := newTracker(newDirtyLog(OSStorage(), t.TempDir()+"/tracker.db"))
	calls := 0
	tracker.onUnclean = func() error { calls++; return nil }
	if err := tracker.truncate(0); err != nil {
		t.Fatal(err)
	}
	if calls != 1 || tracker.pendingPages() != 1 {
		t.Fatal("truncate did not invalidate marker and enqueue size change")
	}
	for _, seg := range []segment{
		{PageSize: 513, DBSize: 513}, {PageSize: 512, DBSize: -512}, {PageSize: 512, DBSize: 1},
		{PageSize: 512, DBSize: 512, Pages: []uint32{0}, Data: [][]byte{make([]byte, 512)}},
		{PageSize: 512, DBSize: 512, Pages: []uint32{2}, Data: [][]byte{make([]byte, 512)}},
		{PageSize: 512, DBSize: 512, Pages: []uint32{1, 1}, Data: [][]byte{make([]byte, 512), make([]byte, 512)}},
	} {
		if _, err := decodeSegment(encodeSegment(seg)); err == nil {
			t.Fatal("accepted invalid segment")
		}
	}
}

func TestLegacyFormatFailsClosedAndLocalDatabaseMigrates(t *testing.T) {
	f := newFixture(t)
	gen := newGenerationID(f.clock.Now())
	data, _ := json.Marshal(map[string]any{"seq": 1, "at": f.clock.Now()})
	if err := f.objects.Put(context.Background(), f.rep.snapshotKey(gen), data); err != nil {
		t.Fatal(err)
	}
	if err := f.rep.Fetch(context.Background(), gen, time.Time{}, f.dir+"/legacy.db"); !errors.Is(err, ErrLegacyFormat) {
		t.Fatalf("legacy restore %v", err)
	}
	// Existing local files start a new generation when their marker is old.
	if err := f.rep.writeMarker(marker{Generation: gen, Complete: true, Clean: true}); err != nil {
		t.Fatal(err)
	}
	if err := f.rep.Prepare(context.Background()); err != nil {
		t.Fatal(err)
	}
	f.write(t, []byte("local"))
	f.sync(t)
	if f.rep.getMarker().Version != formatVersion || f.rep.getMarker().Generation == gen {
		t.Fatal("legacy marker resumed")
	}
}

func TestShortLocalWriteIsAnError(t *testing.T) {
	f := newFixture(t)
	f.rep.files = faultStorage{Storage: OSStorage(), wrap: func(_ string, file File) File {
		return faultFile{File: file, write: func(data []byte, _ int64) (int, error) { return len(data) - 1, nil }}
	}}
	if err := f.rep.writeLocal(f.dir+"/short", []byte("abc")); !errors.Is(err, io.ErrShortWrite) {
		t.Fatalf("short write = %v", err)
	}
}

func TestUncertainCleanWriteForcesNextWriteToInvalidateMarker(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("before"))
	f.rep.files = faultStorage{Storage: OSStorage(), wrap: func(path string, file File) File {
		if path == f.rep.markerPath() {
			return faultFile{File: file, sync: func() error {
				data, err := os.ReadFile(path)
				if err != nil {
					return err
				}
				var m marker
				if err := json.Unmarshal(data, &m); err != nil {
					return err
				}
				if err := file.Sync(); err != nil {
					return err
				}
				if m.Clean {
					return errInjected
				}
				return nil
			}}
		}
		return file
	}}
	if err := f.rep.Sync(context.Background()); !errors.Is(err, errInjected) {
		t.Fatalf("sync %v", err)
	}
	f.rep.files = OSStorage()
	f.write(t, []byte("after"))
	m, err := f.rep.readMarker()
	if err != nil || m.Clean {
		t.Fatalf("uncertain clean write was not invalidated: %+v %v", m, err)
	}
	f.sync(t)
	f.check(t, f.rep.Status().Generation, time.Time{}, []byte("after"))
}

func TestPageSizeChangeStartsFreshSnapshot(t *testing.T) {
	f := newFixture(t)
	value := bytes.Repeat([]byte("size"), 10000)
	f.write(t, value)
	f.sync(t)
	before := f.rep.getMarker()
	if _, err := f.db.db.Exec(`PRAGMA page_size=8192; VACUUM`); err != nil {
		t.Fatal(err)
	}
	f.clock.Add(time.Minute)
	f.sync(t)
	after := f.rep.getMarker()
	if after.Generation == before.Generation || after.PageSize != 8192 {
		t.Fatalf("page size changed without snapshot: %+v", after)
	}
	f.check(t, after.Generation, time.Time{}, value)
}

func TestIdleCompactedGenerationResumesCorrectSequenceAfterRestore(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("first"))
	f.sync(t)
	f.clock.Add(5 * time.Minute)
	f.write(t, []byte("second"))
	f.sync(t)
	f.clock.Add(3 * time.Hour)
	f.sync(t)
	before := f.rep.getMarker()
	path := f.dir + "/resumed.db"
	m, err := f.rep.restoreInto(context.Background(), before.Generation, time.Time{}, path)
	if err != nil {
		t.Fatal(err)
	}
	if m.Seq != before.Seq || m.SealedAt.IsZero() {
		t.Fatalf("restored cursor %+v, source %+v", m, before)
	}
	// Continue on the restored file with its returned durable marker.
	f.db.Close()
	f.rep.Close()
	r, err := NewWithOptions(f.rep.config, f.rep.domain, path, vfs.Find(""), nil, Options{Objects: f.objects, Now: f.clock.Now})
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	if err := r.setMarker(m); err != nil {
		t.Fatal(err)
	}
	if err := r.Prepare(context.Background()); err != nil {
		t.Fatal(err)
	}
	db, err := openTestDatabase(path, r.VFSName())
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	r.Attach(db)
	if err := db.WriteFile("state", []byte("third")); err != nil {
		t.Fatal(err)
	}
	if err := r.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if r.getMarker().Seq != before.Seq+1 || r.getMarker().Generation != before.Generation {
		t.Fatalf("restored cursor not continued: %+v", r.getMarker())
	}
	f.rep = r
	f.db = db
	f.check(t, before.Generation, time.Time{}, []byte("third"))
}

func TestFetchDoesNotTouchExistingDestinationOnDownloadFailure(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("source"))
	f.sync(t)
	path := f.dir + "/existing"
	original := []byte("keep this file")
	if err := os.WriteFile(path, original, 0600); err != nil {
		t.Fatal(err)
	}
	f.objects.setHook(func(method, key string, _ []byte) error {
		if method == "get" && strings.HasSuffix(key, ".seg") {
			return errInjected
		}
		return nil
	})
	if err := f.rep.Fetch(context.Background(), f.rep.Status().Generation, time.Time{}, path); !errors.Is(err, errInjected) {
		t.Fatalf("fetch %v", err)
	}
	got, err := os.ReadFile(path)
	if err != nil || !bytes.Equal(got, original) {
		t.Fatal("failed download modified existing destination")
	}
}

func TestMissingCommittedPartFailsRestoreWithoutPublishing(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("source"))
	f.sync(t)
	l, err := f.rep.loadLayout(context.Background(), f.rep.Status().Generation)
	if err != nil {
		t.Fatal(err)
	}
	if err := f.objects.Delete(context.Background(), l.snapshot.Parts[0].Key); err != nil {
		t.Fatal(err)
	}
	path := f.dir + "/missing"
	if err := f.rep.Fetch(context.Background(), l.generation, time.Time{}, path); !errors.Is(err, ErrNotFound) {
		t.Fatalf("fetch %v", err)
	}
	if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("published incomplete restore")
	}
}

func TestClockRollbackCannotAddToSealedWindows(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("first"))
	f.sync(t)
	f.clock.Add(5 * time.Minute)
	f.write(t, []byte("second"))
	f.sync(t)
	f.clock.Add(time.Hour)
	f.sync(t)
	frontier := f.rep.getMarker().SealedAt
	f.clock.Add(-time.Hour)
	f.write(t, []byte("third"))
	f.sync(t)
	if f.rep.getMarker().At.Before(frontier) {
		t.Fatal("new commit entered an already sealed window")
	}
	f.check(t, f.rep.Status().Generation, time.Time{}, []byte("third"))
}

func TestCompactionPreservesShrinkThenGrow(t *testing.T) {
	f := newFixture(t)
	r := f.rep
	ctx := context.Background()
	gen := newGenerationID(f.clock.Now())
	stages := []segment{
		{PageSize: 512, DBSize: 1536, Pages: []uint32{1, 2, 3}, Data: [][]byte{bytes.Repeat([]byte("a"), 512), bytes.Repeat([]byte("b"), 512), bytes.Repeat([]byte("c"), 512)}},
		{PageSize: 512, DBSize: 1536, Pages: []uint32{2}, Data: [][]byte{bytes.Repeat([]byte("d"), 512)}},
		{PageSize: 512, DBSize: 512, Pages: []uint32{1}, Data: [][]byte{bytes.Repeat([]byte("x"), 512)}},
		{PageSize: 512, DBSize: 1536, Pages: []uint32{3}, Data: [][]byte{bytes.Repeat([]byte("z"), 512)}},
	}
	for index, seg := range stages {
		if index == 2 {
			f.clock.Add(20 * time.Minute)
		} else {
			f.clock.Add(time.Minute)
		}
		ref, err := r.putPart(ctx, fmt.Sprintf("%sdata/input/%d.seg", r.generationPrefix(gen), index), seg)
		if err != nil {
			t.Fatal(err)
		}
		m := manifest{Version: formatVersion, FirstSeq: int64(index + 1), Seq: int64(index + 1), At: f.clock.Now(), MinSize: seg.DBSize, Parts: []partRef{ref}}
		key := r.rawKey(gen, m.Seq, m.At)
		if index == 0 {
			key = r.snapshotKey(gen)
		}
		if err := r.putManifest(ctx, key, m); err != nil {
			t.Fatal(err)
		}
	}
	before := f.dir + "/before-compact"
	if err := r.Fetch(ctx, gen, time.Time{}, before); err != nil {
		t.Fatal(err)
	}
	f.clock.Add(2 * time.Hour)
	if err := r.compact(ctx, gen); err != nil {
		t.Fatal(err)
	}
	after := f.dir + "/after-compact"
	if err := r.Fetch(ctx, gen, time.Time{}, after); err != nil {
		t.Fatal(err)
	}
	a, err := os.ReadFile(before)
	if err != nil {
		t.Fatal(err)
	}
	b, err := os.ReadFile(after)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(a, b) || !bytes.Equal(b[512:1024], make([]byte, 512)) {
		t.Fatal("compaction resurrected a page removed by truncation")
	}
}

type cancelStore struct {
	ObjectStore
	entered chan struct{}
}

func (s cancelStore) Put(ctx context.Context, key string, data []byte) error {
	close(s.entered)
	<-ctx.Done()
	return ctx.Err()
}
func TestCloseCancelsSyncBeforeReleasingRegistration(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("close"))
	entered := make(chan struct{})
	f.rep.s3 = cancelStore{ObjectStore: f.objects, entered: entered}
	synced := make(chan error, 1)
	go func() { synced <- f.rep.Sync(context.Background()) }()
	select {
	case <-entered:
	case <-time.After(5 * time.Second):
		t.Fatal("sync did not reach object store")
	}
	closed := make(chan struct{})
	go func() { f.rep.Close(); close(closed) }()
	select {
	case <-closed:
	case <-time.After(5 * time.Second):
		t.Fatal("Close did not cancel active sync")
	}
	if err := <-synced; !errors.Is(err, context.Canceled) {
		t.Fatalf("sync %v", err)
	}
	if err := f.rep.Sync(context.Background()); err == nil {
		t.Fatal("closed replica accepted sync")
	}
}

func TestMissingBatchCannotBeHiddenByLaterCommits(t *testing.T) {
	f := newFixture(t)
	for i := 0; i < 3; i++ {
		f.write(t, []byte{byte(i)})
		f.sync(t)
		f.clock.Add(time.Second)
	}
	l, err := f.rep.loadLayout(context.Background(), f.rep.Status().Generation)
	if err != nil {
		t.Fatal(err)
	}
	if err := f.objects.Delete(context.Background(), l.raw[0].key); err != nil {
		t.Fatal(err)
	}
	if err := f.rep.Fetch(context.Background(), l.generation, time.Time{}, f.dir+"/gap"); err == nil {
		t.Fatal("restore accepted missing batch")
	}
}

func TestCleanMarkerCannotResumeInDifferentBucketNamespace(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("kept locally"))
	f.sync(t)
	previous := f.rep.getMarker()
	if err := f.db.Close(); err != nil {
		t.Fatal(err)
	}
	f.rep.Close()
	config := f.rep.config
	config.Prefix += "/new-destination"
	next, err := NewWithOptions(config, f.rep.domain, f.rep.path, vfs.Find(""), nil, Options{Objects: f.objects, Now: f.clock.Now})
	if err != nil {
		t.Fatal(err)
	}
	defer next.Close()
	if err := next.Prepare(context.Background()); err != nil {
		t.Fatal(err)
	}
	db, err := openTestDatabase(next.path, next.VFSName())
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	next.Attach(db)
	if err := next.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if next.getMarker().Generation == previous.Generation || !next.Status().Complete {
		t.Fatal("clean marker from old namespace resumed without a snapshot")
	}
	current, err := f.objects.Get(context.Background(), next.currentKey())
	if err != nil || string(current) != next.Status().Generation {
		t.Fatalf("new namespace has no current generation: %q %v", current, err)
	}
	f.rep = next
	f.db = db
	f.check(t, next.Status().Generation, time.Time{}, []byte("kept locally"))
}
