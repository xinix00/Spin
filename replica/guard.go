package replica

import (
	"context"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"strings"
	"time"
)

// The guards in this file answer the one question the replica could not ask
// before: is the source still the database this replica has been following?
// Everything else in the package trusts the tracking VFS to see every write.
// That trust is documented (README: writes outside the VFS are unsupported)
// and it held for months, but unsupported is not the same as detected, and
// the day it slipped the replica kept shipping for hours onto a generation
// that could never restore.
//
// Litestream, which solves the same problem through SQLite's WAL, carries
// four guards we did not have (litestream/db.go): a read lock so nobody can
// checkpoint behind it, verify() on the WAL salts, lastPageMatch() on the
// bytes of the page it shipped last, and checkDatabaseBehindReplica() so a
// local database that lost writes does not win over the replica. We cannot
// copy the first two: we do not read the WAL, we watch writes. The other two
// we can, and the cheapest detector for a VFS that misses a write is SQLite's
// own file change counter, four bytes on page 1 that every write transaction
// increments.

// errForeignWrite: the source changed in a way this replica did not see. The
// generation so far stays restorable; the next sync starts a fresh one.
var errForeignWrite = errors.New("replica: the database changed outside the tracking VFS; a fresh generation is needed")

// errUnprovenDatabase: a database with no replica state of its own met a
// bucket that already holds a generation. Both may hold real data and there
// is no way to order them, so the replica refuses to bury either.
var errUnprovenDatabase = errors.New("replica: this database has no replica state while the bucket holds a generation")

// changeCounter is SQLite's file change counter: page 1, offset 24, four
// bytes big endian, incremented by every write transaction. The version this
// replica last agreed with is remembered in memory; it only has to catch a
// writer while this process runs, and a restart starts from the dirty log.
func changeCounter(page1 []byte) (uint32, bool) {
	if len(page1) < 28 || string(page1[:15]) != "SQLite format 3" {
		return 0, false
	}
	return binary.BigEndian.Uint32(page1[24:28]), true
}

// checkSource runs before a capture, inside a read transaction, and looks for
// a write this replica never saw. Two questions, both cheap:
//
//   - does the page shipped last still hold the bytes that were shipped? This
//     is Litestream's lastPageMatch. A page the tracker holds as dirty is
//     skipped, because a legitimate rewrite is exactly what a sync is for.
//   - did the change counter move while the dirty set stayed empty? A write
//     transaction always touches page 1, so a counter that moved without a
//     mark means the write went around the VFS.
//
// Either way there is no repair: pages this replica never saw are not in the
// dirty set and no later increment brings them. Saying so and starting a
// fresh generation is the whole remedy.
func (r *Replica) checkSource(ctx context.Context, db Database) error {
	pageSize := r.tracker.currentPageSize()
	if pageSize == 0 {
		return nil
	}
	last, counter, known := r.sourceWitness()
	if !known || last.pageSize != pageSize {
		return nil
	}
	dirty := r.tracker.dirtyCount()
	var mismatch string
	err := db.WithReadTransaction(ctx, func() error {
		seg, err := r.readPages(pageSize, []uint32{1})
		if err != nil {
			return err
		}
		if len(seg.Data) == 0 {
			return nil // an empty database has nothing to witness
		}
		if now, ok := changeCounter(seg.Data[0]); ok && now != counter && dirty == 0 {
			mismatch = fmt.Sprintf("the change counter moved from %d to %d while no page was marked", counter, now)
			return nil
		}
		if last.number == 0 || r.tracker.isDirty(last.number) {
			return nil
		}
		seg, err = r.readPages(pageSize, []uint32{last.number})
		if err != nil {
			return err
		}
		if len(seg.Data) == 0 {
			return nil // the page is past the end now: a shrink, not a stray write
		}
		if sha256.Sum256(seg.Data[0]) != last.hash {
			mismatch = fmt.Sprintf("page %d no longer holds the bytes that were shipped for it", last.number)
		}
		return nil
	})
	if err != nil {
		return err
	}
	if mismatch == "" {
		return nil
	}
	r.markerMu.Lock()
	r.marker.Complete = false
	writeErr := r.writeMarker(r.marker)
	// Say it once. The incomplete marker arms the remedy, a fresh generation
	// with a full snapshot, and that capture sets a new witness. Holding on to
	// the old one would make this guard refuse its own repair for ever.
	r.witnessKnown = false
	r.markerMu.Unlock()
	r.logger.Warn("replica: a write bypassed this replica; the next sync starts a fresh generation",
		"domain", r.domain, "generation", r.getMarker().Generation, "detail", mismatch)
	if writeErr != nil {
		return writeErr
	}
	return errForeignWrite
}

