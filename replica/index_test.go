package replica

import (
	"bytes"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

// indexTestReplica is enough of a Replica to read, write and compare the
// page index against a database file on disk.
func indexTestReplica(t *testing.T, pages [][]byte) *Replica {
	t.Helper()
	path := filepath.Join(t.TempDir(), "index.db")
	if err := os.WriteFile(path, bytes.Join(pages, nil), 0o644); err != nil {
		t.Fatal(err)
	}
	return &Replica{files: storageFor(vfs.Find("")), path: path, logger: slog.New(slog.NewTextHandler(io.Discard, nil))}
}

func indexTestPage(size int, fill byte) []byte { return bytes.Repeat([]byte{fill}, size) }

// The index survives a round trip, and every damaged form is refused: a
// refused index costs a full snapshot, an accepted wrong one would lose
// pages, so refusing is the safe side.
func TestIndexRoundTripAndEveryDamageIsRefused(t *testing.T) {
	x := pageIndex{pageSize: 4096, seq: 77, hashes: []pageHash{hashPage(indexTestPage(4096, 1)), hashPage(indexTestPage(4096, 2)), {}}}
	data := encodeIndex(x)
	decoded, err := decodeIndex(data)
	if err != nil || decoded.pageSize != x.pageSize || decoded.seq != 77 || len(decoded.hashes) != 3 || decoded.hashes[0] != x.hashes[0] || decoded.hashes[1] != x.hashes[1] || decoded.hashes[2] != x.hashes[2] {
		t.Fatalf("round trip = %+v, %v", decoded, err)
	}
	for position := range data {
		damaged := append([]byte(nil), data...)
		damaged[position] ^= 0x40
		if _, err := decodeIndex(damaged); err == nil {
			t.Fatalf("a flipped byte at %d was accepted", position)
		}
	}
	for length := 0; length < len(data); length++ {
		if _, err := decodeIndex(data[:length]); err == nil {
			t.Fatalf("an index cut at %d bytes was accepted", length)
		}
	}
	if _, err := decodeIndex(append(data, 0)); err == nil {
		t.Fatal("an index with a trailing byte was accepted")
	}
	for _, size := range []int{0, 256, 3000, 131072} {
		if _, err := decodeIndex(encodeIndex(pageIndex{pageSize: size})); err == nil {
			t.Fatalf("page size %d was accepted", size)
		}
	}
	if empty, err := decodeIndex(encodeIndex(pageIndex{pageSize: 512})); err != nil || empty.pageSize != 512 || len(empty.hashes) != 0 {
		t.Fatalf("empty index = %+v, %v", empty, err)
	}
}

// apply follows the database size: pages beyond it drop, new ones start
// unknown, and a page number outside the database is ignored.
func TestIndexApplyFollowsTheDatabaseSize(t *testing.T) {
	var x pageIndex
	one, two, three := hashPage([]byte{1}), hashPage([]byte{2}), hashPage([]byte{3})
	x.apply(512, 3*512, []uint32{1, 2, 3}, []pageHash{one, two, three})
	if len(x.hashes) != 3 || x.hashes[2] != three {
		t.Fatalf("after three pages: %+v", x)
	}
	x.apply(512, 2*512, []uint32{2, 7, 0}, []pageHash{three, one, one})
	if len(x.hashes) != 2 || x.hashes[0] != one || x.hashes[1] != three {
		t.Fatalf("after shrinking to two pages: %+v", x)
	}
	x.apply(512, 4*512, []uint32{4}, []pageHash{two})
	if len(x.hashes) != 4 || x.hashes[2] != (pageHash{}) || x.hashes[3] != two {
		t.Fatalf("after growing to four pages: %+v", x)
	}
	x.apply(512, 4*512, []uint32{1, 2}, []pageHash{two})
	if x.hashes[0] != two || x.hashes[1] != three {
		t.Fatalf("fewer hashes than pages must not touch the unhashed page: %+v", x)
	}
}

// The comparison names exactly the pages whose bytes differ from the index,
// the pages the index never saw, and page 1 when the size changed.
func TestDifferingPagesNamesTheDifferenceAndTheSizeChange(t *testing.T) {
	const size = 1024
	pages := [][]byte{indexTestPage(size, 'a'), indexTestPage(size, 'b'), indexTestPage(size, 'c'), indexTestPage(size, 'd')}
	r := indexTestReplica(t, pages)
	if err := r.rebuildIndex(size, 3); err != nil {
		t.Fatal(err)
	}
	index, err := r.usableIndex(marker{PageSize: size, Seq: 3})
	if err != nil || len(index.hashes) != 4 {
		t.Fatalf("index after rebuild = %+v, %v", index, err)
	}
	if _, err := r.usableIndex(marker{PageSize: size, Seq: 4}); err == nil {
		t.Fatal("an index behind the marker was usable")
	}
	if _, err := r.usableIndex(marker{PageSize: 2 * size, Seq: 3}); err == nil {
		t.Fatal("an index for another page size was usable")
	}
	same, total, err := r.differingPages(index)
	if err != nil || total != 4 || len(same) != 0 {
		t.Fatalf("an unchanged database differs in %v of %d, %v", same, total, err)
	}
	write := func(pages [][]byte) {
		if err := os.WriteFile(r.path, bytes.Join(pages, nil), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	write([][]byte{pages[0], pages[1], indexTestPage(size, 'x'), pages[3]})
	if changed, _, err := r.differingPages(index); err != nil || len(changed) != 1 || changed[0] != 3 {
		t.Fatalf("one changed page reported as %v, %v", changed, err)
	}
	write(append(append([][]byte{}, pages...), indexTestPage(size, 'e'), indexTestPage(size, 'f')))
	if grown, total, err := r.differingPages(index); err != nil || total != 6 || len(grown) != 3 || grown[0] != 1 || grown[1] != 5 || grown[2] != 6 {
		t.Fatalf("a grown database reported as %v of %d, %v", grown, total, err)
	}
	write(pages[:2])
	if shrunk, total, err := r.differingPages(index); err != nil || total != 2 || len(shrunk) != 1 || shrunk[0] != 1 {
		t.Fatalf("a shrunk database reported as %v of %d, %v", shrunk, total, err)
	}
	write([][]byte{indexTestPage(size, 'q'), pages[1], pages[2]})
	if both, _, err := r.differingPages(index); err != nil || len(both) != 1 || both[0] != 1 {
		t.Fatalf("page 1 is named once when it changed and the size changed: %v, %v", both, err)
	}
	write([][]byte{pages[0], []byte("half")})
	if _, _, err := r.differingPages(index); err == nil {
		t.Fatal("a database whose size is not page aligned was compared")
	}
	if err := os.WriteFile(r.indexPath(), []byte("SPINIDX1 garbage"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := r.readIndex(); err == nil {
		t.Fatal("a damaged index file was read")
	}
}

func FuzzDecodeIndex(f *testing.F) {
	f.Add(encodeIndex(pageIndex{pageSize: 4096, hashes: []pageHash{hashPage([]byte("a")), hashPage([]byte("b"))}}))
	f.Add(encodeIndex(pageIndex{pageSize: 512}))
	f.Add([]byte(indexMagic))
	f.Fuzz(func(t *testing.T, data []byte) {
		x, err := decodeIndex(data)
		if err != nil {
			return
		}
		again, err := decodeIndex(encodeIndex(x))
		if err != nil || again.pageSize != x.pageSize || again.seq != x.seq || len(again.hashes) != len(x.hashes) {
			t.Fatalf("decoded index cannot round trip: %+v, %v", again, err)
		}
	})
}

// The schedule parser accepts the documented form and refuses every shape
// that would break the tiers.
func TestParseScheduleAcceptsTiersAndRefusesTheRest(t *testing.T) {
	levels, err := ParseSchedule("")
	if err != nil || len(levels) != len(DefaultSchedule) || levels[0] != DefaultSchedule[0] {
		t.Fatalf("empty schedule = %v, %v", levels, err)
	}
	levels, err = ParseSchedule(" 15m:2h , 1h:24h,24h:168h")
	if err != nil || len(levels) != 3 || levels[1] != (Level{Window: time.Hour, Keep: 24 * time.Hour}) || levels[2].Keep != 168*time.Hour {
		t.Fatalf("documented schedule = %v, %v", levels, err)
	}
	if config := (Config{Schedule: levels}); config.validate() != nil {
		t.Fatalf("parsed schedule does not validate: %v", config.validate())
	}
	for _, bad := range []string{"15m", "15m:", ":2h", "30s:2h", "15m:10m", "15m:2h,20m:3h", "1h:2h,15m:3h", "15m:2h,1h:1h,90m:3h", "x:y", "15m:2h,,1h:24h"} {
		if levels, err := ParseSchedule(bad); err == nil {
			t.Fatalf("schedule %q was accepted as %v", bad, levels)
		}
	}
	if err := (Config{Schedule: []Level{{Window: 90 * time.Second, Keep: time.Hour}, {Window: 4 * time.Minute, Keep: time.Hour}}}).validate(); err == nil {
		t.Fatal("a level that is not a multiple of its predecessor validated")
	}
	if err := (Config{Schedule: []Level{{Window: time.Minute + time.Millisecond, Keep: time.Hour}}}).validate(); err == nil {
		t.Fatal("a window that is not whole seconds validated")
	}
	defaults := (Config{}).withDefaults()
	if defaults.Prefix == "" || defaults.Interval <= 0 || defaults.SegmentBytes <= 0 || len(defaults.Schedule) == 0 || defaults.Generation <= 0 || defaults.Retention < defaults.Generation {
		t.Fatalf("defaults = %+v", defaults)
	}
}

func FuzzParseSchedule(f *testing.F) {
	f.Add("15m:2h,1h:24h,24h:168h")
	f.Add("15m:2h")
	f.Add("")
	f.Fuzz(func(t *testing.T, value string) {
		levels, err := ParseSchedule(value)
		if err != nil {
			return
		}
		if err := (Config{Schedule: levels}).validate(); err != nil {
			t.Fatalf("parsed %q into a schedule that does not validate: %v", value, err)
		}
	})
}
