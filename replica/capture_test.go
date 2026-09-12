package replica

import (
	"bytes"
	"context"
	"net/http/httptest"
	"os"
	"testing"
)

// countingDatabase counts read transactions and can write between them,
// the way the server does while a live snapshot runs.
type countingDatabase struct {
	*testDatabase
	calls int
	write func(call int)
}

func (d *countingDatabase) WithReadTransaction(ctx context.Context, fn func() error) error {
	err := d.testDatabase.WithReadTransaction(ctx, fn)
	d.calls++
	if d.write != nil {
		d.write(d.calls)
	}
	return err
}

// A snapshot taken while the Spin serves goes in short transactions and
// still restores to the database as it was when the copy finished: pages
// written during the copy are read again. A snapshot taken before the
// Spin opens is one transaction.
func TestLiveSnapshotStaysConsistentAndOpeningSnapshotIsOneTransaction(t *testing.T) {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	server := httptest.NewServer(bucket)
	defer server.Close()
	config := testConfig(server)
	dir := t.TempDir()

	source, database := openReplicated(t, config, "one.example.test", dir+"/one.db")
	first := bytes.Repeat([]byte("first-version-"), 60000) // several segments of 256 KiB
	if err := database.WriteFile("large", first); err != nil {
		t.Fatal(err)
	}
	if err := database.WriteFile("state", []byte(`{"version":1}`)); err != nil {
		t.Fatal(err)
	}
	final := bytes.Repeat([]byte("final-version-"), 60000)
	changing := &countingDatabase{testDatabase: database}
	changing.write = func(call int) {
		// After the first segment left, the whole large file changes, and
		// the state too; both live on pages that were already copied.
		if call == 2 {
			if err := database.WriteFile("large", final); err != nil {
				t.Fatal(err)
			}
			if err := database.WriteFile("state", []byte(`{"version":2}`)); err != nil {
				t.Fatal(err)
			}
		}
	}
	source.Attach(changing)
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if status := source.Status(); !status.Complete || status.PendingPages != 0 {
		t.Fatalf("status after the live sync = %+v", status)
	}
	if changing.calls < 3 {
		t.Fatalf("a live snapshot used %d read transactions; expected one per segment plus the closing round", changing.calls)
	}
	// A generation past its limit renews in the background, one transaction
	// per segment again.
	source.config.Generation = 0
	if source.SnapshotRequiredBeforeServing() != "" {
		t.Fatal("an old generation must not block the opening")
	}
	if source.SnapshotReason() == "" {
		t.Fatal("an old generation is due for a background snapshot")
	}
	changing.write = nil
	changing.calls = 0
	if err := source.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	if changing.calls < 3 {
		t.Fatalf("the background renewal used %d read transactions", changing.calls)
	}
	source.config.Generation = testConfig(server).Generation
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	source.Close()

	if err := os.MkdirAll(dir+"/restored", 0o755); err != nil {
		t.Fatal(err)
	}
	restoredReplica, restored := openReplicated(t, config, "one.example.test", dir+"/restored/one.db")
	if state, err := restored.ReadFile("state"); err != nil || string(state) != `{"version":2}` {
		t.Fatalf("restored state = %q, %v", state, err)
	}
	if large, err := restored.ReadFile("large"); err != nil || !bytes.Equal(large, final) {
		t.Fatalf("restored large file: %d bytes, equal to final = %v, %v", len(large), bytes.Equal(large, final), err)
	}
	restored.Close()
	restoredReplica.Close()

	// Before a Spin opens, the copy is one transaction.
	opening, openingDB := openReplicated(t, config, "two.example.test", dir+"/two.db")
	defer opening.Close()
	defer openingDB.Close()
	if err := openingDB.WriteFile("large", first); err != nil {
		t.Fatal(err)
	}
	counting := &countingDatabase{testDatabase: openingDB}
	opening.Attach(counting)
	if opening.SnapshotRequiredBeforeServing() == "" {
		t.Fatal("a Spin without a generation must copy before it opens")
	}
	if err := opening.SyncAtOpen(context.Background()); err != nil {
		t.Fatal(err)
	}
	if counting.calls != 1 {
		t.Fatalf("the opening snapshot used %d read transactions; expected one", counting.calls)
	}
}
