package replica

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

func TestRenewalWithUnknownCurrentOutcome(t *testing.T) {
	for _, landed := range []bool{false, true} {
		for _, restart := range []bool{false, true} {
			t.Run(fmt.Sprintf("landed=%t/restart=%t", landed, restart), func(t *testing.T) {
				f := newFixture(t)
				f.write(t, []byte("before"))
				f.sync(t)
				f.clock.Add(time.Minute)
				f.rep.config.Generation = time.Nanosecond
				f.write(t, []byte("snapshot"))
				attempted := false
				f.objects.setHook(func(method, key string, data []byte) error {
					if key != f.rep.currentKey() {
						return nil
					}
					if method == "put" {
						attempted = true
						if landed {
							f.objects.mu.Lock()
							f.objects.data[key] = append([]byte(nil), data...)
							f.objects.mu.Unlock()
						}
						return errInjected
					}
					if method == "get" && attempted {
						return errInjected
					}
					return nil
				})
				if err := f.rep.Sync(context.Background()); !errors.Is(err, errInjected) || !attempted {
					t.Fatalf("publication fault: attempted=%v, err=%v", attempted, err)
				}
				f.objects.setHook(nil)
				pending := f.rep.getMarker()
				if pending.Complete || pending.Previous == nil {
					t.Fatalf("unknown publication lost its recovery marker: %+v", pending)
				}
				f.rep.config.Generation = 7 * 24 * time.Hour
				f.write(t, []byte("after the uncertain publication"))
				if restart {
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
				if landed {
					// Resolving a published snapshot must continue it, without
					// uploading another complete copy of the database.
					f.objects.setHook(func(method, key string, _ []byte) error {
						if method == "put" && strings.HasSuffix(key, "/snapshot") {
							return errors.New("unexpected replacement snapshot")
						}
						return nil
					})
				}
				f.sync(t)
				current, err := f.objects.Get(context.Background(), f.rep.currentKey())
				if err != nil || string(current) != f.rep.getMarker().Generation {
					t.Fatalf("current=%q, marker=%+v: %v", current, f.rep.getMarker(), err)
				}
				if landed && string(current) != pending.Generation {
					t.Fatal("the published generation was abandoned")
				}
				f.check(t, string(current), time.Time{}, []byte("after the uncertain publication"))
			})
		}
	}
}

func TestInvalidatedGenerationCannotFallBackToItsAncestor(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("ancestor"))
	f.sync(t)
	ancestor := f.rep.getMarker()
	f.clock.Add(time.Minute)
	f.rep.config.Generation = time.Nanosecond
	f.write(t, []byte("renewed"))
	f.sync(t)
	f.rep.config.Generation = 7 * 24 * time.Hour
	current := f.rep.getMarker()
	if current.Previous != nil {
		t.Fatal("a completed renewal retained its ancestor as a fallback")
	}
	// Model an already-persisted marker from a pre-fix build.
	current.Previous = &ancestor
	if err := f.rep.setMarker(current); err != nil {
		t.Fatal(err)
	}
	plain, err := openTestDatabase(f.rep.path, "")
	if err != nil {
		t.Fatal(err)
	}
	if err := plain.WriteFile("state", []byte("foreign")); err != nil {
		t.Fatal(err)
	}
	plain.Close()
	if err := f.rep.Sync(context.Background()); !errors.Is(err, errForeignWrite) {
		t.Fatal(err)
	}
	failPut(func(key string) bool { return strings.HasSuffix(key, ".seg") }, false)(f)
	if err := f.rep.Sync(context.Background()); !errors.Is(err, errInjected) {
		t.Fatal(err)
	}
	disarm(f)
	if f.rep.getMarker().Complete || f.rep.getMarker().Previous != nil {
		t.Fatal("failed repair resumed an ancestor whose pages are no longer tracked")
	}
	f.sync(t)
	f.check(t, f.rep.getMarker().Generation, time.Time{}, []byte("foreign"))
}
