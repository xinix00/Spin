package replica

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

// TestReplicaScale exercises real SQLite with a bucket on disk. The default
// uses 8 MiB of row data; larger runs are explicit, for example:
//
//	REPLICA_SCALE_MIB=1024 go test ./replica -run '^TestReplicaScale$' -count=1 -v -timeout=30m
//
// Allow roughly five times that much free disk. Each restore is checked and
// removed before the next one. Neither the bucket nor the content oracle keeps
// the database in memory. Reported heap samples are observations, not limits.
func TestReplicaScale(t *testing.T) {
	mib := 8
	if value := os.Getenv("REPLICA_SCALE_MIB"); value != "" {
		var err error
		mib, err = strconv.Atoi(value)
		if err != nil || mib < 1 || int64(mib) > (1<<63-1)/(1<<20) {
			t.Fatalf("REPLICA_SCALE_MIB must be a positive MiB count, got %q", value)
		}
	}
	const rowBytes = 32 << 10
	rows := int64(mib) * (1 << 20) / rowBytes
	initialRows := min(rows, (960<<20)/rowBytes)
	ctx := context.Background()
	dir := t.TempDir()
	path := filepath.Join(dir, "source.db")
	objects := &scaleDiskObjects{dir: filepath.Join(dir, "bucket")}
	if err := os.MkdirAll(objects.dir, 0o700); err != nil {
		t.Fatal(err)
	}
	clock := &testClock{at: time.Date(2026, 9, 25, 8, 0, 10, 0, time.UTC)}
	config := Config{
		SegmentBytes: 4 << 20,
		Schedule: []Level{
			{Window: time.Minute, Keep: 2 * time.Minute},
			{Window: 5 * time.Minute, Keep: time.Hour},
		},
	}
	// Build an existing database through the ordinary VFS. This exercises the
	// first backup of a large database without making the fixture's dirty-page
	// tracker itself account for every page before the snapshot starts.
	scaleMeasure(t, objects, "build", func() error {
		db, err := openTestDatabase(path, "")
		if err != nil {
			return err
		}
		defer db.Close()
		if _, err := db.db.Exec(`PRAGMA cache_size=-2048; CREATE TABLE scale_rows(id INTEGER PRIMARY KEY, revision INTEGER NOT NULL, payload BLOB NOT NULL)`); err != nil {
			return err
		}
		tx, err := db.db.BeginTx(ctx, nil)
		if err != nil {
			return err
		}
		defer tx.Rollback()
		stmt, err := tx.Prepare(`INSERT INTO scale_rows(id,revision,payload) VALUES(?,0,?)`)
		if err != nil {
			return err
		}
		defer stmt.Close()
		payload := scalePayload(rowBytes, 0)
		for id := int64(1); id <= initialRows; id++ {
			binary.LittleEndian.PutUint64(payload, uint64(id))
			if _, err := stmt.Exec(id, payload); err != nil {
				return err
			}
		}
		return tx.Commit()
	})
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("fixture: initial_rows=%d target_rows=%d target_payload=%d MiB initial_database=%.2f MiB segment=%d MiB", initialRows, rows, mib, float64(info.Size())/(1<<20), config.SegmentBytes/(1<<20))
	rep, err := NewWithOptions(config, t.Name(), path, vfs.Find(""), nil, Options{Objects: objects, Now: clock.Now})
	if err != nil {
		t.Fatal(err)
	}
	defer rep.Close()
	scaleMeasure(t, objects, "prepare", func() error { return rep.Prepare(ctx) })
	db, err := openTestDatabase(path, rep.VFSName())
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if _, err := db.db.Exec(`PRAGMA cache_size=-2048`); err != nil {
		t.Fatal(err)
	}
	rep.Attach(db)
	scaleMeasure(t, objects, "snapshot", func() error { return rep.Sync(ctx) })
	generation := rep.Status().Generation
	if status := rep.Status(); !status.Complete || status.PendingPages != 0 {
		t.Fatalf("snapshot status = %+v", status)
	}
	restore := func(phase string, at time.Time, count int64, revision int) {
		t.Helper()
		destination := filepath.Join(dir, "restored.db")
		scaleMeasure(t, objects, phase, func() error { return rep.Fetch(ctx, generation, at, destination) })
		scaleMeasure(t, objects, phase+"_verify", func() error {
			return scaleCheckDatabase(destination, count, rowBytes, revision)
		})
		if err := os.Remove(destination); err != nil {
			t.Fatal(err)
		}
	}
	restore("snapshot_restore", rep.getMarker().At, initialRows, 0)
	if initialRows < rows {
		// Cross SQLite's reserved lock-byte page through tracked writes. A
		// snapshot of an already-large DB alone cannot catch this boundary.
		scaleMeasure(t, objects, "grow", func() error {
			tx, err := db.db.BeginTx(ctx, nil)
			if err != nil {
				return err
			}
			defer tx.Rollback()
			stmt, err := tx.Prepare(`INSERT INTO scale_rows(id,revision,payload) VALUES(?,0,?)`)
			if err != nil {
				return err
			}
			defer stmt.Close()
			payload := scalePayload(rowBytes, 0)
			for id := initialRows + 1; id <= rows; id++ {
				binary.LittleEndian.PutUint64(payload, uint64(id))
				if _, err := stmt.Exec(id, payload); err != nil {
					return err
				}
			}
			return tx.Commit()
		})
		clock.Add(10 * time.Second)
		scaleMeasure(t, objects, "growth_increment", func() error { return rep.Sync(ctx) })
		if rep.Status().Generation != generation {
			t.Fatal("ordinary growth replaced the generation")
		}
		restore("growth_restore", rep.getMarker().At, rows, 0)
	}
	for revision := 1; revision <= 2; revision++ {
		scaleMeasure(t, objects, fmt.Sprintf("update_%d", revision), func() error {
			tx, err := db.db.BeginTx(ctx, nil)
			if err != nil {
				return err
			}
			defer tx.Rollback()
			stmt, err := tx.Prepare(`UPDATE scale_rows SET revision=?,payload=? WHERE id=?`)
			if err != nil {
				return err
			}
			defer stmt.Close()
			payload := scalePayload(rowBytes, revision)
			for id := int64(1); id <= rows; id++ {
				// The second update overlaps the first and also changes rows
				// absent from it: compaction must preserve each latest value.
				if id%4 >= int64(revision) {
					continue
				}
				binary.LittleEndian.PutUint64(payload, uint64(id))
				if _, err := stmt.Exec(revision, payload, id); err != nil {
					return err
				}
			}
			return tx.Commit()
		})
		clock.Add(10 * time.Second)
		before := objects.stats()
		scaleMeasure(t, objects, fmt.Sprintf("increment_%d", revision), func() error { return rep.Sync(ctx) })
		if uploaded := objects.stats().written - before.written; uploaded >= uint64(info.Size()) {
			t.Fatalf("increment %d uploaded %d bytes for a %d-byte database", revision, uploaded, info.Size())
		}
		if status := rep.Status(); status.Generation != generation || !status.Complete || status.PendingPages != 0 {
			t.Fatalf("increment %d status = %+v", revision, status)
		}
	}
	restore("incremental_restore", rep.getMarker().At, rows, 2)
	clock.Add(10 * time.Minute)
	scaleMeasure(t, objects, "compact", func() error { return rep.Sync(ctx) })
	layout, err := rep.loadLayout(ctx, generation)
	if err != nil {
		t.Fatal(err)
	}
	if len(layout.raw) != 0 || len(layout.windows[1]) != 0 || len(layout.windows[2]) != 1 {
		t.Fatalf("expected only a coarse window after expiration: raw=%d L1=%d L2=%d", len(layout.raw), len(layout.windows[1]), len(layout.windows[2]))
	}
	restore("compacted_restore", layout.windows[2][0].End, rows, 2)
	stats := objects.stats()
	t.Logf("bucket totals: reads=%d writes=%d read=%.2f MiB uploaded=%.2f MiB", stats.gets, stats.puts, float64(stats.read)/(1<<20), float64(stats.written)/(1<<20))
}

