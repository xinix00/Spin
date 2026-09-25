package replica

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

func restartReviewFixture(t *testing.T, f *fixture) {
	t.Helper()
	if err := f.db.Close(); err != nil {
		t.Fatal(err)
	}
	f.rep.Close()
	var err error
	f.rep, err = NewWithOptions(f.rep.config, f.rep.domain, f.rep.path, vfs.Find(""), nil, Options{Objects: f.objects, Now: f.clock.Now})
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

func TestPrunePreservesDataWhenSnapshotDeleteFails(t *testing.T) {
	f := newFixture(t)
	f.rep.config.Generation = time.Hour
	f.rep.config.Retention = time.Hour
	f.write(t, bytes.Repeat([]byte("source"), 10000))
	f.sync(t)
	old := f.rep.getMarker().Generation
	f.objects.setHook(func(method, key string, _ []byte) error {
		if method == "delete" && key == f.rep.snapshotKey(old) {
			return errInjected
		}
		return nil
	})
	f.clock.Add(2 * time.Hour)
	f.sync(t)
	points, err := f.rep.Points(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	found := false
	for _, p := range points {
		if p.Generation != old {
			continue
		}
		found = true
		err := f.rep.Fetch(context.Background(), p.Generation, p.At, f.dir+"/expired.db")
		if err != nil {
			t.Fatalf("advertised expired point %s lost parts after snapshot DELETE failed: %v", p.At, err)
		}
	}
	if !found {
		t.Fatal("failed snapshot deletion unexpectedly hid the old generation")
	}
}

func TestIdleRestartCannotLowerCompactionFrontier(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("first"))
	f.sync(t)
	f.clock.Add(5 * time.Minute)
	f.write(t, []byte("second"))
	f.sync(t)
	f.clock.Add(time.Hour)
	f.sync(t)
	before := f.rep.getMarker()
	oldPoint := before.SealedAt.Truncate(time.Hour)
	f.check(t, before.Generation, oldPoint, []byte("second"))
	f.clock.Add(-time.Hour)
	restartReviewFixture(t, f)
	f.sync(t)
	if f.rep.getMarker().SealedAt.Before(before.SealedAt) {
		t.Errorf("idle restart lowered sealed frontier from %s to %s", before.SealedAt, f.rep.getMarker().SealedAt)
	}
	f.write(t, []byte("third"))
	f.sync(t)
	f.check(t, before.Generation, oldPoint, []byte("second"))
}

func TestInterruptedRenewalDoesNotDependOnGenerationOrder(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("first"))
	f.sync(t)
	previous := f.rep.getMarker()
	f.clock.Add(-time.Minute)
	pending := f.rep.renewal(previous)
	if !strings.Contains(pending.Generation, "-") || pending.Generation >= previous.Generation {
		t.Fatal("fixture did not use an earlier generation")
	}
	if err := f.rep.setMarker(pending); err != nil {
		t.Fatal(err)
	}
	restartReviewFixture(t, f)
	if f.rep.getMarker().Generation != previous.Generation {
		t.Fatal("did not recover the interrupted renewal")
	}
}

func TestCompactionCannotSealPastDurableFrontier(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("first"))
	f.sync(t)
	f.clock.Add(5 * time.Minute)
	f.write(t, []byte("second"))
	f.sync(t)
	f.clock.Add(54*time.Minute + 29*time.Second) // 08:59:59
	frontier := f.clock.Now()
	fired := false
	f.rep.files = faultStorage{Storage: OSStorage(), wrap: func(path string, file File) File {
		if path != f.rep.markerPath() {
			return file
		}
		return faultFile{File: file, sync: func() error {
			if err := file.Sync(); err != nil {
				return err
			}
			data, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			var m marker
			if err := json.Unmarshal(data, &m); err != nil {
				return err
			}
			if !fired && m.SealedAt.Equal(frontier) {
				fired = true
				f.clock.Add(2 * time.Second) // Slow durable marker write crosses 09:00.
			}
			return nil
		}}
	}}
	f.sync(t)
	f.rep.files = OSStorage()
	if !fired {
		t.Fatal("marker write did not cross the compaction boundary")
	}
	old := f.rep.getMarker()
	points, err := f.rep.Points(context.Background())
	if err != nil || len(points) == 0 {
		t.Fatalf("points = %v, %v", points, err)
	}
	point := points[0].At
	if point.After(old.SealedAt) {
		t.Fatalf("compaction sealed %s past durable frontier %s", point, old.SealedAt)
	}
	f.check(t, old.Generation, point, []byte("second"))
	f.clock.Add(-1500 * time.Millisecond) // 08:59:59.5 is AFTER persisted frontier.
	restartReviewFixture(t, f)
	f.write(t, []byte("third"))
	f.sync(t)
	f.check(t, old.Generation, point, []byte("second"))
}
