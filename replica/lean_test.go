package replica

import (
	"context"
	"errors"
	"sync"
	"testing"
	"time"
)

// A virtual large source lets this test cross terabyte page counts without
// allocating or writing a terabyte. SQLite still takes real locks on the
// fixture; only the capture's reads see the large, zero-filled file.
type virtualDatabaseStorage struct {
	Storage
	path string
	size int64
	read int64
}

type virtualDatabaseFile struct {
	File
	storage *virtualDatabaseStorage
}

func (s *virtualDatabaseStorage) Open(path string, create bool) (File, error) {
	file, err := s.Storage.Open(path, create)
	if err == nil && path == s.path {
		return &virtualDatabaseFile{File: file, storage: s}, nil
	}
	return file, err
}
func (f *virtualDatabaseFile) Size() (int64, error) { return f.storage.size, nil }
func (f *virtualDatabaseFile) ReadAt(data []byte, offset int64) (int, error) {
	clear(data)
	f.storage.read += int64(len(data))
	return len(data), nil
}

func TestTerabyteSnapshotStartsWithOneSegmentAndRetriesOnlyActualWrites(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("before"))
	f.sync(t)
	f.write(t, []byte("after"))
	pending := f.rep.tracker.pendingPages()
	files := &virtualDatabaseStorage{Storage: f.rep.files, path: f.rep.path, size: 1 << 40}
	f.rep.files = files
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	f.rep.OnCopy = func(progress CopyProgress) {
		if progress.Stage == "read" {
			if progress.Total != files.size {
				t.Errorf("copy total = %d, want %d", progress.Total, files.size)
			}
			cancel()
		}
	}
	captured, err := f.rep.capture(ctx, f.db, true)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("capture = %v", err)
	}
	if !captured.snapshot || files.read != int64(f.rep.config.SegmentBytes) {
		t.Fatalf("first segment: snapshot=%v bytes=%d", captured.snapshot, files.read)
	}
	if len(captured.pages) != pending || f.rep.tracker.pendingPages() != pending {
		t.Fatalf("snapshot range polluted dirty tracking: captured=%d pending=%d want=%d", len(captured.pages), f.rep.tracker.pendingPages(), pending)
	}
}

func TestCompactionLoadsOnlyOneMetadataLayout(t *testing.T) {
	f := newFixture(t)
	f.rep.config.Schedule = []Level{{Window: time.Minute, Keep: 2 * time.Minute}, {Window: 5 * time.Minute, Keep: time.Hour}}
	f.write(t, []byte("before"))
	f.sync(t)
	f.clock.Add(10 * time.Second)
	f.write(t, []byte("after"))
	f.sync(t)
	generation := f.rep.getMarker().Generation
	f.clock.Add(10 * time.Minute)
	var mu sync.Mutex
	lists, snapshots := 0, 0
	f.objects.setHook(func(method, key string, _ []byte) error {
		mu.Lock()
		defer mu.Unlock()
		if method == "list" {
			lists++
			if key != f.rep.generationPrefix(generation)+"L" {
				t.Errorf("compaction lists data objects: %q", key)
			}
		}
		if method == "get" && key == f.rep.snapshotKey(generation) {
			snapshots++
		}
		return nil
	})
	if err := f.rep.compact(context.Background(), generation, f.clock.Now()); err != nil {
		t.Fatal(err)
	}
	if lists != 1 || snapshots != 1 {
		t.Fatalf("compaction reloaded unchanged metadata: lists=%d snapshot GETs=%d", lists, snapshots)
	}
	f.objects.setHook(nil)
	f.check(t, generation, time.Time{}, []byte("after"))
}

func TestPrefetchReusesSlotsWithoutReorderingParts(t *testing.T) {
	objects := newMemoryObjects()
	rep := &Replica{s3: objects}
	const count = 37 // wraps the four-slot ring repeatedly
	refs := make([]partRef, count)
	for i := range refs {
		data := make([]byte, 512)
		data[0] = byte(i)
		ref, err := rep.putPart(context.Background(), string(rune('a'+i)), segment{PageSize: 512, DBSize: 512, Pages: []uint32{1}, Data: [][]byte{data}})
		if err != nil {
			t.Fatal(err)
		}
		refs[i] = ref
	}
	firstStarted := make(chan struct{})
	releaseFirst := make(chan struct{})
	var mu sync.Mutex
	started := 0
	objects.setHook(func(method, key string, _ []byte) error {
		if method != "get" {
			return nil
		}
		mu.Lock()
		started++
		mu.Unlock()
		if key == refs[0].Key {
			close(firstStarted)
			<-releaseFirst
		}
		return nil
	})
	next, stop := rep.prefetchParts(context.Background(), refs, 4)
	defer stop()
	<-firstStarted
	mu.Lock()
	if started > 4 {
		t.Errorf("started %d downloads without consuming a part", started)
	}
	mu.Unlock()
	close(releaseFirst)
	for i := range refs {
		seg, err := next()
		if err != nil || len(seg.Data) != 1 || seg.Data[0][0] != byte(i) {
			t.Fatalf("part %d = %+v, %v", i, seg, err)
		}
	}
	if _, err := next(); err == nil {
		t.Fatal("read beyond the restore plan succeeded")
	}
}
