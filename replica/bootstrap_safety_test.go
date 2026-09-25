package replica

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/ncruces/go-sqlite3/vfs"
)

func TestPrepareRefusesEmptyBootstrapWhenCurrentIsMissing(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("the archived database"))
	f.sync(t)
	generation := f.rep.Status().Generation
	if err := f.db.Close(); err != nil {
		t.Fatal(err)
	}
	f.rep.Close()
	ctx := context.Background()
	if err := f.objects.Delete(ctx, f.rep.currentKey()); err != nil {
		t.Fatal(err)
	}
	f.objects.mu.Lock()
	before := make(map[string][]byte, len(f.objects.data))
	for key, data := range f.objects.data {
		before[key] = bytes.Clone(data)
	}
	f.objects.mu.Unlock()
	path := filepath.Join(t.TempDir(), "restored.db")
	restored, err := NewWithOptions(f.rep.config, t.Name(), path, vfs.Find(""), nil, Options{Objects: f.objects, Now: f.clock.Now})
	if err != nil {
		t.Fatal(err)
	}
	defer restored.Close()
	err = restored.Prepare(ctx)
	if err == nil || !strings.Contains(err.Error(), "current") || !strings.Contains(err.Error(), "empty") {
		t.Fatalf("missing current with an archived snapshot allowed empty bootstrap: %v", err)
	}
	if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("failed bootstrap created its target: %v", err)
	}
	f.objects.mu.Lock()
	unchanged := len(before) == len(f.objects.data)
	for key, data := range before {
		unchanged = unchanged && bytes.Equal(data, f.objects.data[key])
	}
	f.objects.mu.Unlock()
	if !unchanged {
		t.Fatal("refusing the empty bootstrap changed the archive")
	}
	// An operator restores the known pointer. Retry the same prepared replica:
	// the failed attempt must not leave local state that prevents recovery.
	if err := f.objects.Put(ctx, restored.currentKey(), []byte(generation)); err != nil {
		t.Fatal(err)
	}
	if err := restored.Prepare(ctx); err != nil {
		t.Fatal(err)
	}
	if !restored.Status().Restored {
		t.Fatal("repairing current did not restore the archived database")
	}
	db, err := openTestDatabase(path, restored.VFSName())
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if data, err := db.ReadFile("state"); err != nil || string(data) != "the archived database" {
		t.Fatalf("restored state = %q: %v", data, err)
	}
}

func TestPrepareEmptyBootstrapRequiresAnEmptyArchive(t *testing.T) {
	for _, listingFails := range []bool{false, true} {
		name := "empty"
		if listingFails {
			name = "list-failed"
		}
		t.Run(name, func(t *testing.T) {
			objects := newMemoryObjects()
			if listingFails {
				objects.setHook(func(method, _ string, _ []byte) error {
					if method == "list" {
						return errInjected
					}
					return nil
				})
			}
			path := filepath.Join(t.TempDir(), "new.db")
			rep, err := NewWithOptions(Config{}, t.Name(), path, vfs.Find(""), nil, Options{Objects: objects})
			if err != nil {
				t.Fatal(err)
			}
			defer rep.Close()
			err = rep.Prepare(context.Background())
			if listingFails {
				if !errors.Is(err, errInjected) {
					t.Fatalf("Prepare ignored the archive listing failure: %v", err)
				}
				if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
					t.Fatalf("failed bootstrap created its target: %v", err)
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			db, err := openTestDatabase(path, rep.VFSName())
			if err != nil {
				t.Fatal(err)
			}
			defer db.Close()
			rep.Attach(db)
			if err := db.WriteFile("state", []byte("first start")); err != nil {
				t.Fatal(err)
			}
			if err := rep.Sync(context.Background()); err != nil {
				t.Fatal(err)
			}
			if status := rep.Status(); !status.Complete || status.Restored {
				t.Fatalf("initial database status = %+v", status)
			}
		})
	}
}

func TestPrepareMissingCurrentPreservesInterruptedRestore(t *testing.T) {
	objects := newMemoryObjects()
	// The interrupted-restore error takes precedence over deciding whether
	// the archive is empty; never reinterpret an intent as a new database.
	objects.setHook(func(method, _ string, _ []byte) error {
		if method == "list" {
			return errInjected
		}
		return nil
	})
	path := filepath.Join(t.TempDir(), "interrupted.db")
	intent := path + ".replica-restoring"
	if err := os.WriteFile(intent, []byte("restore in progress"), 0o600); err != nil {
		t.Fatal(err)
	}
	rep, err := NewWithOptions(Config{}, t.Name(), path, vfs.Find(""), nil, Options{Objects: objects})
	if err != nil {
		t.Fatal(err)
	}
	defer rep.Close()
	if err := rep.Prepare(context.Background()); err == nil || !strings.Contains(err.Error(), "interrupted restore") {
		t.Fatalf("interrupted restore was treated as empty bootstrap: %v", err)
	}
	if data, err := os.ReadFile(intent); err != nil || string(data) != "restore in progress" {
		t.Fatalf("restore intent was modified: %q %v", data, err)
	}
}
