package replica

import (
	"context"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

// A conservative log may replay already-committed pages after a clean stop.
// That replay must not leave a clean marker beside an unclean tracker: the
// next real write would otherwise skip durable invalidation of the marker.
func TestDirtyLogReplayCannotHideLaterWrites(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("one"))
	f.sync(t)
	f.write(t, []byte("two"))
	failLocal(func(path string) bool { return strings.Contains(path, ".replica-dirty-") }, 1)(f)
	f.sync(t) // commit succeeded; log rewrite failed, so the older log survives
	disarm(f)
	if !f.rep.getMarker().Clean {
		t.Fatal("test needs a clean marker with a conservative dirty log")
	}
	generation := f.rep.getMarker().Generation
	reopen := func() {
		t.Helper()
		if err := f.db.Close(); err != nil {
			t.Fatal(err)
		}
		f.rep.Close()
		var err error
		f.rep, err = NewWithOptions(f.rep.config, t.Name(), f.rep.path, vfs.Find(""), nil, Options{Objects: f.objects, Now: f.clock.Now})
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
	}
	reopen()
	if f.rep.Status().PendingPages == 0 {
		t.Fatal("test did not replay a conservative log")
	}
	f.write(t, []byte("three"))
	stored, err := f.rep.readMarker()
	if err != nil || stored.Clean {
		t.Fatalf("a write after replay left a clean marker: %+v, %v", stored, err)
	}
	// Remove the proof of which pages changed, then restart without syncing.
	for _, path := range f.rep.tracker.log.paths {
		if err := os.WriteFile(path, []byte("damaged"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	reopen()
	if !f.rep.SnapshotDue() {
		t.Fatal("damaged log hid an unshipped write")
	}
	f.sync(t)
	if f.rep.getMarker().Generation == generation {
		t.Fatal("a damaged dirty log continued the old generation")
	}
	f.check(t, f.rep.getMarker().Generation, time.Time{}, []byte("three"))
}