func scalePayload(size, revision int) []byte {
	data := make([]byte, size)
	state := uint64(revision + 1)
	for offset := 8; offset < len(data); offset += 8 {
		state ^= state << 13
		state ^= state >> 7
		state ^= state << 17
		binary.LittleEndian.PutUint64(data[offset:], state)
	}
	return data
}

func scaleCheckDatabase(path string, count int64, rowBytes, revision int) error {
	db, err := openTestDatabase(path, "")
	if err != nil {
		return err
	}
	defer db.Close()
	var integrity string
	if err := db.db.QueryRow(`PRAGMA integrity_check`).Scan(&integrity); err != nil {
		return err
	}
	if integrity != "ok" {
		return fmt.Errorf("integrity_check: %s", integrity)
	}
	rows, err := db.db.Query(`SELECT id,revision,payload FROM scale_rows ORDER BY id`)
	if err != nil {
		return err
	}
	defer rows.Close()
	patterns := [3][]byte{scalePayload(rowBytes, 0), scalePayload(rowBytes, 1), scalePayload(rowBytes, 2)}
	var seen int64
	for rows.Next() {
		var id int64
		var gotRevision int
		var data []byte
		if err := rows.Scan(&id, &gotRevision, &data); err != nil {
			return err
		}
		seen++
		wantRevision := 0
		if revision >= 1 && id%4 == 0 {
			wantRevision = 1
		}
		if revision >= 2 && id%4 <= 1 {
			wantRevision = 2
		}
		want := patterns[wantRevision]
		binary.LittleEndian.PutUint64(want, uint64(id))
		if id != seen || gotRevision != wantRevision || !bytes.Equal(data, want) {
			return fmt.Errorf("row %d: id=%d revision=%d, expected id=%d revision=%d and matching payload", seen, id, gotRevision, seen, wantRevision)
		}
	}
	if err := rows.Err(); err != nil {
		return err
	}
	if seen != count {
		return fmt.Errorf("restored %d rows, expected %d", seen, count)
	}
	return nil
}

