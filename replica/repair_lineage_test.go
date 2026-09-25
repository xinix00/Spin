package replica

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

func TestRepairPreservesLineageAfterUncertainPublication(t *testing.T) {
	for _, restart := range []bool{false, true} {
		t.Run(fmt.Sprintf("restart_before_next_repair=%t", restart), func(t *testing.T) {
			f, original, _, increment, want := selfHealFixture(t)
			f.objects.Delete(context.Background(), increment.Parts[0].Key)
			f.clock.Add(20 * time.Minute)
			if err := f.rep.Sync(context.Background()); !errors.Is(err, errReplicaCorrupt) {
				t.Fatalf("damage was not detected: %v", err)
			}
			var published atomic.Bool
			f.objects.setHook(func(method, key string, data []byte) error {
				if key != f.rep.currentKey() {
					return nil
				}
				if method == "put" {
					f.objects.mu.Lock()
					f.objects.data[key] = bytes.Clone(data)
					f.objects.mu.Unlock()
					published.Store(true)
					return errInjected
				}
				if method == "get" && published.Load() {
					return errInjected
				}
				return nil
			})
			if err := f.rep.Sync(context.Background()); !errors.Is(err, errInjected) || !published.Load() {
				t.Fatalf("did not inject an uncertain repair publication: %v", err)
			}
			attempt := f.rep.getMarker()
			if attempt.Complete || attempt.RepairFrom != original {
				t.Fatalf("repair lost original lineage: %+v", attempt)
			}
			f.objects.setHook(nil)
			if err := f.objects.Put(context.Background(), f.rep.snapshotKey(attempt.Generation), []byte("broken snapshot metadata")); err != nil {
				t.Fatal(err)
			}
			want = append(bytes.Clone(want), []byte(" after uncertain publication")...)
			f.write(t, want)
			if restart {
				restartReviewFixture(t, f)
			}
			failPut(func(key string) bool { return strings.HasSuffix(key, ".seg") }, false)(f)
			if err := f.rep.Sync(context.Background()); !errors.Is(err, errInjected) {
				t.Fatalf("next replacement did not reach its upload: %v", err)
			}
			pending := f.rep.getMarker()
			if pending.Generation == attempt.Generation || pending.RepairFrom != attempt.Generation || pending.Previous != nil || pending.Complete {
				t.Fatalf("replacement lost the newly published damaged lineage: %+v", pending)
			}
			disarm(f)
			restartReviewFixture(t, f)
			f.sync(t)
			current := f.rep.getMarker()
			if !current.Complete || current.RepairFrom != "" || current.Previous != nil {
				t.Fatalf("repair did not finish and clear its intent: %+v", current)
			}
			checkSelfHealSource(t, f, want)
			f.check(t, current.Generation, time.Time{}, want)
		})
	}
}

