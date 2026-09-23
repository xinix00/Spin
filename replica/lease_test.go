package replica

import (
	"context"
	"encoding/json"
	"os"
	"testing"
	"time"
)

func testLease(t *testing.T, path string, lost func(error)) (*Lease, error) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	return holdLease(ctx, &Lease{
		files: OSStorage(), path: path, lost: lost, now: time.Now,
		ttl: 400 * time.Millisecond, renew: 50 * time.Millisecond, settle: 50 * time.Millisecond,
	})
}

// A second process waits for as long as the first one keeps renewing, and
// takes over the moment it lets go.
func TestLeaseWaitsForTheHolderToLetGo(t *testing.T) {
	path := t.TempDir() + "/spin.db.lease"
	first, err := testLease(t, path, nil)
	if err != nil {
		t.Fatal(err)
	}
	acquired := make(chan *Lease, 1)
	go func() {
		second, err := testLease(t, path, nil)
		if err != nil {
			t.Error(err)
		}
		acquired <- second
	}()
	// Longer than the TTL: only renewal keeps the first one's hold.
	select {
	case <-acquired:
		t.Fatal("a second process took a lease that is being renewed")
	case <-time.After(time.Second):
	}
	released := time.Now()
	first.Release()
	select {
	case second := <-acquired:
		if waited := time.Since(released); waited > 2*time.Second {
			t.Fatalf("the second process took %s to follow a release", waited)
		}
		second.Release()
	case <-time.After(3 * time.Second):
		t.Fatal("the second process never took the released lease")
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("a released lease leaves its file: %v", err)
	}
}

// A holder that died without letting go (a crash, an out-of-memory stop)
// holds the database for one TTL at most.
func TestLeaseOfADeadHolderExpires(t *testing.T) {
	path := t.TempDir() + "/spin.db.lease"
	dead, err := testLease(t, path, nil)
	if err != nil {
		t.Fatal(err)
	}
	close(dead.stop) // it stops renewing, and never releases
	<-dead.done
	started := time.Now()
	next, err := testLease(t, path, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer next.Release()
	if waited := time.Since(started); waited < 200*time.Millisecond || waited > 2*time.Second {
		t.Fatalf("taking over a dead holder's lease took %s; its TTL is 400ms", waited)
	}
}

// A holder that finds another owner in its lease has lost the database and
// hears so, once, so it can stop writing.
func TestLeaseHolderHearsWhenAnotherTookOver(t *testing.T) {
	path := t.TempDir() + "/spin.db.lease"
	lost := make(chan error, 2)
	holder, err := testLease(t, path, func(err error) { lost <- err })
	if err != nil {
		t.Fatal(err)
	}
	defer holder.Release()
	data, _ := json.Marshal(leaseRecord{Owner: "someone-else", ExpiresAt: time.Now().Add(time.Minute)})
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-lost:
		if err != ErrLeaseLost {
			t.Fatalf("lost with %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("the holder never noticed the takeover")
	}
	holder.Release()
	if raw, err := os.ReadFile(path); err != nil || string(raw) != string(data) {
		t.Fatalf("releasing a lost lease touched the new owner's: %q, %v", raw, err)
	}
}

// A process that lets go can take the database again at once, and a stale
// or unreadable lease file holds nothing.
func TestLeaseTakesAFreeOrUnreadableLeaseAtOnce(t *testing.T) {
	path := t.TempDir() + "/spin.db.lease"
	for _, content := range []string{"", "not json", `{"owner":"old","expires_at":"2020-01-01T00:00:00Z"}`} {
		if content != "" {
			if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
				t.Fatal(err)
			}
		}
		started := time.Now()
		lease, err := testLease(t, path, nil)
		if err != nil {
			t.Fatal(err)
		}
		if waited := time.Since(started); waited > 2*time.Second {
			t.Fatalf("a lease file %q held the database for %s", content, waited)
		}
		lease.Release()
	}
}
