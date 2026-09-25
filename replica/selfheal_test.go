package replica

import (
	"bytes"
	"context"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func selfHealFixture(t *testing.T) (*fixture, string, string, manifest, []byte) {
	t.Helper()
	f := newFixture(t)
	f.write(t, bytes.Repeat([]byte("snapshot"), 4<<10))
	f.sync(t)
	f.clock.Add(time.Minute)
	want := bytes.Repeat([]byte("current source value"), 4<<10)
	f.write(t, want)
	f.sync(t)
	current := f.rep.getMarker()
	key := f.rep.rawKey(current.Generation, current.Seq, current.At)
	increment, err := f.rep.getManifest(context.Background(), current.Generation, key)
	if err != nil || current.Seq != 2 || len(increment.Parts) == 0 {
		t.Fatalf("fixture did not commit an increment: seq=%d, parts=%d, err=%v", current.Seq, len(increment.Parts), err)
	}
	return f, current.Generation, key, increment, want
}

func checkSelfHealSource(t *testing.T, f *fixture, want []byte) {
	t.Helper()
	got, err := f.db.ReadFile("state")
	if err != nil || !bytes.Equal(got, want) {
		t.Fatalf("source changed during replica repair: got=%d bytes, want=%d, err=%v", len(got), len(want), err)
	}
}

// A committed generation with damaged bucket data cannot become healthy by
// retrying compaction. The source must replace it, including when the first
// replacement upload fails and ordinary renewal would fall back.
func TestCompactionDamageStartsFreshGenerationWithoutUnsafeFallback(t *testing.T) {
	for _, damage := range []string{"missing_part", "missing_part_clock_rollback", "checksum", "malformed_manifest", "missing_snapshot_manifest"} {
		t.Run(damage, func(t *testing.T) {
			f, generation, key, increment, want := selfHealFixture(t)
			f.objects.mu.Lock()
			switch damage {
			case "missing_part", "missing_part_clock_rollback":
				delete(f.objects.data, increment.Parts[0].Key)
			case "checksum":
				part := bytes.Clone(f.objects.data[increment.Parts[0].Key])
				part[len(part)-1] ^= 0xff
				f.objects.data[increment.Parts[0].Key] = part
			case "malformed_manifest":
				f.objects.data[key] = []byte(`{"version":`)
			case "missing_snapshot_manifest":
				delete(f.objects.data, f.rep.snapshotKey(generation))
			}
			f.objects.mu.Unlock()
			f.clock.Add(20 * time.Minute)
			if err := f.rep.Sync(context.Background()); err == nil {
				t.Fatal("compaction did not report damaged committed data")
			}
			current := f.rep.getMarker()
			if current.Complete || !f.rep.SnapshotDue() || current.Previous != nil {
				t.Fatalf("damaged generation stayed eligible for increments or fallback: complete=%v, snapshot_due=%v, previous=%v", current.Complete, f.rep.SnapshotDue(), current.Previous != nil)
			}
			checkSelfHealSource(t, f, want)
			if damage == "missing_part_clock_rollback" {
				f.clock.Add(-24 * time.Hour)
			}

			var uploadFailed atomic.Bool
			f.objects.setHook(func(method, key string, _ []byte) error {
				if method == "put" && strings.HasSuffix(key, ".seg") && uploadFailed.CompareAndSwap(false, true) {
					return errInjected
				}
				return nil
			})
			if err := f.rep.Sync(context.Background()); err == nil || !uploadFailed.Load() {
				t.Fatalf("first repair upload did not fail as intended: %v", err)
			}
			current = f.rep.getMarker()
			if current.Complete || current.Generation == generation || current.Previous != nil || !f.rep.SnapshotDue() {
				t.Fatalf("failed repair fell back to the damaged generation: current=%q, damaged=%q, complete=%v, previous=%v, snapshot_due=%v", current.Generation, generation, current.Complete, current.Previous != nil, f.rep.SnapshotDue())
			}
			checkSelfHealSource(t, f, want)

			f.objects.setHook(nil)
			restartReviewFixture(t, f)
			current = f.rep.getMarker()
			if current.Complete || current.Generation == generation || current.Previous != nil || !f.rep.SnapshotDue() {
				t.Fatalf("restart resumed the damaged generation after failed repair: current=%q, damaged=%q, complete=%v, previous=%v, snapshot_due=%v", current.Generation, generation, current.Complete, current.Previous != nil, f.rep.SnapshotDue())
			}
			checkSelfHealSource(t, f, want)
			f.sync(t)
			current = f.rep.getMarker()
			if !current.Complete || current.Generation == generation || f.rep.SnapshotDue() {
				t.Fatalf("retry did not publish a healthy replacement: current=%q, damaged=%q, complete=%v, snapshot_due=%v", current.Generation, generation, current.Complete, f.rep.SnapshotDue())
			}
			pointer, err := f.objects.Get(context.Background(), f.rep.currentKey())
			if err != nil || string(pointer) != current.Generation {
				t.Fatalf("replacement was not made current: pointer=%q, want=%q, err=%v", pointer, current.Generation, err)
			}
			checkSelfHealSource(t, f, want)
			f.check(t, current.Generation, time.Time{}, want)
			points, err := f.rep.Points(context.Background())
			if err != nil {
				t.Fatalf("old damaged metadata still blocks the repaired backup listing: %v", err)
			}
			listed := false
			for _, point := range points {
				if point.Generation == current.Generation && point.Current {
					listed = true
				}
			}
			if !listed {
				t.Fatal("backup listing omits the repaired current generation")
			}
		})
	}
}

// Transport and service errors say nothing about data integrity. Replacing
// a large database on every failed GET or cleanup request would turn a
// recoverable outage into an expensive full-copy loop.
func TestTransientCompactionErrorsKeepGenerationForRetry(t *testing.T) {
	for _, operation := range []string{"part_get", "manifest_list", "part_put", "manifest_delete"} {
		t.Run(operation, func(t *testing.T) {
			f, generation, rawKey, increment, want := selfHealFixture(t)
			f.clock.Add(20 * time.Minute)
			var failed atomic.Bool
			f.objects.setHook(func(method, key string, _ []byte) error {
				matches := false
				switch operation {
				case "part_get":
					matches = method == "get" && key == increment.Parts[0].Key
				case "manifest_list":
					matches = method == "list" && key == f.rep.generationPrefix(generation)+"L"
				case "part_put":
					matches = method == "put" && strings.HasSuffix(key, ".seg")
				case "manifest_delete":
					matches = method == "delete" && key == rawKey
				}
				if matches && failed.CompareAndSwap(false, true) {
					return errInjected
				}
				return nil
			})
			if err := f.rep.Sync(context.Background()); err == nil || !failed.Load() {
				t.Fatalf("transient %s was not injected: %v", operation, err)
			}
			current := f.rep.getMarker()
			if !current.Complete || current.Generation != generation || f.rep.SnapshotDue() {
				t.Fatalf("transient %s invalidated healthy data: generation=%q, complete=%v, snapshot_due=%v", operation, current.Generation, current.Complete, f.rep.SnapshotDue())
			}
			f.objects.setHook(nil)
			f.check(t, generation, time.Time{}, want)
			f.clock.Add(time.Minute)
			f.sync(t)
			current = f.rep.getMarker()
			if !current.Complete || current.Generation != generation {
				t.Fatalf("retry replaced a healthy generation: generation=%q, want=%q, complete=%v", current.Generation, generation, current.Complete)
			}
			checkSelfHealSource(t, f, want)
			f.check(t, generation, time.Time{}, want)
		})
	}
}