func TestRepairRefusesAnUnrelatedGeneration(t *testing.T) {
	for _, healthy := range []bool{false, true} {
		t.Run(fmt.Sprintf("healthy=%t", healthy), func(t *testing.T) {
			f, original, key, _, want := selfHealFixture(t)
			f.objects.Put(context.Background(), key, []byte("broken manifest"))
			f.clock.Add(20 * time.Minute)
			if err := f.rep.Sync(context.Background()); !errors.Is(err, errReplicaCorrupt) {
				t.Fatalf("damage was not detected: %v", err)
			}
			failPut(func(key string) bool { return strings.HasSuffix(key, ".seg") }, false)(f)
			if err := f.rep.Sync(context.Background()); !errors.Is(err, errInjected) {
				t.Fatal(err)
			}
			disarm(f)
			if f.rep.getMarker().RepairFrom != original {
				t.Fatal("fixture did not retain repair provenance")
			}
			// An older ID must not imply ownership: clocks can move backwards and
			// another lineage can have an earlier timestamp.
			foreign := newGenerationID(f.clock.Now().Add(-time.Hour))
			if healthy {
				snapshot, err := f.rep.getManifest(context.Background(), original, f.rep.snapshotKey(original))
				if err != nil {
					t.Fatal(err)
				}
				for index, part := range snapshot.Parts {
					data, err := f.objects.Get(context.Background(), part.Key)
					if err != nil {
						t.Fatal(err)
					}
					part.Key = fmt.Sprintf("%sdata/foreign/%d.seg", f.rep.generationPrefix(foreign), index)
					if err := f.objects.Put(context.Background(), part.Key, data); err != nil {
						t.Fatal(err)
					}
					snapshot.Parts[index] = part
				}
				if err := f.rep.putManifest(context.Background(), f.rep.snapshotKey(foreign), snapshot); err != nil {
					t.Fatal(err)
				}
			}
			if err := f.objects.Put(context.Background(), f.rep.currentKey(), []byte(foreign)); err != nil {
				t.Fatal(err)
			}
			checkSelfHealSource(t, f, want)
			f.db.Close()
			f.rep.Close()
			before, err := os.ReadFile(f.rep.path)
			if err != nil {
				t.Fatal(err)
			}
			f.rep, err = NewWithOptions(f.rep.config, f.rep.domain, f.rep.path, vfs.Find(""), nil, Options{Objects: f.objects, Now: f.clock.Now})
			if err != nil {
				t.Fatal(err)
			}
			if err := f.rep.Prepare(context.Background()); err == nil {
				t.Fatal("repair adopted an unrelated generation without proof")
			}
			pointer, err := f.objects.Get(context.Background(), f.rep.currentKey())
			if err != nil || string(pointer) != foreign {
				t.Fatalf("repair overwrote the unrelated bucket pointer: %q, %v", pointer, err)
			}
			after, err := os.ReadFile(f.rep.path)
			if err != nil || !bytes.Equal(before, after) {
				t.Fatalf("refused repair changed the local database: %v", err)
			}
		})
	}
}

func TestDamagedUncertainCommitStartsRepair(t *testing.T) {
	f, generation, _, _, want := selfHealFixture(t)
	want = append(bytes.Clone(want), []byte(" another commit")...)
	f.write(t, want)
	f.objects.setHook(func(method, key string, _ []byte) error {
		if method == "put" && strings.Contains(key, "/L0/") {
			f.objects.mu.Lock()
			f.objects.data[key] = []byte("damaged accepted commit")
			f.objects.mu.Unlock()
			return errInjected
		}
		return nil
	})
	if err := f.rep.Sync(context.Background()); !errors.Is(err, errInjected) {
		t.Fatal(err)
	}
	f.objects.setHook(nil)
	if f.rep.getMarker().Uncertain == 0 {
		t.Fatal("fixture has no uncertain commit")
	}
	if err := f.rep.Sync(context.Background()); !errors.Is(err, errReplicaCorrupt) {
		t.Fatalf("uncertain damaged commit was not detected: %v", err)
	}
	if f.rep.getMarker().Complete || !f.rep.SnapshotDue() {
		t.Fatal("damaged uncertain commit did not arm repair")
	}
	f.sync(t)
	if f.rep.getMarker().Generation == generation {
		t.Fatal("damaged generation was resumed")
	}
	f.check(t, f.rep.getMarker().Generation, time.Time{}, want)
}

func TestBackgroundSyncRepairsDamagedGeneration(t *testing.T) {
	f, generation, _, increment, want := selfHealFixture(t)
	if err := f.objects.Delete(context.Background(), increment.Parts[0].Key); err != nil {
		t.Fatal(err)
	}
	f.clock.Add(20 * time.Minute)
	f.rep.config.Interval = 10 * time.Millisecond
	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()
	f.rep.Start(ctx)
	deadline := time.NewTimer(5 * time.Second)
	defer deadline.Stop()
	poll := time.NewTicker(5 * time.Millisecond)
	defer poll.Stop()
	for {
		select {
		case <-deadline.C:
			t.Fatalf("background loop did not repair damage: %+v", f.rep.Status())
		case <-poll.C:
			status := f.rep.Status()
			if status.Generation == generation || !status.Complete || status.LastError != "" {
				continue
			}
			cancel()
			if status.PendingPages != 0 || status.LastSyncAt.IsZero() {
				t.Fatalf("background repair left an unhealthy status: %+v", status)
			}
			checkSelfHealSource(t, f, want)
			f.check(t, status.Generation, time.Time{}, want)
			return
		}
	}
}
