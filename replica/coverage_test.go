package replica

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http/httptest"
	"os"
	"strings"
	"testing"

	"github.com/ncruces/go-sqlite3/vfs"
)

// The failure this file exists for, in one sentence: a generation whose
// manifests record a size that its own segments cannot fill restores into a
// file with holes, and SQLite calls that "database disk image is malformed".
// It happened on a live tenant on 22 September and cost a day of data.

// A capture that lacks the pages the database grew by must not be committed.
// The generation so far stays restorable, the log says what happened, and the
// next sync starts a fresh generation that restores cleanly.
func TestSyncRefusesASizeTheGenerationCannotFill(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	dir := t.TempDir()

	source, database := openReplicated(t, config, "short.example.test", dir+"/short.db")
	if err := database.WriteFile("state", []byte(`{"version":1}`)); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	firstGeneration := source.Status().Generation
	sizeAfterSnapshot := source.getMarker().Size

	// The database grows, but the tracker never saw those writes: exactly the
	// hole that a missed write, a foreign writer or a lost dirty log leaves.
	grown := bytes.Repeat([]byte("grown-page-"), 120000) // ~1.3 MiB, many pages
	if err := database.WriteFile("grown", grown); err != nil {
		t.Fatal(err)
	}
	stolen := source.tracker.take()
	if len(stolen) == 0 {
		t.Fatal("no dirty pages to steal; the write did not reach the tracker")
	}

	// Two guards cover this, and they overlap on purpose: the source guard
	// sees the change counter move while nothing is marked (guard.go), and
	// behind it the commit gate refuses a size the chain cannot fill
	// (coverage.go). Whichever fires, the sync must not commit.
	err := source.Sync(context.Background())
	if !errors.Is(err, errForeignWrite) && !errors.Is(err, errGenerationShort) {
		t.Fatalf("sync with missing pages = %v, want errForeignWrite or errGenerationShort", err)
	}
	if source.getMarker().Complete {
		t.Fatal("the marker stayed complete; the next sync would continue the broken generation")
	}
	for key, data := range bucket.objects {
		if !strings.Contains(key, firstGeneration) || !strings.HasSuffix(key, ".json") {
			continue
		}
		var m manifest
		if err := json.Unmarshal(data, &m); err != nil {
			continue
		}
		if m.MinSize > sizeAfterSnapshot {
			t.Fatalf("manifest %s records %d bytes while the generation carries %d", key, m.MinSize, sizeAfterSnapshot)
		}
	}

	// And now the resync: a fresh generation, and a restore of it opens.
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if second := source.Status().Generation; second == firstGeneration {
		t.Fatalf("generation stayed %s; no fresh one was started", second)
	}
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	source.Close()

	if err := os.MkdirAll(dir+"/restored", 0o755); err != nil {
		t.Fatal(err)
	}
	replica, restored := openReplicated(t, config, "short.example.test", dir+"/restored/short.db")
	defer replica.Close()
	defer restored.Close()
	if !replica.Status().Restored {
		t.Fatalf("status = %+v; the fresh generation did not restore", replica.Status())
	}
	back, err := restored.ReadFile("grown")
	if err != nil || !bytes.Equal(back, grown) {
		t.Fatalf("restored grown value: %d bytes, %v", len(back), err)
	}
}

// A generation that claims more than it holds is refused before SQLite sees
// it, and the error names the numbers. This is the bollenloods case, built by
// patching the size in the last manifest just as the writer once did.
func TestRestoreRefusesAGenerationThatIsShort(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	dir := t.TempDir()

	source, database := openReplicated(t, config, "patched.example.test", dir+"/patched.db")
	if err := database.WriteFile("state", []byte(`{"version":1}`)); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	generation := source.Status().Generation
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	source.Close()

	// Double the size in the newest manifest: the pages for the second half
	// were never shipped, which is what the replica did to itself.
	// Manifests are every object of the generation outside data/; the newest
	// is the one with the highest sequence number.
	last, best := "", manifest{}
	for key, data := range bucket.objects {
		if !strings.Contains(key, generation) || strings.Contains(key, "/data/") {
			continue
		}
		var candidate manifest
		if err := json.Unmarshal(data, &candidate); err != nil || candidate.Seq == 0 {
			continue
		}
		if candidate.Seq >= best.Seq {
			last, best = key, candidate
		}
	}
	if last == "" {
		t.Fatal("no manifest in the bucket")
	}
	m := best
	// The real failure recorded the larger size in the segment as well, so the
	// existing truncation check saw nothing wrong. Rebuild that exactly: the
	// same pages, a database size twice as large, hash and manifest in step.
	claimed := m.MinSize * 2
	m.MinSize = claimed
	for index, ref := range m.Parts {
		raw, ok := bucket.objects[ref.Key]
		if !ok {
			t.Fatalf("part %s is missing from the bucket", ref.Key)
		}
		seg, err := decodeSegment(raw)
		if err != nil {
			t.Fatal(err)
		}
		seg.DBSize = claimed
		grown := encodeSegment(seg)
		bucket.objects[ref.Key] = grown
		m.Parts[index].Size = int64(len(grown))
		m.Parts[index].Hash = sha256hex(grown)
	}
	patched, err := json.Marshal(m)
	if err != nil {
		t.Fatal(err)
	}
	bucket.objects[last] = patched

	replica, err := New(config, "patched.example.test", dir+"/restored.db", vfs.Find(""), slog.New(slog.NewTextHandler(io.Discard, nil)))
	if err != nil {
		t.Fatal(err)
	}
	defer replica.Close()
	err = replica.Prepare(context.Background())
	if !errors.Is(err, errGenerationShort) || !strings.Contains(err.Error(), fmt.Sprintf("for a database of %d bytes", claimed)) {
		t.Fatalf("prepare on a short generation = %v; it should name the claimed size and the missing pages", err)
	}
	if _, err := os.Stat(dir + "/restored.db"); err == nil {
		t.Fatal("a short restore was published anyway")
	}
}

