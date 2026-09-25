package replica

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"sync"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

// The lease detects an existing writer and loss of ownership. It is advisory:
// Storage has no atomic compare-and-swap, so two simultaneous acquisitions
// cannot be excluded by this file alone. The host must serialize starts and
// use recreate updates on a shared volume. SQLite and this replica both assume
// one writer; on HopOS a rolling update breaks it, because the
// new slot starts while the old one still runs, on the same volume. For a
// while two processes then wrote the same SQLite file, the same dirty log,
// and commits into the same generation: the gaps in the commit sequence and
// the markers behind the bucket of 22 and 23 September.
//
// Litestream guards the same thing with a lease in the bucket (lock.json,
// written with a conditional PUT). Both slots share the volume here, so the
// lease sits next to the database, where it covers a Spin without a replica
// as well and needs nothing from the object store. A holder renews it; a
// process that finds it held waits until the holder lets go or stops
// renewing.

const (
	// LeaseTTL is how long a lease holds without renewal: how long a start
	// waits for a predecessor that died without letting go.
	LeaseTTL     = 30 * time.Second
	leaseRenew   = 5 * time.Second
	leaseSettle  = time.Second
	leaseLogWait = time.Minute
)

// ErrLeaseLost says that another process holds the database now. Whoever held
// the lease must stop writing at once.
var ErrLeaseLost = errors.New("another process took over this database")

type leaseRecord struct {
	Owner     string    `json:"owner"`
	ExpiresAt time.Time `json:"expires_at"`
}

// Lease is a held lease; Release lets it go.
type Lease struct {
	files  Storage
	path   string
	owner  string
	logger *slog.Logger
	lost   func(error)
	now    func() time.Time
	ttl    time.Duration
	renew  time.Duration
	settle time.Duration

	stop    chan struct{}
	done    chan struct{}
	release sync.Once
}

// HoldLease waits until no other live process holds the database at path,
// takes it, and keeps renewing it until Release. The host must serialize
// acquisitions; this is not an atomic process lock. lost runs once when a
// renewal cannot confirm ownership; the caller must then stop writing.
func HoldLease(ctx context.Context, inner vfs.VFS, path string, logger *slog.Logger, lost func(error)) (*Lease, error) {
	if inner == nil {
		return nil, errors.New("lease needs a storage VFS")
	}
	return holdLease(ctx, &Lease{
		files: storageFor(inner), path: fullPath(inner, path) + ".lease", logger: logger, lost: lost,
		now: time.Now, ttl: LeaseTTL, renew: leaseRenew, settle: leaseSettle,
	})
}

func holdLease(ctx context.Context, l *Lease) (*Lease, error) {
	if l.logger == nil {
		l.logger = slog.New(slog.NewTextHandler(io.Discard, nil))
	}
	var id [12]byte
	_, _ = rand.Read(id[:])
	l.owner = hex.EncodeToString(id[:])
	l.stop, l.done = make(chan struct{}), make(chan struct{})
	var logged time.Time
	for {
		record, readErr := l.read()
		if readErr != nil {
			return nil, fmt.Errorf("read database lease: %w", readErr)
		}
		now := l.now()
		held := readErr == nil && record.Owner != "" && record.Owner != l.owner && now.Before(record.ExpiresAt)
		if held {
			if now.Sub(logged) >= leaseLogWait {
				logged = now
				l.logger.Warn("database is held by another process; waiting until it lets go", "path", l.path, "owner", record.Owner, "until", record.ExpiresAt)
			}
			if err := sleep(ctx, min(record.ExpiresAt.Sub(now)+10*time.Millisecond, l.renew)); err != nil {
				return nil, err
			}
			continue
		}
		takeover := readErr == nil && record.Owner != "" && record.Owner != l.owner
		if err := l.write(); err != nil {
			return nil, err
		}
		if takeover {
			// Another process may have seen the same expired lease: the one
			// whose write is there after a moment has it.
			if err := sleep(ctx, l.settle); err != nil {
				return nil, err
			}
			if record, err := l.read(); err != nil || record.Owner != l.owner {
				continue
			}
		}
		go l.keep()
		return l, nil
	}
}

func sleep(ctx context.Context, d time.Duration) error {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-timer.C:
		return nil
	}
}

func (l *Lease) read() (leaseRecord, error) {
	exists, err := l.files.Exists(l.path)
	if err != nil || !exists {
		return leaseRecord{}, err
	}
	file, err := l.files.Open(l.path, false)
	if err != nil {
		return leaseRecord{}, err
	}
	defer file.Close()
	size, err := file.Size()
	if err != nil {
		return leaseRecord{}, err
	}
	if size <= 0 || size > 4096 {
		return leaseRecord{}, errors.New("invalid database lease size")
	}
	data := make([]byte, size)
	if n, err := file.ReadAt(data, 0); err != nil || n != len(data) {
		if err == nil {
			err = io.ErrUnexpectedEOF
		}
		return leaseRecord{}, err
	}
	var record leaseRecord
	// A torn file may be a live owner's interrupted renewal. It is not
	// evidence that the database is free.
	if err := json.Unmarshal(data, &record); err != nil {
		return leaseRecord{}, fmt.Errorf("invalid database lease: %w", err)
	}
	if record.Owner == "" || record.ExpiresAt.IsZero() {
		return leaseRecord{}, errors.New("database lease has no owner or expiry")
	}
	return record, nil
}

func (l *Lease) write() error {
	data, _ := json.Marshal(leaseRecord{Owner: l.owner, ExpiresAt: l.now().Add(l.ttl)})
	return writeLocalFile(l.files, l.path, data)
}

// keep renews the lease until Release, and says so when another process
// has taken it.
func (l *Lease) keep() {
	defer close(l.done)
	ticker := time.NewTicker(l.renew)
	defer ticker.Stop()
	for {
		select {
		case <-l.stop:
			return
		case <-ticker.C:
		}
		record, err := l.read()
		if err == nil && (record.Owner != l.owner || !l.now().Before(record.ExpiresAt)) {
			err = ErrLeaseLost
		}
		if err == nil {
			err = l.write()
		}
		if err != nil {
			// An unreadable, expired or unrenewable lease is no proof of
			// ownership. Never recreate it and let the old writer carry on.
			l.logger.Error("database lease is no longer confirmed", "path", l.path, "error", err)
			if l.lost != nil {
				l.lost(errors.Join(ErrLeaseLost, err))
			}
			return
		}
	}
}

// Release stops renewing and removes the lease if it is still this one.
func (l *Lease) Release() {
	l.release.Do(func() {
		close(l.stop)
		<-l.done
		if record, err := l.read(); err == nil && record.Owner == l.owner {
			_ = l.files.Remove(l.path)
		}
	})
}
