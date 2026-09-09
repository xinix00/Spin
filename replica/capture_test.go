package replica

import (
	"bytes"
	"context"
	"net/http/httptest"
	"os"
	"testing"
)

// writingBetween is a database that writes between the short read
// transactions of a capture, the way the server does while a snapshot runs.
type writingBetween struct {
	*testDatabase
	calls int
	write func(call int)
}

func (d *writingBetween) WithReadTransaction(ctx context.Context, fn func() error) error {
	err := d.testDatabase.WithReadTransaction(ctx, fn)
	d.calls++
	d.write(d.calls)
	return err
}

// A snapshot copied in segments while the database keeps changing still
// restores to the database as it was when the copy finished: pages written
// during the copy are read again.
func TestSnapshotStaysConsistentWhileTheDatabaseChanges(t *testing.T) {
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
	changing := &writingBetween{testDatabase: database}
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
	status := source.Status()
	if !status.Complete || status.PendingPages != 0 {
		t.Fatalf("status after sync = %+v", status)
	}
	if changing.calls < 3 {
		t.Fatalf("capture used %d read transactions; expected one per segment plus the closing round", changing.calls)
	}
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	source.Close()

	if err := os.MkdirAll(dir+"/restored", 0o755); err != nil {
		t.Fatal(err)
	}
	restoredReplica, restored := openReplicated(t, config, "one.example.test", dir+"/restored/one.db")
	defer restoredReplica.Close()
	defer restored.Close()
	if !restoredReplica.Status().Restored {
		t.Fatalf("status = %+v; the database was not restored", restoredReplica.Status())
	}
	state, err := restored.ReadFile("state")
	if err != nil || string(state) != `{"version":2}` {
		t.Fatalf("restored state = %q, %v", state, err)
	}
	large, err := restored.ReadFile("large")
	if err != nil || !bytes.Equal(large, final) {
		t.Fatalf("restored large file: %d bytes, equal to final = %v, %v", len(large), bytes.Equal(large, final), err)
	}
}