// witness is the page whose bytes the next sync checks against the source,
// with the page size it was read at: a database that changed page size gets a
// fresh snapshot anyway, and its old page numbers mean nothing.
type witness struct {
	number   uint32
	pageSize int
	hash     [32]byte
}

func (r *Replica) sourceWitness() (witness, uint32, bool) {
	r.markerMu.Lock()
	defer r.markerMu.Unlock()
	return r.witness, r.counter, r.witnessKnown
}

// rememberSource keeps what the next sync compares against: the last page of
// this capture and the change counter that belongs to it.
func (r *Replica) rememberSource(seg segment) {
	if len(seg.Pages) == 0 {
		return
	}
	index := len(seg.Pages) - 1
	r.markerMu.Lock()
	defer r.markerMu.Unlock()
	r.witness = witness{number: seg.Pages[index], pageSize: seg.PageSize, hash: sha256.Sum256(seg.Data[index])}
	for i, page := range seg.Pages {
		if page == 1 {
			if counter, ok := changeCounter(seg.Data[i]); ok {
				r.counter = counter
			}
		}
	}
	r.witnessKnown = true
}

// remoteTip is where the bucket stands: its current generation and the
// highest sequence that generation can restore to.
func (r *Replica) remoteTip(ctx context.Context) (generation string, seq int64, err error) {
	data, err := r.s3.Get(ctx, r.currentKey())
	if errors.Is(err, ErrNotFound) {
		return "", 0, nil
	}
	if err != nil {
		return "", 0, err
	}
	generation = strings.TrimSpace(string(data))
	if generation == "" {
		return "", 0, nil
	}
	l, err := r.loadLayout(ctx, generation)
	if err != nil {
		return generation, 0, err
	}
	plan, err := l.plan(time.Time{})
	if err != nil {
		return generation, 0, err
	}
	if len(plan) > 0 {
		seq = plan[len(plan)-1].Seq
	}
	return generation, seq, nil
}

// adoptLocal decides what happens when a database file is already there.
// Three outcomes, and the middle one is what this whole file is for:
//
//   - the local marker names the bucket's generation and is at or past its
//     tip: the database is current, it continues (the normal path);
//   - the local marker is behind that tip while it is complete and clean:
//     nothing was written since the commit it names, so writes this file once
//     had are missing, the replica is provably ahead and wins. This is
//     Litestream's checkDatabaseBehindReplica;
//   - the file has no replica state at all while the bucket holds a
//     generation: nothing orders the two, so neither is buried. It refuses,
//     names both, and the operator chooses. Measured on 22 September: a
//     database created from nothing was adopted as the truth, its fresh
//     snapshot became the current generation, and a day of real data sat one
//     pointer away in the bucket.
//
// A marker that is behind but interrupted (incomplete) or unclean is the
// fourth case, and it is not a database that lost writes: a commit reached
// the bucket while the marker write that records it did not, which is the
// window sync already guards with an incomplete marker. The file then holds
// that commit and everything written since, so it is ahead, not behind, and
// restoring would throw those writes away. interrupted says so: the file
// stays, and the next sync starts a fresh generation, because the sequence
// the bucket already holds may never be reused for other data.
func (r *Replica) adoptLocal(ctx context.Context, stored marker, usable bool) (restoreFrom string, interrupted bool, err error) {
	generation, seq, err := r.remoteTip(ctx)
	if err != nil || generation == "" {
		return "", false, err
	}
	if usable && stored.Generation == generation {
		if stored.Seq >= seq {
			return "", false, nil
		}
		if stored.Complete && stored.Clean {
			return generation, false, nil
		}
		return "", true, nil
	}
	if usable && stored.Generation > generation {
		// Generation ids are time ordered: ours started later, so this file
		// is the newer lineage and takes over with a fresh generation.
		return "", false, nil
	}
	if usable {
		return generation, false, nil
	}
	if r.config.AdoptLocalDatabase {
		r.logger.Warn("replica: adopting a database without replica state over the generation in the bucket, as configured",
			"domain", r.domain, "generation", generation)
		return "", false, nil
	}
	return "", false, fmt.Errorf("%w: generation %s holds %d commits; restore it, or set AdoptLocalDatabase to declare this file the new truth",
		errUnprovenDatabase, generation, seq)
}