// The restore side asks whether a generation ever held a page. One that was
// shipped and later cut away by a truncation is not a hole: the source has
// the same zero there after it grew again, which is what
// TestCompactionPreservesShrinkThenGrow guards. One that never arrived is.
func TestShortfallNamesPagesTheGenerationNeverHeld(t *testing.T) {
	const pageSize = 4096
	var shipped pageSet
	for page := uint32(1); page <= 8; page++ {
		shipped.add(page)
	}
	if _, _, ok := shipped.shortfall(8*pageSize, pageSize); !ok {
		t.Fatal("eight shipped pages cover a database of eight pages")
	}
	// A shrink to four pages and a grow back to eight: the pages were shipped
	// once, so there is nothing to report.
	if _, _, ok := shipped.shortfall(8*pageSize, pageSize); !ok {
		t.Fatal("a shrink and a later grow does not make shipped pages missing")
	}
	// A database that doubles while nothing was shipped for the second half.
	first, missing, ok := shipped.shortfall(16*pageSize, pageSize)
	if ok || first != 9 || missing != 8 {
		t.Fatalf("doubling without pages: first=%d missing=%d ok=%v, want 9, 8, false", first, missing, ok)
	}
}

// The writer's own question, on the numbers of the real failure: a database
// that doubles while the capture carries only the pages of the first half.
func TestGrowthGapNamesTheFirstMissingPage(t *testing.T) {
	const pageSize = 4096
	var pages []uint32
	for page := uint32(1); page <= 100; page++ {
		pages = append(pages, page)
	}
	if _, _, gap := growthGap(100*pageSize, 100*pageSize, pageSize, pages); gap {
		t.Fatal("a database that did not grow has no gap")
	}
	if _, _, gap := growthGap(0, 100*pageSize, pageSize, pages); gap {
		t.Fatal("a capture that holds every page has no gap")
	}
	first, missing, gap := growthGap(100*pageSize, 200*pageSize, pageSize, pages)
	if !gap || first != 101 || missing != 100 {
		t.Fatalf("doubling without the new pages: first=%d missing=%d gap=%v, want 101, 100, true", first, missing, gap)
	}
}

func TestCoverageExcludesOnlySQLiteLockBytePage(t *testing.T) {
	for _, pageSize := range []int{512, 4096, 65536} {
		lockPage := uint32((1<<30)/pageSize + 1)
		previous := int64(lockPage-2) * int64(pageSize)
		size := int64(lockPage+2) * int64(pageSize)
		pages := []uint32{lockPage - 1, lockPage + 1, lockPage + 2}
		if first, missing, gap := growthGap(previous, size, pageSize, pages); gap {
			t.Fatalf("page size %d: lock page reported as a gap: first=%d missing=%d", pageSize, first, missing)
		}
		if first, missing, gap := growthGap(previous, size, pageSize, pages[1:]); !gap || first != lockPage-1 || missing != 1 {
			t.Fatalf("page size %d: real missing page not detected: first=%d missing=%d gap=%v", pageSize, first, missing, gap)
		}
		var shipped pageSet
		for page := uint32(1); page <= lockPage+2; page++ {
			if page != lockPage {
				shipped.add(page)
			}
		}
		if first, missing, ok := shipped.shortfall(size, pageSize); !ok {
			t.Fatalf("page size %d: restore rejects missing lock page: first=%d missing=%d", pageSize, first, missing)
		}
		if first, missing, ok := shipped.shortfall(size+int64(pageSize), pageSize); ok || first != lockPage+3 || missing != 1 {
			t.Fatalf("page size %d: restore accepted a real hole: first=%d missing=%d ok=%v", pageSize, first, missing, ok)
		}
	}
}

func BenchmarkGrowthGapAtTerabyte(b *testing.B) {
	const previous = 1 << 40
	const pageSize = 4096
	pages := []uint32{1, previous/pageSize + 1}
	b.ReportAllocs()
	for b.Loop() {
		if _, _, gap := growthGap(previous, previous+pageSize, pageSize, pages); gap {
			b.Fatal("the new tail page is present")
		}
	}
}
