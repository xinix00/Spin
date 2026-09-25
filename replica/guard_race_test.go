package replica

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"strings"
	"testing"
	"time"
)

// Hooks straddle acquisition of the real SQLite read transaction. A write in
// before models a writer finishing while the replica waits for the connection;
// it must be included in both the dirty set and the file the guard observes.
type guardedDatabase struct {
	Database
	calls  int
	before func(int)
	after  func(int)
}

func (d *guardedDatabase) WithReadTransaction(ctx context.Context, fn func() error) error {
	d.calls++
	if d.before != nil {
		d.before(d.calls)
	}
	err := d.Database.WithReadTransaction(ctx, fn)
	if d.after != nil {
		d.after(d.calls)
	}
	return err
}

func TestSourceGuardTrackedWritesAtReadBoundaries(t *testing.T) {
	for _, pageSize := range []int{4096, 65536} {
		for _, phase := range []string{"waiting-for-guard", "after-guard", "during-capture", "during-upload"} {
			t.Run(fmt.Sprintf("%d/%s", pageSize, phase), func(t *testing.T) {
				f := newFixture(t)
				if _, err := f.db.db.Exec(fmt.Sprintf("PRAGMA page_size=%d; VACUUM", pageSize)); err != nil {
					t.Fatal(err)
				}
				f.write(t, bytes.Repeat([]byte("a"), 3*pageSize))
				f.sync(t)
				generation := f.rep.getMarker().Generation
				if f.rep.Status().PendingPages != 0 {
					t.Fatal("the race must start with a clean tracker")
				}
				copies := 0
				f.rep.OnCopy = func(CopyProgress) { copies++ }
				want := bytes.Repeat([]byte("b"), 3*pageSize)
				writes := 0
				write := func() { f.write(t, want); writes++ }
				db := &guardedDatabase{Database: f.db}
				switch phase {
				case "waiting-for-guard":
					db.before = func(call int) {
						if call == 1 {
							write()
						}
					}
				case "after-guard":
					db.after = func(call int) {
						if call == 1 {
							write()
						}
					}
				case "during-capture":
					f.write(t, bytes.Repeat([]byte("c"), 3*pageSize))
					db.after = func(call int) {
						if call == 3 { // guard, capture begin, first segment
							write()
						}
					}
				case "during-upload":
					f.write(t, bytes.Repeat([]byte("c"), 3*pageSize))
					// OnCopy is intentionally unused by incremental uploads.
					// One worker makes this fault injection deterministic.
					f.rep.config.UploadParallelism = 1
					f.objects.setHook(func(method, key string, _ []byte) error {
						if method == "put" && writes == 0 {
							if err := f.db.WriteFile("state", want); err != nil {
								return err
							}
							writes++
						}
						return nil
					})
				}
				f.rep.Attach(db)
				f.sync(t)
				f.objects.setHook(nil)
				if writes != 1 {
					t.Fatalf("injected %d writes, want 1", writes)
				}
				f.rep.Attach(f.db)
				f.sync(t) // also checks the witness left by the raced capture
				if copies != 0 || f.rep.getMarker().Generation != generation || f.rep.Status().PendingPages != 0 {
					t.Fatalf("tracked write caused a copy or stopped shipping: copies=%d, status=%+v", copies, f.rep.Status())
				}
				f.check(t, generation, time.Time{}, want)
			})
		}
	}
}

