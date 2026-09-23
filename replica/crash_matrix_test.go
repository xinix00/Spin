package replica

import (
	"context"
	"fmt"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

// crashPoint is one place a sync can stop: a request to the bucket that
// fails or whose reply is lost, or a local write that fails.
type crashPoint struct {
	name    string
	renewal bool // the sync renews the generation
	// after is what the generation is after the failure: "same" for the one
	// the sync worked on, "renewed" for the new one a renewal made.
	after string
	arm   func(f *fixture) // injects the fault for the next sync
}

func failPut(match func(key string) bool, lost bool) func(f *fixture) {
	return func(f *fixture) {
		var fired atomic.Bool // segments travel two at a time
		f.objects.setHook(func(method, key string, data []byte) error {
			if method != "put" || !match(key) || !fired.CompareAndSwap(false, true) {
				return nil
			}
			if lost {
				f.objects.mu.Lock()
				f.objects.data[key] = append([]byte(nil), data...)
				f.objects.mu.Unlock()
			}
			return errInjected
		})
	}
}

// failLocal fails the nth creation of a local file whose name matches.
func failLocal(match func(path string) bool, nth int) func(f *fixture) {
	return func(f *fixture) {
		count := 0
		fault := faultStorage{Storage: OSStorage(), open: func(path string, create bool) error {
			if !create || !match(path) {
				return nil
			}
			count++
			if count == nth {
				return errInjected
			}
			return nil
		}}
		f.rep.files = fault
		f.rep.tracker.log.mu.Lock()
		f.rep.tracker.log.files = fault
		f.rep.tracker.log.mu.Unlock()
	}
}

func disarm(f *fixture) {
	f.objects.setHook(nil)
	f.rep.files = OSStorage()
	f.rep.tracker.log.mu.Lock()
	f.rep.tracker.log.files = OSStorage()
	f.rep.tracker.log.mu.Unlock()
}

var crashPoints = []crashPoint{
	{name: "segment PUT fails", after: "same", arm: failPut(func(k string) bool { return strings.HasSuffix(k, ".seg") }, false)},
	{name: "commit PUT fails", after: "same", arm: failPut(func(k string) bool { return strings.Contains(k, "/L0/") }, false)},
	{name: "commit PUT reply lost", after: "same", arm: failPut(func(k string) bool { return strings.Contains(k, "/L0/") }, true)},
	{name: "marker write after the commit fails", after: "same", arm: failLocal(func(p string) bool { return strings.HasSuffix(p, ".replica") }, 1)},
	{name: "dirty log rewrite after the commit fails", after: "same", arm: failLocal(func(p string) bool { return strings.Contains(p, ".replica-dirty-") }, 1)},
	{name: "clean marker write fails", after: "same", arm: failLocal(func(p string) bool { return strings.HasSuffix(p, ".replica") }, 2)},
	{name: "compaction PUT fails", after: "same", arm: failPut(func(k string) bool { return strings.Contains(k, "/L1/") }, false)},
	{name: "renewal segment PUT fails", renewal: true, after: "same", arm: failPut(func(k string) bool { return strings.HasSuffix(k, ".seg") }, false)},
	{name: "renewal snapshot PUT fails", renewal: true, after: "same", arm: failPut(func(k string) bool { return strings.HasSuffix(k, "/snapshot") }, false)},
	{name: "renewal snapshot PUT reply lost", renewal: true, after: "same", arm: failPut(func(k string) bool { return strings.HasSuffix(k, "/snapshot") }, true)},
	{name: "renewal current PUT fails", renewal: true, after: "same", arm: failPut(func(k string) bool { return strings.HasSuffix(k, "/current") }, false)},
	{name: "renewal marker write after the commit fails", renewal: true, after: "renewed", arm: failLocal(func(p string) bool { return strings.HasSuffix(p, ".replica") }, 2)},
}

// Every place a sync can stop, followed by either a retry in the same
// process or a crash and a start. Whatever the place: the database here keeps
// every write, nothing is restored over it, no whole-database copy follows,
// and after one more sync the bucket restores exactly what the database holds.
func TestCrashMatrix(t *testing.T) {
	for _, point := range crashPoints {
		for _, restart := range []bool{false, true} {
			mode := "retry"
			if restart {
				mode = "restart"
			}
			t.Run(point.name+"/"+mode, func(t *testing.T) {
				f := newFixture(t)
				f.write(t, []byte("one"))
				f.sync(t)
				f.clock.Add(time.Minute)
				f.write(t, []byte("two"))
				f.sync(t)
				before := f.rep.getMarker()
				// Past a window, so the next sync compacts.
				f.clock.Add(20 * time.Minute)
				if point.renewal {
					f.rep.config.Generation = time.Nanosecond
				}
				f.write(t, []byte("three"))
				point.arm(f)
				syncErr := f.rep.Sync(context.Background())
				disarm(f)
				f.rep.config.Generation = Config{}.withDefaults().Generation
				if syncErr == nil && !strings.Contains(point.name, "dirty log") {
					t.Fatal("the fault never fired")
				}
				failed := f.rep.getMarker()
				f.write(t, []byte("four"))

				if restart {
					// The process dies: no sync, no clean stop of anything
					// but the file handles.
					if err := f.db.Close(); err != nil {
						t.Fatal(err)
					}
					f.rep.Close()
					config := Config{SegmentBytes: 4096, Schedule: []Level{{Window: 15 * time.Minute, Keep: 2 * time.Hour}, {Window: time.Hour, Keep: 24 * time.Hour}}}
					var err error
					f.rep, err = NewWithOptions(config, t.Name(), f.dir+"/source.db", vfs.Find(""), nil, Options{Objects: f.objects, Now: f.clock.Now})
					if err != nil {
						t.Fatal(err)
					}
					if err := f.rep.Prepare(context.Background()); err != nil {
						t.Fatalf("start after the crash: %v", err)
					}
					if f.db, err = openTestDatabase(f.rep.path, f.rep.VFSName()); err != nil {
						t.Fatal(err)
					}
					f.rep.Attach(f.db)
					if f.rep.Status().Restored {
						t.Fatal("the start restored the bucket over the database")
					}
				}
				if state, err := f.db.ReadFile("state"); err != nil || string(state) != "four" {
					t.Fatalf("the database holds %q (%v), not the last write", state, err)
				}
				if reason := f.rep.SnapshotReason(); reason != "" {
					t.Fatalf("a whole-database copy follows: %s (marker %+v)", reason, f.rep.getMarker())
				}
				f.clock.Add(time.Minute)
				f.sync(t)
				after := f.rep.getMarker()
				want := before.Generation
				if point.after == "renewed" {
					want = failed.Generation
				}
				if after.Generation != want {
					t.Fatalf("generation after = %s, want %s (%s)", after.Generation, want, describeGenerations(before, failed, after))
				}
				f.check(t, after.Generation, time.Time{}, []byte("four"))
			})
		}
	}
}

func describeGenerations(before, failed, after marker) string {
	return fmt.Sprintf("before %s@%d, at the failure %s@%d complete=%v, after %s@%d", before.Generation, before.Seq, failed.Generation, failed.Seq, failed.Complete, after.Generation, after.Seq)
}