// scaleMeasure samples the process Go heap while a phase runs. SQLite's WASM
// runtime, other tests, and GC affect these observations; use -run with this
// test alone for comparisons. Total allocations include reclaimed objects.
func scaleMeasure(t *testing.T, objects *scaleDiskObjects, phase string, fn func() error) {
	t.Helper()
	runtime.GC()
	var before runtime.MemStats
	runtime.ReadMemStats(&before)
	ioBefore := objects.stats()
	stop := make(chan struct{})
	peakResult := make(chan uint64, 1)
	go func() {
		ticker := time.NewTicker(10 * time.Millisecond)
		defer ticker.Stop()
		peak := before.HeapAlloc
		for {
			var current runtime.MemStats
			runtime.ReadMemStats(&current)
			peak = max(peak, current.HeapAlloc)
			select {
			case <-stop:
				peakResult <- peak
				return
			case <-ticker.C:
			}
		}
	}()
	started := time.Now()
	err := fn()
	elapsed := time.Since(started)
	close(stop)
	peak := <-peakResult
	var after runtime.MemStats
	runtime.ReadMemStats(&after)
	peak = max(peak, after.HeapAlloc)
	ioAfter := objects.stats()
	t.Logf("%s: elapsed=%s heap_base=%.2f MiB heap_peak=%.2f MiB heap_growth=%.2f MiB allocated=%.2f MiB gets=%d puts=%d read=%.2f MiB uploaded=%.2f MiB", phase, elapsed.Round(time.Millisecond), float64(before.HeapAlloc)/(1<<20), float64(peak)/(1<<20), float64(peak-before.HeapAlloc)/(1<<20), float64(after.TotalAlloc-before.TotalAlloc)/(1<<20), ioAfter.gets-ioBefore.gets, ioAfter.puts-ioBefore.puts, float64(ioAfter.read-ioBefore.read)/(1<<20), float64(ioAfter.written-ioBefore.written)/(1<<20))
	if err != nil {
		t.Fatalf("%s: %v", phase, err)
	}
}

type scaleObjectStats struct {
	gets, puts, read, written uint64
}

// scaleDiskObjects retains only object metadata in RAM. Atomic rename and the
// lock implement ObjectStore's replacement and consistency requirements.
type scaleDiskObjects struct {
	mu       sync.Mutex
	dir      string
	counters scaleObjectStats
}

func (b *scaleDiskObjects) stats() scaleObjectStats {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.counters
}

func (b *scaleDiskObjects) Put(ctx context.Context, key string, data []byte) error {
	b.mu.Lock()
	defer b.mu.Unlock()
	if err := ctx.Err(); err != nil {
		return err
	}
	path := filepath.Join(b.dir, filepath.FromSlash(key))
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	file, err := os.CreateTemp(filepath.Dir(path), ".put-")
	if err != nil {
		return err
	}
	defer os.Remove(file.Name())
	_, writeErr := file.Write(data)
	if err := errors.Join(writeErr, file.Close()); err != nil {
		return err
	}
	if err := os.Rename(file.Name(), path); err != nil {
		return err
	}
	b.counters.puts++
	b.counters.written += uint64(len(data))
	return nil
}

func (b *scaleDiskObjects) Get(ctx context.Context, key string) ([]byte, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	data, err := os.ReadFile(filepath.Join(b.dir, filepath.FromSlash(key)))
	if errors.Is(err, os.ErrNotExist) {
		return nil, ErrNotFound
	}
	if err != nil {
		return nil, err
	}
	b.counters.gets++
	b.counters.read += uint64(len(data))
	return data, nil
}

func (b *scaleDiskObjects) Delete(ctx context.Context, key string) error {
	b.mu.Lock()
	defer b.mu.Unlock()
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := os.Remove(filepath.Join(b.dir, filepath.FromSlash(key))); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return nil
}

func (b *scaleDiskObjects) List(ctx context.Context, prefix string) ([]Object, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	var objects []Object
	err := filepath.WalkDir(b.dir, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if err := ctx.Err(); err != nil {
			return err
		}
		if entry.IsDir() {
			return nil
		}
		relative, err := filepath.Rel(b.dir, path)
		if err != nil {
			return err
		}
		key := filepath.ToSlash(relative)
		if !strings.HasPrefix(key, prefix) {
			return nil
		}
		info, err := entry.Info()
		if err != nil {
			return err
		}
		objects = append(objects, Object{Key: key, Size: info.Size()})
		return nil
	})
	return objects, err
}