// Exercise the actual connection-pool wait as well as the boundary hooks
// above. No scheduler timing decides the order: WaitCount proves the guard
// is waiting before the held connection commits, rolls back, or is cancelled.
func TestSourceGuardWaitsForWriter(t *testing.T) {
	for _, outcome := range []string{"commit", "rollback", "cancel"} {
		t.Run(outcome, func(t *testing.T) {
			f := newFixture(t)
			before := []byte("before")
			f.write(t, before)
			f.sync(t)
			generation := f.rep.getMarker().Generation
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()
			conn, err := f.db.db.Conn(ctx)
			if err != nil {
				t.Fatal(err)
			}
			defer conn.Close()
			waits := f.db.db.Stats().WaitCount
			done := make(chan error, 1)
			go func() { done <- f.rep.Sync(ctx) }()
			ticker := time.NewTicker(time.Millisecond)
			defer ticker.Stop()
			for f.db.db.Stats().WaitCount == waits {
				select {
				case err := <-done:
					t.Fatalf("sync finished before waiting for the writer: %v", err)
				case <-ctx.Done():
					t.Fatal("guard did not wait for the connection")
				case <-ticker.C:
				}
			}
			want := before
			switch outcome {
			case "commit":
				want = []byte("committed while sync waited")
				if _, err := conn.ExecContext(ctx, `UPDATE spin_kv SET value=? WHERE key='state'`, want); err != nil {
					t.Fatal(err)
				}
			case "rollback":
				// Force uncommitted pages onto the file, then have SQLite undo
				// them through the same tracker before releasing the connection.
				if _, err := conn.ExecContext(ctx, `PRAGMA cache_size=1; PRAGMA cache_spill=ON`); err != nil {
					t.Fatal(err)
				}
				tx, err := conn.BeginTx(ctx, nil)
				if err != nil {
					t.Fatal(err)
				}
				defer tx.Rollback()
				if _, err := tx.ExecContext(ctx, `UPDATE spin_kv SET value=? WHERE key='state'`, bytes.Repeat([]byte("x"), 128<<10)); err != nil {
					t.Fatal(err)
				}
				if f.rep.Status().PendingPages == 0 {
					t.Fatal("rollback test did not spill any pages")
				}
				if err := tx.Rollback(); err != nil {
					t.Fatal(err)
				}
			case "cancel":
				cancel()
				if err := <-done; !errors.Is(err, context.Canceled) {
					t.Fatalf("cancelled guard: %v", err)
				}
			}
			if err := conn.Close(); err != nil {
				t.Fatal(err)
			}
			if outcome != "cancel" {
				if err := <-done; err != nil {
					t.Fatalf("guard after %s: %v", outcome, err)
				}
			}
			f.sync(t)
			if f.rep.getMarker().Generation != generation || !f.rep.getMarker().Complete {
				t.Fatalf("%s invalidated the generation: %+v", outcome, f.rep.getMarker())
			}
			f.check(t, generation, time.Time{}, want)
		})
	}
}

func TestSourceGuardPageSizeChangesWhileWaiting(t *testing.T) {
	f := newFixture(t)
	want := bytes.Repeat([]byte("state"), 4000)
	f.write(t, want)
	f.sync(t)
	generation := f.rep.getMarker().Generation
	db := &guardedDatabase{Database: f.db, before: func(call int) {
		if call == 1 {
			if _, err := f.db.db.Exec(`PRAGMA page_size=8192; VACUUM`); err != nil {
				t.Fatal(err)
			}
		}
	}}
	f.rep.Attach(db)
	f.sync(t)
	after := f.rep.getMarker()
	if after.Generation == generation || after.PageSize != 8192 || !after.Complete {
		t.Fatalf("page size change did not complete its snapshot: %+v", after)
	}
	f.rep.Attach(f.db)
	f.sync(t)
	f.check(t, after.Generation, time.Time{}, want)
}

// Fixing the race must not disable the counter check. Use a real SQLite
// transaction through the unwrapped VFS, with no tracked writes to hide it.
func TestSourceGuardUntrackedCommitWhileWaiting(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("before"))
	f.sync(t)
	generation := f.rep.getMarker().Generation
	want := []byte("outside")
	f.rep.Attach(&guardedDatabase{Database: f.db, before: func(call int) {
		if call != 1 {
			return
		}
		plain, err := openTestDatabase(f.rep.path, "")
		if err != nil {
			t.Fatal(err)
		}
		defer plain.Close()
		if err := plain.WriteFile("state", want); err != nil {
			t.Fatal(err)
		}
		if f.rep.Status().PendingPages != 0 {
			t.Fatal("foreign writer unexpectedly used the tracker")
		}
	}})
	if err := f.rep.Sync(context.Background()); !errors.Is(err, errForeignWrite) {
		t.Fatalf("untracked commit: %v, want errForeignWrite", err)
	}
	if detail := f.rep.Status().LastError; !strings.Contains(detail, "the change counter moved") {
		t.Fatalf("status lost the reason for the foreign-write detection: %q", detail)
	}
	if f.rep.getMarker().Complete {
		t.Fatal("foreign write did not invalidate the generation")
	}
	f.check(t, generation, time.Time{}, []byte("before"))
	f.rep.Attach(f.db)
	f.sync(t)
	if f.rep.getMarker().Generation == generation {
		t.Fatal("foreign write did not start a new generation")
	}
	f.sync(t)
	f.check(t, f.rep.getMarker().Generation, time.Time{}, want)
}
