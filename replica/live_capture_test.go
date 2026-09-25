package replica

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"math/rand/v2"
	"strings"
	"testing"
	"time"
)

// Live snapshots and increments must remain restorable after compaction even
// when transactions change the file's size between segments.
func TestLiveCaptureSizeChangesSurviveCompaction(t *testing.T) {
	for _, change := range []string{"grow", "shrink", "shrink-grow"} {
		t.Run(change, func(t *testing.T) {
			f := newFixture(t)
			f.write(t, bytes.Repeat([]byte("a"), 64<<10))
			f.sync(t)
			generation := f.rep.getMarker().Generation
			f.clock.Add(time.Minute)
			f.write(t, bytes.Repeat([]byte("b"), 64<<10))
			want := bytes.Repeat([]byte("c"), 256<<10)
			db := &guardedDatabase{Database: f.db, after: func(call int) {
				if call == 3 {
					if change != "grow" {
						want = []byte("small")
					}
					f.write(t, want)
					if change != "grow" {
						if _, err := f.db.db.Exec(`VACUUM`); err != nil {
							t.Fatal(err)
						}
					}
				}
				if call == 4 && change == "shrink-grow" {
					want = bytes.Repeat([]byte("d"), 128<<10)
					f.write(t, want)
				}
			}}
			f.rep.Attach(db)
			f.sync(t)
			f.rep.Attach(f.db)
			f.check(t, generation, time.Time{}, want)
			f.clock.Add(3 * time.Hour)
			if err := f.rep.compact(context.Background(), generation, f.clock.Now()); err != nil {
				t.Fatalf("compact a live %s: %v", change, err)
			}
			f.check(t, generation, time.Time{}, want)
			// An idle check after growth must not see a fictitious coverage gap.
			f.sync(t)
			if f.rep.getMarker().Generation != generation {
				t.Fatal("ordinary size changes forced a new generation")
			}
		})
	}
}

func TestLiveCaptureRecordsFinalSize(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("small"))
	f.sync(t)
	f.write(t, []byte("dirty"))
	f.rep.Attach(&guardedDatabase{Database: f.db, after: func(call int) {
		if call == 3 {
			f.write(t, bytes.Repeat([]byte("big"), 10000))
		}
	}})
	f.sync(t)
	size, err := f.rep.fileSize()
	if err != nil {
		t.Fatal(err)
	}
	if got := f.rep.getMarker().Size; got != size {
		t.Fatalf("marker remembers size %d, current committed database is %d", got, size)
	}
	f.rep.Attach(f.db)
	f.sync(t)
}

func TestLiveCaptureCatchupSegmentsStayBounded(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("small"))
	f.sync(t)
	f.write(t, []byte("dirty"))
	want := bytes.Repeat([]byte("large"), 100000)
	f.rep.Attach(&guardedDatabase{Database: f.db, after: func(call int) {
		if call == 3 {
			f.write(t, want)
		}
	}})
	f.objects.setHook(func(method, key string, data []byte) error {
		if method == "put" && strings.HasSuffix(key, ".seg") {
			seg, err := decodeSegment(data)
			if err != nil {
				return err
			}
			if len(seg.Pages) > max(1, f.rep.config.SegmentBytes/seg.PageSize) {
				return fmt.Errorf("catchup segment holds %d pages, limit is %d bytes", len(seg.Pages), f.rep.config.SegmentBytes)
			}
		}
		return nil
	})
	f.sync(t)
	f.objects.setHook(nil)
	f.check(t, f.rep.getMarker().Generation, time.Time{}, want)
}

func TestLiveCaptureFinishesUnderContinuousWrites(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("initial"))
	f.sync(t)
	f.write(t, []byte("pending"))
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	want, captured := []byte("pending"), []byte(nil)
	f.rep.Attach(&guardedDatabase{Database: f.db, after: func(call int) {
		if call >= 3 {
			captured = want
			want = []byte(fmt.Sprintf("write-%d", call))
			f.write(t, want)
		}
		if call == 30 {
			cancel() // deterministic bound, no timing-dependent starvation test
		}
	}})
	if err := f.rep.Sync(ctx); err != nil {
		t.Fatalf("capture never finished while the writer stayed active: %v", err)
	}
	if f.rep.Status().PendingPages == 0 {
		t.Fatal("writes after the capture boundary must remain pending")
	}
	generation := f.rep.getMarker().Generation
	f.check(t, generation, time.Time{}, captured)
	f.rep.Attach(f.db)
	f.sync(t)
	f.check(t, generation, time.Time{}, want)
}

func TestLiveCaptureRejectsPageSizeChangeMidCopy(t *testing.T) {
	for _, phase := range []string{"between-segments", "before-reconciliation"} {
		t.Run(phase, func(t *testing.T) {
			f := newFixture(t)
			before := bytes.Repeat([]byte("before"), 8000)
			f.write(t, before)
			f.sync(t)
			generation := f.rep.getMarker().Generation
			if phase == "before-reconciliation" {
				f.rep.config.SegmentBytes = 1 << 20 // all initial pages in one segment
			}
			want := bytes.Repeat([]byte("after!"), 8000)
			f.write(t, want)
			changed := false
			f.rep.Attach(&guardedDatabase{Database: f.db, after: func(n int) {
				if n == 3 {
					if _, err := f.db.db.Exec(`PRAGMA page_size=8192; VACUUM`); err != nil {
						t.Fatal(err)
					}
					changed = true
				}
			}})
			if err := f.rep.Sync(context.Background()); err == nil || !changed {
				t.Fatalf("mixed page sizes were published: changed=%v, err=%v", changed, err)
			}
			f.check(t, generation, time.Time{}, before)
			f.rep.Attach(f.db)
			f.sync(t)
			if f.rep.getMarker().Generation == generation {
				t.Fatal("page size change resumed the old generation")
			}
			f.check(t, f.rep.getMarker().Generation, time.Time{}, want)
		})
	}
}

