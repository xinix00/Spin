package replica

import (
	"bytes"
	"context"
	"database/sql"
	"errors"
	"io"
	"log/slog"
	"net/http/httptest"
	"net/url"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

// A database that has no replica state of its own must not bury a generation
// that is already in the bucket: nothing orders the two. This is the case that
// cost a day of data on 22 September, when a database created from nothing was
// adopted as the truth and its empty snapshot became the current generation.
func TestPrepareRefusesToBuryAGenerationWithoutProvenance(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	dir := t.TempDir()

	source, database := openReplicated(t, config, "proof.example.test", dir+"/proof.db")
	if err := database.WriteFile("state", []byte(`{"version":1}`)); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	generation := source.Status().Generation
	current := append([]byte(nil), bucket.objects[source.currentKey()]...)
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	source.Close()

	// A fresh, unrelated database at another path: it never shipped a page and
	// carries no marker.
	plain, err := openTestDatabase(dir+"/fresh.db", "")
	if err != nil {
		t.Fatal(err)
	}
	if err := plain.WriteFile("state", []byte(`{"version":"from nowhere"}`)); err != nil {
		t.Fatal(err)
	}
	if err := plain.Close(); err != nil {
		t.Fatal(err)
	}

	replica, err := New(config, "proof.example.test", dir+"/fresh.db", vfs.Find(""), slog.New(slog.NewTextHandler(io.Discard, nil)))
	if err != nil {
		t.Fatal(err)
	}
	err = replica.Prepare(context.Background())
	if !errors.Is(err, errUnprovenDatabase) {
		t.Fatalf("prepare = %v, want errUnprovenDatabase", err)
	}
	if !bytes.Equal(bucket.objects[replica.currentKey()], current) {
		t.Fatal("the current generation was moved anyway")
	}
	if got := string(bucket.objects[replica.currentKey()]); got != generation {
		t.Fatalf("current = %q, want %q", got, generation)
	}

	// One VFS name per namespace: this one has to go before the next opens.
	replica.Close()

	// With the knob it adopts, loudly, because then the operator said so.
	adopting := config
	adopting.AdoptLocalDatabase = true
	second, err := New(adopting, "proof.example.test", dir+"/fresh.db", vfs.Find(""), slog.New(slog.NewTextHandler(io.Discard, nil)))
	if err != nil {
		t.Fatal(err)
	}
	defer second.Close()
	if err := second.Prepare(context.Background()); err != nil {
		t.Fatalf("prepare with AdoptLocalDatabase = %v", err)
	}
}

// A marker behind the bucket's tip of its own generation is never a reason to
// write the bucket over the database: this process is the only writer, so the
// file holds those commits and whatever came after. When the dirty log cannot
// say what changed since the marker, a new generation starts from the file.
func TestPrepareNeverRestoresOverItsOwnDatabase(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	dir := t.TempDir()

	source, database := openReplicated(t, config, "behind.example.test", dir+"/behind.db")
	if err := database.WriteFile("state", []byte(`{"version":1}`)); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if err := database.WriteFile("state", []byte(`{"version":2}`)); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	stored := source.getMarker()
	generation := stored.Generation
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	// A marker behind the bucket and a dirty log already past it.
	stored.Seq--
	if err := source.setMarker(stored); err != nil {
		t.Fatal(err)
	}
	source.Close()

	replica, reopened := openReplicated(t, config, "behind.example.test", dir+"/behind.db")
	defer replica.Close()
	defer reopened.Close()
	if replica.Status().Restored {
		t.Fatal("the bucket was written over the database")
	}
	if state, err := reopened.ReadFile("state"); err != nil || string(state) != `{"version":2}` {
		t.Fatalf("state = %q, %v", state, err)
	}
	if reason := replica.SnapshotReason(); !strings.Contains(reason, "past this marker") {
		t.Fatalf("snapshot reason = %q", reason)
	}
	if err := replica.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if replica.getMarker().Generation == generation {
		t.Fatal("the generation continued without knowing what changed")
	}
}

// The bucket moved to a generation this database never made (another copy
// of the Spin ran on and replicated). With nothing unshipped here the file is
// a stale copy and the bucket wins; with writes here the bucket lacks, nothing
// orders the two and the open refuses, naming both.
func TestPrepareMeetsABucketThatMovedOn(t *testing.T) {
	for _, unshipped := range []bool{false, true} {
		bucket := &fakeBucket{objects: map[string][]byte{}}
		server := httptest.NewServer(bucket)
		config := testConfig(server)
		here, elsewhere := t.TempDir(), t.TempDir()

		first, database := openReplicated(t, config, "moved.example.test", here+"/moved.db")
		if err := database.WriteFile("state", []byte(`{"from":"here"}`)); err != nil {
			t.Fatal(err)
		}
		if err := first.Sync(context.Background()); err != nil {
			t.Fatal(err)
		}
		if unshipped {
			if err := database.WriteFile("state", []byte(`{"from":"here, unshipped"}`)); err != nil {
				t.Fatal(err)
			}
		}
		if err := database.Close(); err != nil {
			t.Fatal(err)
		}
		first.Close()

		// Another copy restores the Spin, runs on and renews the generation.
		other, otherDatabase := openReplicated(t, config, "moved.example.test", elsewhere+"/moved.db")
		if err := otherDatabase.WriteFile("state", []byte(`{"from":"elsewhere"}`)); err != nil {
			t.Fatal(err)
		}
		other.config.Generation = time.Nanosecond
		// Generation ids order by the second they start in.
		time.Sleep(1100 * time.Millisecond)
		if err := other.Sync(context.Background()); err != nil {
			t.Fatal(err)
		}
		if err := otherDatabase.Close(); err != nil {
			t.Fatal(err)
		}
		other.Close()

		replica, err := New(config, "moved.example.test", here+"/moved.db", vfs.Find(""), slog.New(slog.NewTextHandler(io.Discard, nil)))
		if err != nil {
			t.Fatal(err)
		}
		prepareErr := replica.Prepare(context.Background())
		restored := replica.Status().Restored
		replica.Close()
		server.Close()
		if unshipped {
			if !errors.Is(prepareErr, errUnprovenDatabase) {
				t.Fatalf("writes here that the bucket lacks: prepare = %v", prepareErr)
			}
			continue
		}
		if prepareErr != nil || !restored {
			t.Fatalf("a stale copy with nothing unshipped: prepare = %v, restored = %v", prepareErr, restored)
		}
		check, err := openTestDatabase(here+"/moved.db", "")
		if err != nil {
			t.Fatal(err)
		}
		state, err := check.ReadFile("state")
		check.Close()
		if err != nil || string(state) != `{"from":"elsewhere"}` {
			t.Fatalf("state after the restore = %q, %v", state, err)
		}
	}
}

// The other way a marker ends up behind the bucket: the commit reached the
// bucket and the marker write that records it did not. The database then holds
// that commit plus everything written since, so restoring over it throws away
// real writes. It keeps the file and starts a fresh generation.
func TestPrepareKeepsADatabaseWhoseCommitOutranItsMarker(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	dir := t.TempDir()

	source, database := openReplicated(t, config, "interrupted.example.test", dir+"/interrupted.db")
	if err := database.WriteFile("state", []byte(`{"version":1}`)); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if err := database.WriteFile("state", []byte(`{"version":2}`)); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	// The write that follows the commit the marker never recorded: it is only
	// in this file, and nothing but this file can bring it back.
	if err := database.WriteFile("state", []byte(`{"version":3}`)); err != nil {
		t.Fatal(err)
	}
	stored := source.getMarker()
	generation := stored.Generation
	// What the sync leaves behind when the manifest lands and the marker write
	// after it does not: a sequence behind the bucket, and incomplete.
	stored.Seq--
	stored.Complete, stored.Clean = false, false
	if err := source.setMarker(stored); err != nil {
		t.Fatal(err)
	}
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	source.Close()

	replica, reopened := openReplicated(t, config, "interrupted.example.test", dir+"/interrupted.db")
	if replica.Status().Restored {
		t.Fatalf("status = %+v; the database was ahead of the bucket and was restored over", replica.Status())
	}
	state, err := reopened.ReadFile("state")
	if err != nil || string(state) != `{"version":3}` {
		t.Fatalf("state after the reopen = %q, %v; the unshipped write is gone", state, err)
	}
	if reason := replica.SnapshotReason(); reason == "" {
		t.Fatal("the next sync continues the generation the bucket already holds a commit for")
	}
	if err := replica.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if fresh := replica.getMarker().Generation; fresh == generation {
		t.Fatalf("the sync stayed in generation %s instead of starting a fresh one", fresh)
	}
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}
	replica.Close()

	// And the bucket now restores to the file as it stood, the unshipped write
	// included.
	into := t.TempDir()
	check, restored := openReplicated(t, config, "interrupted.example.test", into+"/interrupted.db")
	defer check.Close()
	defer restored.Close()
	if !check.Status().Restored {
		t.Fatalf("status = %+v; a fresh directory must restore", check.Status())
	}
	if state, err := restored.ReadFile("state"); err != nil || string(state) != `{"version":3}` {
		t.Fatalf("restored state = %q, %v", state, err)
	}
}

