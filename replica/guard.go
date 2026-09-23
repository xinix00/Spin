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

// bucketTip is where the bucket stands: its current generation, the highest
// sequence that generation can restore to, the time of that commit, and the
// end of the last window compaction sealed.
type bucketTip struct {
	generation string
	seq        int64
	at         time.Time
	sealed     time.Time
	size       int64 // the database size the tip records
	bytes      int64 // what a restore of the tip reads
}

func (r *Replica) remoteTip(ctx context.Context) (bucketTip, error) {
	data, err := r.s3.Get(ctx, r.currentKey())
	if errors.Is(err, ErrNotFound) {
		return bucketTip{}, nil
	}
	if err != nil {
		return bucketTip{}, err
	}
	tip := bucketTip{generation: strings.TrimSpace(string(data))}
	if tip.generation == "" {
		return bucketTip{}, nil
	}
	l, err := r.loadLayout(ctx, tip.generation)
	if err != nil {
		return tip, err
	}
	plan, err := l.plan(time.Time{})
	if err != nil {
		return tip, err
	}
	if len(plan) > 0 {
		tip.seq, tip.at, tip.size = plan[len(plan)-1].Seq, plan[len(plan)-1].At, plan[len(plan)-1].MinSize
	}
	for _, m := range plan {
		for _, part := range m.Parts {
			tip.bytes += part.Size
		}
	}
	for _, windows := range l.windows {
		for _, w := range windows {
			if w.end.After(tip.sealed) {
				tip.sealed = w.end
			}
		}
	}
	return tip, nil
}

// adoption is what Prepare does with a database that is already here.
type adoption struct {
	// restoreFrom replaces the database with this generation: only for a
	// database with nothing unshipped whose bucket moved to a generation it
	// never made.
	restoreFrom string
	// continueAt is the marker to continue with: the bucket holds commits of
	// this database's own generation past its marker, which a stop between a
	// commit and the marker write leaves. The dirty log to continue with is
	// the one of logGeneration at logSeq.
	continueAt    *marker
	logGeneration string
	logSeq        int64
	// fresh says why a new generation starts from the database here.
	fresh string
}

// adoptLocal decides what happens when a database file is already there. The
// file is the truth for everything it wrote: SQLite and the lease make this
// process its only writer, so a commit in the bucket past the marker is one of
// its own whose marker write did not happen. Litestream's
// checkDatabaseBehindReplica does the same: it moves its own position to the
// replica's and never writes the replica over the database.
//
//   - the marker names the bucket's generation and is at or past its tip: it
//     continues (the normal path);
//   - it names that generation but is behind its tip: the dirty log still
//     names every page written since the marker, a superset of what those
//     commits carried, so the generation continues at the tip with them.
//     Without that log a new generation starts from the file;
//   - the bucket's generation is one this marker never made: with nothing
//     unshipped here the bucket wins (the file is a stale copy), with writes
//     here that the bucket does not have, nothing orders the two and it
//     refuses, naming both;
//   - the file has no replica state at all while the bucket holds a
//     generation: it refuses too. Measured on 22 September: a database
//     created from nothing was adopted as the truth, its fresh snapshot became
//     the current generation, and a day of real data sat one pointer away.
func (r *Replica) adoptLocal(ctx context.Context, stored marker, usable bool) (adoption, error) {
	tip, err := r.remoteTip(ctx)
	if err != nil || tip.generation == "" {
		return adoption{}, err
	}
	r.remoteGeneration, r.remoteSeq = tip.generation, tip.seq
	switch {
	case usable && stored.Generation == tip.generation && stored.Seq >= tip.seq:
		return adoption{}, nil
	case usable && stored.Generation == tip.generation:
		logGeneration, logSeq := stored.Generation, stored.Seq
		if _, err := r.tracker.log.read(logGeneration, logSeq); err != nil && stored.Previous != nil {
			// A renewal whose snapshot reached the bucket while its marker
			// write did not: the dirty log still follows the generation it
			// renewed, and names every page written since that one's last
			// commit, a superset of what changed after the snapshot.
			logGeneration, logSeq = stored.Previous.Generation, stored.Previous.Seq
		}
		if _, err := r.tracker.log.read(logGeneration, logSeq); err != nil {
			return adoption{fresh: fmt.Sprintf("the bucket holds commits of generation %s past this marker (%d of %d) and the dirty log cannot say what changed since", stored.Generation, stored.Seq, tip.seq)}, nil
		}
		next := stored
		next.Seq, next.Complete, next.Clean = tip.seq, true, false
		if next.PageSize == 0 && stored.Previous != nil {
			next.PageSize = stored.Previous.PageSize
		}
		if next.Size == 0 {
			next.Size = tip.size
		}
		next.Bytes = max(next.Bytes, tip.bytes)
		if tip.at.After(next.At) {
			next.At = tip.at
		}
		if tip.sealed.After(next.SealedAt) {
			next.SealedAt = tip.sealed
		}
		next.Previous = nil
		return adoption{continueAt: &next, logGeneration: logGeneration, logSeq: logSeq}, nil
	case usable && stored.Generation > tip.generation:
		// Generation ids are time ordered: ours started later, so this file
		// is the newer lineage (a renewal in progress, see Prepare).
		return adoption{}, nil
	case usable && stored.Complete && stored.Clean:
		return adoption{restoreFrom: tip.generation}, nil
	case usable:
		return adoption{}, fmt.Errorf("%w: this database holds writes of generation %s that the bucket's generation %s (%d commits) does not; restore that one, or set AdoptLocalDatabase to declare this file the new truth",
			errUnprovenDatabase, stored.Generation, tip.generation, tip.seq)
	case r.config.AdoptLocalDatabase:
		r.logger.Warn("replica: adopting a database without replica state over the generation in the bucket, as configured",
			"domain", r.domain, "generation", tip.generation)
		return adoption{}, nil
	}
	return adoption{}, fmt.Errorf("%w: generation %s holds %d commits; restore it, or set AdoptLocalDatabase to declare this file the new truth",
		errUnprovenDatabase, tip.generation, tip.seq)
}