func TestCompactionIncludesFirstChildWindow(t *testing.T) {
	f := newFixture(t)
	f.write(t, []byte("initial"))
	f.sync(t)
	f.clock.Add(time.Minute) // sole incremental commit is in the first quarter
	f.write(t, []byte("first quarter"))
	f.sync(t)
	generation := f.rep.getMarker().Generation
	f.clock.Add(3 * time.Hour)
	if err := f.rep.compact(context.Background(), generation, f.clock.Now()); err != nil {
		t.Fatal(err)
	}
	l, err := f.rep.loadLayout(context.Background(), generation)
	if err != nil {
		t.Fatal(err)
	}
	if len(l.windows[2]) != 1 || len(l.windows[1]) != 0 {
		t.Fatalf("idle first quarter was never rolled up/expired: L1=%d L2=%d", len(l.windows[1]), len(l.windows[2]))
	}
	f.check(t, generation, time.Time{}, []byte("first quarter"))
}

func TestLiveCatchupFailureRetriesAllPages(t *testing.T) {
	for _, renewal := range []bool{false, true} {
		for _, failure := range []string{"read", "write", "sync", "cancel"} {
			t.Run(fmt.Sprintf("renewal=%t/%s", renewal, failure), func(t *testing.T) {
				f := newFixture(t)
				f.write(t, []byte("before"))
				f.sync(t)
				generation := f.rep.getMarker().Generation
				f.clock.Add(time.Minute)
				if renewal {
					f.rep.config.Generation = time.Nanosecond
				}
				f.write(t, []byte("dirty"))
				ctx, cancel := context.WithCancel(context.Background())
				defer cancel()
				want := bytes.Repeat([]byte("catchup"), 10000)
				changed, fired, operations := false, false, 0
				f.rep.Attach(&guardedDatabase{Database: f.db, after: func(call int) {
					if call == 3 {
						f.write(t, want)
						changed = true
					}
				}})
				f.rep.files = faultStorage{Storage: OSStorage(), open: func(path string, create bool) error {
					if failure == "read" && changed && !create && path == f.rep.path {
						operations++
						if operations == 2 {
							fired = true
							return errInjected
						}
					}
					return nil
				}, wrap: func(path string, file File) File {
					if !strings.HasSuffix(path, ".replica-spool") {
						return file
					}
					return faultFile{File: file, write: func(data []byte, offset int64) (int, error) {
						if changed && (failure == "write" || failure == "cancel") {
							operations++
							if operations == 2 {
								fired = true
								if failure == "write" {
									return 0, errInjected
								}
								cancel()
							}
						}
						return file.WriteAt(data, offset)
					}, sync: func() error {
						if failure == "sync" {
							fired = true
							return errInjected
						}
						return file.Sync()
					}}
				}}
				err := f.rep.Sync(ctx)
				if !fired || (!errors.Is(err, errInjected) && !errors.Is(err, context.Canceled)) {
					t.Fatalf("catchup fault: fired=%v, err=%v", fired, err)
				}
				f.rep.files = OSStorage()
				f.rep.Attach(f.db)
				f.rep.config.Generation = 7 * 24 * time.Hour
				f.check(t, generation, time.Time{}, []byte("before"))
				f.sync(t)
				if f.rep.getMarker().Generation != generation {
					t.Fatal("failed catchup forced a whole-database copy")
				}
				f.check(t, generation, time.Time{}, want)
			})
		}
	}
}

// Complements TestModel's crash/bucket faults with writers between live reads.
// Deterministic seeds change sizes repeatedly, vacuum and leave one final
// transaction pending while every published state is restored and checked.
func TestLiveCaptureModel(t *testing.T) {
	for seed := uint64(1); seed <= 4; seed++ {
		t.Run(fmt.Sprint(seed), func(t *testing.T) {
			f := newFixture(t)
			rng := rand.New(rand.NewPCG(seed, seed+100))
			want := []byte("initial")
			f.write(t, want)
			f.sync(t)
			generation := f.rep.getMarker().Generation
			for round := 0; round < 12; round++ {
				f.clock.Add(time.Minute)
				f.write(t, want)
				captured := want
				f.rep.Attach(&guardedDatabase{Database: f.db, after: func(call int) {
					captured = want
					if call < 3 {
						return
					}
					want = bytes.Repeat([]byte{byte(round + call)}, 1+rng.IntN(48<<10))
					f.write(t, want)
					if rng.IntN(3) == 0 {
						if _, err := f.db.db.Exec(`VACUUM`); err != nil {
							t.Fatal(err)
						}
					}
				}})
				f.sync(t)
				f.rep.Attach(f.db)
				f.check(t, generation, time.Time{}, captured)
				f.sync(t)
				f.check(t, generation, time.Time{}, want)
			}
			f.clock.Add(3 * time.Hour)
			f.sync(t)
			f.check(t, generation, time.Time{}, want)
		})
	}
}