// A write that goes around the tracking VFS leaves pages nobody will ship.
// The source guard finds it on the page it shipped last, which is
// Litestream's lastPageMatch, and ends the generation instead of poisoning it.
func TestSourceGuardCatchesAWriteOutsideTheVFS(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	dir := t.TempDir()

	source, database := openReplicated(t, config, "stray.example.test", dir+"/stray.db")
	defer source.Close()
	if err := database.WriteFile("state", []byte(`{"version":1}`)); err != nil {
		t.Fatal(err)
	}
	large := bytes.Repeat([]byte("witness-"), 40000)
	if err := database.WriteFile("large", large); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	generation := source.Status().Generation
	page, pageSize := source.witness.number, source.witness.pageSize
	if page == 0 || pageSize == 0 {
		t.Fatal("no witness page after a sync")
	}

	// Straight into the file, behind the VFS the tracker listens on.
	file, err := os.OpenFile(dir+"/stray.db", os.O_RDWR, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := file.WriteAt(bytes.Repeat([]byte{0x5a}, pageSize), int64(page-1)*int64(pageSize)); err != nil {
		t.Fatal(err)
	}
	if err := file.Sync(); err != nil {
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}

	if err := source.Sync(context.Background()); !errors.Is(err, errForeignWrite) {
		t.Fatalf("sync after a stray write = %v, want errForeignWrite", err)
	}
	if source.getMarker().Complete {
		t.Fatal("the marker stayed complete; the next sync would continue this generation")
	}
	if source.Status().Generation != generation {
		t.Fatal("the generation changed before the remedy ran")
	}
	// And the remedy is not blocked by the guard that asked for it.
	if err := source.Sync(context.Background()); err != nil {
		t.Fatalf("the resync after a stray write = %v", err)
	}
	if source.Status().Generation == generation {
		t.Fatal("no fresh generation was started")
	}
	_ = database.Close()
}

// The restore asks the host whether SQLite agrees, before it publishes over a
// database that may still be serving. A refusal leaves the old file in place.
func TestRestoreAsksTheHostToVerify(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	dir := t.TempDir()

	source, database := openReplicated(t, config, "verify.example.test", dir+"/verify.db")
	if err := database.WriteFile("state", []byte(`{"version":1}`)); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	source.Close()

	// A host that refuses: nothing is published.
	refused := errors.New("quick_check said no")
	blocked, err := NewWithOptions(config, "verify.example.test", dir+"/blocked.db", vfs.Find(""),
		slog.New(slog.NewTextHandler(io.Discard, nil)), Options{Verify: func(string) error { return refused }})
	if err != nil {
		t.Fatal(err)
	}
	if err := blocked.Prepare(context.Background()); !errors.Is(err, refused) {
		t.Fatalf("prepare with a refusing host = %v, want the refusal", err)
	}
	blocked.Close()
	if _, err := os.Stat(dir + "/blocked.db"); err == nil {
		t.Fatal("a database the host refused was published anyway")
	}

	// A host that runs the real check: it passes, and it saw the scratch file.
	var checked string
	accepted, err := NewWithOptions(config, "verify.example.test", dir+"/accepted.db", vfs.Find(""),
		slog.New(slog.NewTextHandler(io.Discard, nil)), Options{Verify: func(path string) error {
			checked = path
			return quickCheck(path)
		}})
	if err != nil {
		t.Fatal(err)
	}
	defer accepted.Close()
	if err := accepted.Prepare(context.Background()); err != nil {
		t.Fatalf("prepare with a checking host = %v", err)
	}
	if checked == "" {
		t.Fatal("the host was never asked")
	}
	if !accepted.Status().Restored {
		t.Fatalf("status = %+v; the verified database was not restored", accepted.Status())
	}
}

// quickCheck is the host side of Options.Verify, the way Spin wires it.
func quickCheck(path string) error {
	u := url.URL{Scheme: "file", Path: path}
	u.RawQuery = "mode=ro"
	db, err := sql.Open("sqlite3", u.String())
	if err != nil {
		return err
	}
	defer db.Close()
	var result string
	if err := db.QueryRow(`PRAGMA quick_check`).Scan(&result); err != nil {
		return err
	}
	if result != "ok" {
		return errors.New("quick_check: " + result)
	}
	return nil
}

// A restart while a new generation is being copied is not a database that
// changed behind the replica's back: the marker names the new generation, the
// dirty log still the one before, and the database is whole. With a complete
// generation in the bucket the Spin opens and the copy starts over in the
// background, with its progress.
func TestAnInterruptedCopyStartsOverInTheBackground(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	dir := t.TempDir()

	source, database := openReplicated(t, config, "interrupted-copy.example.test", dir+"/copy.db")
	if err := database.WriteFile("state", []byte(`{"version":1}`)); err != nil {
		t.Fatal(err)
	}
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	complete := source.getMarker().Generation
	if err := database.WriteFile("state", []byte(`{"version":2}`)); err != nil {
		t.Fatal(err)
	}
	// What a stop in the middle of a new generation's copy leaves: its name
	// in the marker, incomplete, and a dirty log of the generation before.
	interrupted := marker{Version: formatVersion, Generation: newGenerationID(time.Now().Add(time.Hour)), StartedAt: time.Now()}
	if err := source.setMarker(interrupted); err != nil {
		t.Fatal(err)
	}
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	source.Close()

	replica, reopened := openReplicated(t, config, "interrupted-copy.example.test", dir+"/copy.db")
	defer replica.Close()
	defer reopened.Close()
	if reason := replica.SnapshotReason(); !strings.Contains(reason, "did not finish") {
		t.Fatalf("snapshot reason = %q", reason)
	}
	var stages []string
	replica.OnCopy = func(progress CopyProgress) {
		if len(stages) == 0 || stages[len(stages)-1] != progress.Stage {
			stages = append(stages, progress.Stage)
		}
		if progress.Done > progress.Total || progress.Total <= 0 {
			t.Errorf("progress %+v", progress)
		}
	}
	if err := replica.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(stages, ","); got != "read,upload" {
		t.Fatalf("copy stages = %q", got)
	}
	if replica.Status().Copy != nil {
		t.Fatal("the copy's progress stays after it finished")
	}
	fresh := replica.getMarker()
	if !fresh.Complete || fresh.Generation == complete || fresh.Generation == interrupted.Generation {
		t.Fatalf("marker after the background copy = %+v", fresh)
	}
	state, err := reopened.ReadFile("state")
	if err != nil || string(state) != `{"version":2}` {
		t.Fatalf("state = %q, %v", state, err)
	}
}
