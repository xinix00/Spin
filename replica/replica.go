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
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

// A Replica keeps one database in object storage as complete, manifest-backed
// batches. A generation starts with a permanent snapshot and grows through
// incremental commits and compacted windows. See README.md for the wire layout.

// Database must exclude ALL writers for the duration of fn, including writes
// through other connections. Use rollback-journal mode: WAL is not supported.
// The adapter must read a database table before fn to acquire SQLite's shared lock.
// The single-connection adapter in the example also works with a lockless VFS.
type Database interface {
	WithReadTransaction(ctx context.Context, fn func() error) error
}

type Status struct {
	Enabled       bool      `json:"enabled"`
	Bucket        string    `json:"bucket,omitempty"`
	Generation    string    `json:"generation,omitempty"`
	Complete      bool      `json:"complete"`
	Restored      bool      `json:"restored,omitempty"`
	LastSyncAt    time.Time `json:"last_sync_at,omitempty"`
	LastError     string    `json:"last_error,omitempty"`
	PendingPages  int       `json:"pending_pages"`
	UploadedBytes int64     `json:"uploaded_bytes"`
	// Copy is how far a whole-database copy has come while one runs.
	Copy *CopyProgress `json:"copy,omitempty"`
}

// CopyProgress is how far a whole-database copy has come. It has two stages:
// "read" puts the database's pages in the local spool, "upload" sends the
// spool to the bucket. StartedAt is the start of the stage, on the wall
// clock, so a page can tell the rate and what is left.
type CopyProgress struct {
	Stage     string    `json:"stage"`
	Done      int64     `json:"done"`
	Total     int64     `json:"total"`
	StartedAt time.Time `json:"started_at"`
}

// marker is the replica's own state, next to the database on storage. Clean
// means every write reached a segment; a start with an unclean marker knows
// pages may be missing and begins a new generation.
type marker struct {
	Destination string    `json:"destination"`
	PageSize    int       `json:"page_size"`
	Version     int       `json:"version"`
	At          time.Time `json:"at"`
	SealedAt    time.Time `json:"sealed_at"`
	Generation  string    `json:"generation"`
	Seq         int64     `json:"seq"`
	Size        int64     `json:"size"`
	Bytes       int64     `json:"bytes"`
	Complete    bool      `json:"complete"`
	Clean       bool      `json:"clean"`
	StartedAt   time.Time `json:"started_at"`
	// Previous is the complete generation a new one renews, kept until the
	// new one's first commit: a restart in the middle of the copy continues
	// it instead of copying everything again (Prepare).
	Previous *marker `json:"previous,omitempty"`
	// RepairFrom records our damaged generation while a replacement snapshot
	// is in progress. It proves local lineage at restart, but is never a
	// fallback to resume. Clear it only after publishing the replacement.
	RepairFrom string `json:"repair_from,omitempty"`
	// Uncertain is a commit whose manifest PUT has no known outcome: an error
	// after the request left, or a stop right after it. The next sync looks
	// in the bucket whether it is there, and counts it or tries its sequence
	// again. A failed PUT used to end the generation, so one 503 from the
	// object store cost a copy of the whole database.
	Uncertain int64 `json:"uncertain,omitempty"`
}

// renewal is the marker of a new generation that takes over from current.
func (r *Replica) renewal(current marker) marker {
	next := marker{Version: formatVersion, Generation: newGenerationID(r.now()), StartedAt: r.now(), RepairFrom: current.RepairFrom}
	switch {
	case current.Complete:
		current.Previous = nil
		next.Previous = &current
	case current.Previous != nil:
		// A copy that failed in this process is tried again; what it renews
		// has not changed.
		next.Previous = current.Previous
	}
	return next
}

type Replica struct {
	// Progress, when set, hears what a long step of Prepare is doing, for
	// a page that waits on it.
	Progress func(message string)
	// OnCopy, when set, hears how far a whole-database copy has come: the
	// one before a Spin opens and a renewal while it serves alike.
	OnCopy func(CopyProgress)
	// freshReason says why Prepare started a new generation, for the page
	// that waits on the copy it makes.
	freshReason string
	// What the next sync compares the source against (guard.go). Under
	// markerMu, like the marker it belongs with.
	witness      witness
	counter      uint32
	witnessKnown bool
	verify       func(path string) error
	config       Config
	s3           ObjectStore
	domain       string
	path         string
	inner        vfs.VFS
	files        Storage
	tracker      *tracker
	vfsName      string
	logger       *slog.Logger

	restoreMu sync.Mutex
	archiveMu sync.RWMutex
	markerMu  sync.Mutex
	marker    marker

	mu          sync.Mutex
	db          Database
	syncing     bool
	closed      bool
	active      sync.WaitGroup
	lifetime    context.Context
	cancel      context.CancelFunc
	status      Status
	stopOnce    sync.Once
	lastCompact time.Time
	// renewAfter holds a renewal back after one failed: meanwhile the
	// generation it renews goes on shipping changes.
	renewAfter time.Time
	// now is the clock; tests move it.
	now func() time.Time
}

var registered sync.Map

// Options supplies host dependencies. Zero fields select the standard adapters.
type Options struct {
	Objects ObjectStore
	Storage Storage
	Now     func() time.Time
	// Verify, when set, is asked whether a freshly composed database is
	// sound, before it is published over the live path. The host opens that
	// path read-only through the same VFS it gave this replica and runs
	// PRAGMA quick_check; this package stays free of a SQLite driver.
	// Litestream carries the same idea as IntegrityCheckQuick in its restore
	// path, and it is the last of the four gates: coverage says the pages are
	// all there, this says SQLite agrees.
	Verify func(path string) error
}

// New prepares a replica and registers the VFS that SQLite must use.
func New(config Config, domain, path string, inner vfs.VFS, logger *slog.Logger) (*Replica, error) {
	return NewWithOptions(config, domain, path, inner, logger, Options{})
}

// NewWithOptions accepts replacement storage, object storage and clock adapters.
func NewWithOptions(config Config, domain, path string, inner vfs.VFS, logger *slog.Logger, options Options) (*Replica, error) {
	config = config.withDefaults()
	if err := config.validate(); err != nil {
		return nil, err
	}
	config.Schedule = append([]Level(nil), config.Schedule...)
	if options.Objects == nil && (strings.TrimSpace(config.Endpoint) == "" || strings.TrimSpace(config.Bucket) == "") {
		return nil, errors.New("replica needs an S3 endpoint and bucket")
	}
	if inner == nil {
		return nil, errors.New("replica needs a storage VFS")
	}
	full := fullPath(inner, path)
	name := "replica-" + safeName(domain)
	if _, taken := registered.LoadOrStore(name, true); taken {
		return nil, fmt.Errorf("replica for %s is already registered", domain)
	}
	if logger == nil {
		logger = slog.New(slog.NewTextHandler(io.Discard, nil))
	}
	replica := &Replica{
		config: config, domain: domain, path: full, inner: inner, files: storageFor(inner), vfsName: name, logger: logger,
		s3:     &S3{Endpoint: config.Endpoint, Bucket: config.Bucket, Region: config.Region, AccessKey: config.AccessKey, SecretKey: config.SecretKey},
		status: Status{Enabled: true, Bucket: config.Bucket},
		now:    func() time.Time { return time.Now().UTC() },
	}
	if options.Objects != nil {
		replica.s3 = options.Objects
	}
	if options.Storage != nil {
		replica.files = options.Storage
	}
	if options.Now != nil {
		replica.now = options.Now
	}
	replica.verify = options.Verify
	replica.lifetime, replica.cancel = context.WithCancel(context.Background())
	replica.tracker = newTracker(newDirtyLog(replica.files, full))
	replica.tracker.onUnclean = replica.markUnclean
	vfs.Register(name, &trackingVFS{inner: inner, main: full, tracker: replica.tracker})
	return replica, nil
}

// VFSName is the VFS SQLite opens the database with.
func (r *Replica) VFSName() string { return r.vfsName }

func safeName(value string) string {
	var out strings.Builder
	for _, char := range strings.ToLower(value) {
		if (char >= 'a' && char <= 'z') || (char >= '0' && char <= '9') || char == '.' || char == '-' {
			out.WriteRune(char)
		} else {
			out.WriteByte('_')
		}
	}
	return out.String()
}

func (r *Replica) key(parts ...string) string {
	return strings.Join(append([]string{r.config.Prefix, r.domain}, parts...), "/")
}

func (r *Replica) currentKey() string { return r.key("current") }

func (r *Replica) generationPrefix(generation string) string {
	return r.key("generations", generation) + "/"
}

// restoreCurrent fetches a generation over the database path and takes it as
// the local state. Both callers of it are in Prepare: a database that is not
// there, and one the bucket is provably ahead of (guard.go).
func (r *Replica) restoreCurrent(ctx context.Context, generation string) error {
	restored, err := r.restoreInto(ctx, generation, time.Time{}, r.path)
	if err != nil {
		return fmt.Errorf("restore generation %s: %w", generation, err)
	}
	if err := r.setMarker(restored); err != nil {
		return err
	}
	if err := r.tracker.rewriteLog(restored.Generation, restored.Seq); err != nil {
		return fmt.Errorf("start the dirty log: %w", err)
	}
	r.mu.Lock()
	r.status.Restored, r.status.Generation, r.status.Complete = true, generation, true
	r.mu.Unlock()
	r.logger.Info("replica: database restored from the bucket", "domain", r.domain, "generation", generation, "segments", restored.Seq, "bytes", restored.Size)
	return nil
}

// Prepare runs before the database opens: a missing database is restored
// from the current generation; an existing one continues its generation
// when the marker says every write was shipped, and starts a new one else.
func (r *Replica) Prepare(ctx context.Context) error {
	exists, err := r.files.Exists(r.path)
	if err != nil {
		return err
	}
	interrupted, err := r.files.Exists(r.path + ".replica-restoring")
	if err != nil {
		return err
	}
	if !exists || interrupted {
		current, err := r.s3.Get(ctx, r.currentKey())
		if errors.Is(err, ErrNotFound) {
			if interrupted {
				return errors.New("interrupted restore has no current generation")
			}
			generations, err := r.generationIDs(ctx)
			if err != nil {
				return fmt.Errorf("check archive before empty database bootstrap: %w", err)
			}
			if len(generations) > 0 {
				return errors.New("current generation pointer is missing but archived generations exist; refusing to start an empty database")
			}
			r.logger.Info("replica: no generation in the bucket; the database starts empty", "domain", r.domain)
			return r.tracker.rewriteLog("", 0)
		}
		if err != nil {
			return fmt.Errorf("read current generation: %w", err)
		}
		return r.restoreCurrent(ctx, strings.TrimSpace(string(current)))
	}
	stored, err := r.readMarker()
	switch {
	case err != nil:
		// A file without provenance never buries a generation that is in
		// the bucket (guard.go).
		if _, guardErr := r.adoptLocal(ctx, stored, false); guardErr != nil {
			return guardErr
		}
		r.logger.Warn("replica: no usable marker next to the database; a new generation starts", "domain", r.domain, "error", err)
		r.freshReason = "no usable marker next to the database"
		return r.tracker.rewriteLog("", 0)
	case stored.Version != formatVersion:
		r.logger.Warn("replica: legacy local marker; starting a generation with commit manifests", "domain", r.domain)
		return r.tracker.rewriteLog("", 0)
	case stored.Destination != r.destinationID():
		r.logger.Info("replica: object-store destination changed; starting a fresh generation", "domain", r.domain)
		return r.tracker.rewriteLog("", 0)
	}
	// Before a database that is already here is taken as the truth: does the
	// bucket hold a generation this file cannot account for (guard.go)? The
	// dirty log names the pages written since the last sync, and the
	// generation continues with those; a clean marker means none, and
	// whatever the log names then is shipped once more.
	decision, err := r.adoptLocal(ctx, stored, true)
	if err != nil {
		return err
	}
	switch {
	case decision.restoreFrom != "":
		r.logger.Warn("replica: the bucket moved to a generation this database never made, and it has nothing unshipped; restoring that generation",
			"domain", r.domain, "generation", decision.restoreFrom, "local_generation", stored.Generation, "local_seq", stored.Seq)
		return r.restoreCurrent(ctx, decision.restoreFrom)
	case decision.continueAt != nil:
		return r.continueGeneration(*decision.continueAt, decision.pages)
	}
	r.logger.Warn("replica: "+decision.fresh+"; a new generation is copied in the background", "domain", r.domain)
	r.freshReason = decision.fresh
	if decision.repairFrom != "" {
		stored.Complete, stored.Clean = false, false
		stored.Previous = nil
		stored.RepairFrom = decision.repairFrom
		if err := r.setMarker(stored); err != nil {
			return err
		}
	}
	return r.tracker.rewriteLog("", 0)
}

// continueGeneration makes stored the marker and pages the pages to ship
// next: a start that goes on where the last one stopped.
func (r *Replica) continueGeneration(stored marker, pages []uint32) error {
	if len(pages) > 0 {
		// markPages makes the tracker unclean. Its marker must agree even
		// when these are conservative replays of committed pages; otherwise
		// the next real write skips onUnclean and leaves Clean=true on disk.
		stored.Clean = false
	}
	if err := r.setMarker(stored); err != nil {
		return err
	}
	r.tracker.markPages(pages)
	if err := r.tracker.rewriteLog(stored.Generation, stored.Seq); err != nil {
		return fmt.Errorf("start the dirty log: %w", err)
	}
	r.mu.Lock()
	r.status.Generation, r.status.Complete = stored.Generation, stored.Complete
	r.mu.Unlock()
	if !stored.Clean {
		r.logger.Info("replica: the database changed after its last sync; the generation continues with the pages the dirty log names", "domain", r.domain, "generation", stored.Generation, "pages", len(pages))
	}
	return nil
}

// SnapshotDue reports whether the next sync copies the whole database.
func (r *Replica) SnapshotDue() bool {
	return r.SnapshotReason() != ""
}

// SnapshotReason says why the next sync copies the whole database, or is
// empty when it does not: no generation yet, an interrupted generation,
// a generation past its age, or more shipped than the database is worth.
func (r *Replica) SnapshotReason() string {
	return r.snapshotReason(r.getMarker())
}

func (r *Replica) snapshotReason(current marker) string {
	now := r.now()
	switch {
	case current.Generation == "" && r.freshReason != "":
		return r.freshReason
	case current.Generation == "":
		return "no generation yet"
	case !current.Complete:
		return "the last sync of generation " + current.Generation + " did not complete"
	case r.tracker.currentPageSize() != 0 && current.PageSize != r.tracker.currentPageSize():
		// Old page numbers mean nothing at the new size (VACUUM).
		return fmt.Sprintf("the page size changed from %d to %d", current.PageSize, r.tracker.currentPageSize())
	case now.Before(r.renewAfter):
		return ""
	case now.Sub(current.StartedAt) > r.config.Generation:
		return fmt.Sprintf("generation %s is %s old, the limit is %s", current.Generation, now.Sub(current.StartedAt).Round(time.Hour), r.config.Generation)
	case current.Bytes > 2*current.Size+64<<20:
		return fmt.Sprintf("generation %s shipped %d MiB against a database of %d MiB", current.Generation, current.Bytes>>20, current.Size>>20)
	}
	return ""
}

// Attach supplies the database adapter. Start or Sync drives replication.
func (r *Replica) Attach(db Database) {
	r.mu.Lock()
	r.db = db
	r.mu.Unlock()
}

func (r *Replica) Start(ctx context.Context) {
	go func() {
		ticker := time.NewTicker(r.config.Interval)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-r.lifetime.Done():
				return
			case <-ticker.C:
				if err := r.Sync(ctx); err != nil && ctx.Err() == nil {
					r.logger.Warn("replica: sync", "domain", r.domain, "error", err)
				}
			}
		}
	}()
}

// Close stops the loop and frees the VFS name, so the tenant can be opened
// again in the same process.
func (r *Replica) Close() {
	r.stopOnce.Do(func() {
		r.mu.Lock()
		r.closed = true
		r.cancel()
		r.mu.Unlock()
		r.active.Wait()
		r.tracker.log.close()
		vfs.Unregister(r.vfsName)
		registered.Delete(r.vfsName)
	})
}

func (r *Replica) Status() Status {
	r.mu.Lock()
	defer r.mu.Unlock()
	status := r.status
	if status.Copy != nil {
		progress := *status.Copy
		status.Copy = &progress
	}
	status.PendingPages = r.tracker.pendingPages()
	return status
}

// Sync ships what changed: one pass, segments of at most SegmentBytes. A
// whole-database copy it needs goes in short transactions while the Spin
// serves. Like Litestream, nothing waits for a copy: the database here holds
// every write, and the bucket keeps the generation before until the new one
// is complete.
func (r *Replica) Sync(ctx context.Context) error {
	r.mu.Lock()
	if r.closed {
		r.mu.Unlock()
		return errors.New("replica is closed")
	}
	if r.syncing || r.db == nil {
		r.mu.Unlock()
		return nil
	}
	r.syncing = true
	r.active.Add(1)
	defer r.active.Done()
	db := r.db
	r.mu.Unlock()
	ctx, cancel := context.WithCancel(ctx)
	stopCancel := context.AfterFunc(r.lifetime, cancel)
	defer func() { stopCancel(); cancel() }()
	err := r.sync(ctx, db)
	archiveDamaged := errors.Is(err, errReplicaCorrupt) || errors.Is(err, errCommitGap)
	if archiveDamaged || errors.Is(err, errForeignWrite) || errors.Is(err, errGenerationShort) {
		// All detectors report their cause here; one transition schedules
		// repair. A source mismatch during an unfinished repair must retain
		// its original lineage; damage to a published replacement updates it.
		var damaged string
		if archiveDamaged {
			damaged = r.getMarker().Generation
		}
		writeErr := r.invalidateGeneration(damaged)
		r.logger.Warn("replica: the generation cannot continue; the next sync starts a fresh generation", "domain", r.domain, "generation", r.getMarker().Generation, "error", err)
		err = errors.Join(err, writeErr)
	}
	r.mu.Lock()
	r.syncing = false
	if err != nil {
		r.status.LastError = err.Error()
	} else {
		r.status.LastError = ""
		r.status.LastSyncAt = r.now()
	}
	r.mu.Unlock()
	return err
}

// renewalBackoff is how long a failed renewal waits before it is tried again.
const renewalBackoff = 10 * time.Minute

// resolveUncertain settles a commit whose manifest PUT has no known outcome:
// there, it counts, and the pages it carried go again with the next commit
// (they were put back when the sync failed); not there, its sequence is free.
func (r *Replica) resolveUncertain(ctx context.Context, current marker) (marker, error) {
	prefix := r.generationPrefix(current.Generation) + fmt.Sprintf("L0/%012d-", current.Uncertain)
	objects, err := r.s3.List(ctx, prefix)
	if err != nil {
		return current, err
	}
	landed := false
	for _, object := range objects {
		if !strings.HasSuffix(object.Key, ".json") {
			continue
		}
		m, err := r.getManifest(ctx, current.Generation, object.Key)
		if err != nil {
			return current, err
		}
		landed = true
		current.Seq = max(current.Seq, m.Seq)
		if m.At.After(current.At) {
			current.At = m.At
		}
	}
	r.logger.Info("replica: a commit whose outcome was unknown is settled", "domain", r.domain, "generation", current.Generation, "seq", current.Uncertain, "in_bucket", landed)
	current.Uncertain = 0
	if err := r.setMarker(current); err != nil {
		return current, err
	}
	return current, nil
}

// uploadParts sends the spooled parts to the bucket, a few at a time: one PUT
// after another leaves the line idle for a round trip per segment, which is
// most of the time a copy of gigabytes takes. The refs keep the spool order.
func (r *Replica) uploadParts(ctx context.Context, captured capture, prefix string) ([]partRef, error) {
	refs := make([]partRef, len(captured.parts))
	var total int64
	var done atomic.Int64
	for _, part := range captured.parts {
		total += int64(part.length)
	}
	err := each(ctx, r.config.UploadParallelism, len(captured.parts), func(ctx context.Context, index int) error {
		part := captured.parts[index]
		data, err := r.readSpool(captured.path, part)
		if err != nil {
			return err
		}
		ref := partRef{Key: fmt.Sprintf("%s%06d.seg", prefix, index+1), Size: int64(len(data)), Hash: sha256hex(data)}
		if err := r.s3.Put(ctx, ref.Key, data); err != nil {
			return err
		}
		refs[index] = ref
		if captured.snapshot {
			r.reportCopy("upload", done.Add(int64(part.length)), total)
		}
		return nil
	})
	return refs, err
}

// each calls fn for every index below count, workers at a time, and stops
// at the first error, which it returns.
func each(ctx context.Context, workers, count int, fn func(ctx context.Context, index int) error) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	var (
		mu       sync.Mutex
		next     int
		firstErr error
		group    sync.WaitGroup
	)
	for worker := 0; worker < min(workers, count); worker++ {
		group.Add(1)
		go func() {
			defer group.Done()
			for {
				mu.Lock()
				index, stop := next, next >= count || firstErr != nil
				next++
				mu.Unlock()
				if stop {
					return
				}
				if err := fn(ctx, index); err != nil {
					mu.Lock()
					if firstErr == nil {
						firstErr = err
						cancel()
					}
					mu.Unlock()
					return
				}
			}
		}()
	}
	group.Wait()
	return firstErr
}

// reportCopy records how far a whole-database copy has come and tells OnCopy.
func (r *Replica) reportCopy(stage string, done, total int64) {
	r.mu.Lock()
	if r.status.Copy == nil || r.status.Copy.Stage != stage {
		r.status.Copy = &CopyProgress{Stage: stage, StartedAt: time.Now().UTC()}
	}
	r.status.Copy.Done, r.status.Copy.Total = done, total
	progress := *r.status.Copy
	hook := r.OnCopy
	r.mu.Unlock()
	if hook != nil {
		hook(progress)
	}
}

func (r *Replica) endCopy() {
	r.mu.Lock()
	r.status.Copy = nil
	r.mu.Unlock()
}

func (r *Replica) sync(ctx context.Context, db Database) error {
	defer r.endCopy()
	// A snapshot/current PUT may have succeeded even when both its reply
	// and the immediate read-back failed. Ask the bucket what a start asks
	// (guard.go): the attempt goes on at its tip, or the generation it
	// renews does; never fall back blindly while current may already name
	// the new snapshot.
	if current := r.getMarker(); !current.Complete && current.Seq == 0 && current.Generation != "" {
		decision, err := r.adoptLocal(ctx, current, true)
		if err != nil {
			return err
		}
		if decision.repairFrom != "" {
			// An uncertain publication may have moved current to this
			// attempt before its snapshot became damaged. Preserve that
			// proven lineage when starting the next replacement.
			if err := r.invalidateGeneration(decision.repairFrom); err != nil {
				return err
			}
		}
		if decision.continueAt != nil {
			if err := r.continueGeneration(*decision.continueAt, decision.pages); err != nil {
				return err
			}
		}
	}
	// Is the source still the database this replica has been following? A
	// write that went around the tracking VFS leaves pages nobody will ever
	// ship, so it has to end this generation rather than poison it (guard.go).
	if err := r.checkSource(ctx, db); err != nil {
		return err
	}
	current := r.getMarker()
	var publicationErr error
	if current.Complete && current.Uncertain != 0 {
		resolved, err := r.resolveUncertain(ctx, current)
		if err != nil {
			return err
		}
		current = resolved
	}
	// An incomplete attempt is never resumed: a fresh snapshot and fresh keys
	// also make a timeout after a successful PUT safe to retry.
	reason := r.snapshotReason(current)
	fresh := reason != ""
	renewed := false
	if fresh {
		current = r.renewal(current)
		if err := r.setMarker(current); err != nil {
			return err
		}
		if current.Previous != nil {
			// A renewal that does not finish leaves the generation it renews
			// as it was: that one goes on with what changed since its last
			// commit, and the renewal waits before it is tried again. Without
			// this, one failed upload meant the next sync copied everything
			// again, and a copy that keeps failing meant nothing shipped.
			previous := *current.Previous
			defer func() {
				if renewed || !r.tracker.resetToLog() {
					return
				}
				previous.Clean = false
				if err := r.setMarker(previous); err != nil {
					return
				}
				r.renewAfter = r.now().Add(renewalBackoff)
				r.logger.Warn("replica: the renewal did not finish; the generation it renews goes on", "domain", r.domain, "generation", previous.Generation, "retry_after", renewalBackoff)
			}()
		}
	}
	started := r.now()
	if fresh {
		r.logger.Info("replica: snapshot copy starts in the background; the database stays usable", "domain", r.domain, "generation", current.Generation, "reason", reason)
	}
	captured, err := r.capture(ctx, db, fresh)
	if err != nil {
		return err
	}
	defer captured.close(r.files)
	if captured.snapshot {
		r.logger.Info("replica: snapshot copied; the upload starts", "domain", r.domain, "pages", captured.size/int64(captured.pageSize), "bytes", captured.size, "took", r.now().Sub(started).Round(time.Millisecond))
	}
	// Do not commit growth whose pages this capture cannot account for.
	// Sync applies the same repair transition as for other broken chains.
	if !captured.snapshot {
		if first, missing, gap := growthGap(current.Size, captured.size, captured.pageSize, captured.pages); gap {
			r.tracker.putBack(captured.pages)
			return fmt.Errorf("%w: %d missing pages, first %d, size %d (previous %d)", errGenerationShort, missing, first, captured.size, current.Size)
		}
	}
	if len(captured.parts) > 0 {
		// Monotone times keep new batches out of already sealed time windows.
		at := captured.at
		if !at.After(current.At) {
			at = current.At.Add(time.Nanosecond)
		}
		// Sealed windows end at SealedAt and include that instant; a new
		// commit lands after it.
		if !at.After(current.SealedAt) {
			at = current.SealedAt.Add(time.Nanosecond)
		}
		m := manifest{MinSize: captured.minSize, Version: formatVersion, FirstSeq: current.Seq + 1, Seq: current.Seq + 1, At: at}
		prefix := r.generationPrefix(current.Generation) + "data/" + newGenerationID(r.now()) + "/"
		committed, publishing := false, false
		defer func() {
			if committed {
				return
			}
			r.tracker.putBack(captured.pages)
			if publishing {
				_ = r.updateMarker(func(mk *marker) {
					if fresh {
						mk.Complete = false
					} else {
						mk.Uncertain = m.Seq
					}
					mk.Clean = false
				})
			}
		}()
		refs, err := r.uploadParts(ctx, captured, prefix)
		if err != nil {
			return err
		}
		m.Parts = refs
		key := r.rawKey(current.Generation, m.Seq, m.At)
		if fresh {
			key = r.snapshotKey(current.Generation)
		}
		// From here on the server may have accepted the commit despite a lost
		// response, so its sequence is never reused blindly: the deferred
		// marker write above records it as uncertain and the next sync looks
		// whether it is there (resolveUncertain). A snapshot that may not be
		// there ends its generation instead.
		publishing = true
		if err := r.putManifest(ctx, key, m); err != nil {
			return err
		}
		if fresh {
			if err := r.s3.Put(ctx, r.currentKey(), []byte(current.Generation)); err != nil {
				remote, readErr := r.s3.Get(ctx, r.currentKey())
				switch {
				case readErr == nil && strings.TrimSpace(string(remote)) == current.Generation:
					// The pointer did move: finish this generation's local
					// bookkeeping, reporting the failed request afterwards.
					publicationErr = err
				case readErr != nil && !errors.Is(readErr, ErrNotFound):
					// Unknown outcome: do not run the fallback to Previous.
					// The incomplete marker can be resolved here or at restart.
					renewed = true
					return errors.Join(err, readErr)
				default:
					return err
				}
			}
			renewed = true
		}
		current.Seq = m.Seq
		current.At = m.At
		current.Size = captured.size
		current.PageSize = captured.pageSize
		current.Complete = true
		current.Clean = false
		current.Previous = nil
		current.RepairFrom = ""
		for _, part := range m.Parts {
			current.Bytes += part.Size
		}
		committed = true
		if err := r.setMarker(current); err != nil {
			// The commit is in the bucket; only its record here failed. Memory
			// keeps the truth and the next sync goes on from it; a start that
			// finds the older marker continues at the bucket's tip (guard.go).
			return err
		}
		if err := r.tracker.rewriteLog(current.Generation, current.Seq); err != nil {
			// Before header publication the old log remains authoritative;
			// after an uncertain header publication the candidate stays active
			// and subsequent writes persist an invalidation before DB sync.
			r.logger.Warn("replica: rewrite the dirty log", "domain", r.domain, "error", err)
		}
		if captured.snapshot {
			r.logger.Info("replica: snapshot uploaded", "domain", r.domain, "segments", len(m.Parts), "bytes", current.Bytes, "took", r.now().Sub(started).Round(time.Millisecond))
		}
		r.mu.Lock()
		r.status.Generation = current.Generation
		r.status.Complete = true
		for _, part := range m.Parts {
			r.status.UploadedBytes += part.Size
		}
		r.mu.Unlock()
	}
	if current.Complete {
		persistClean := func() error { return r.updateMarker(func(m *marker) { m.Clean = true }) }
		if err := r.tracker.settle(captured.revision, persistClean); err != nil {
			return err
		}
	}
	if fresh && current.Complete {
		r.pruneGenerations(ctx, current.Generation)
	}
	if current.Complete && r.now().Sub(r.lastCompact) >= time.Minute {
		// Persist the frontier before merging; after restart no commit can land in
		// a window which may already have been published.
		frontier := r.now()
		if err := r.updateMarker(func(m *marker) {
			if frontier.After(m.SealedAt) {
				m.SealedAt = frontier
			}
		}); err != nil {
			return err
		}
		// A failed compaction waits its turn like a successful one: it reads
		// every manifest of the generation, and retrying it every sync did
		// that every 15 seconds.
		r.lastCompact = frontier
		if err := r.compact(ctx, current.Generation, frontier); err != nil {
			return fmt.Errorf("compact: %w", err)
		}
	}
	return publicationErr
}

// readPages reads the pages that still exist, contiguous ones in one read:
// on HopOS every read is a call into the system, and a snapshot is
// hundreds of thousands of pages.
func (r *Replica) readPages(pageSize int, pages []uint32) (segment, error) {
	file, err := r.files.Open(r.path, false)
	if err != nil {
		return segment{}, err
	}
	defer file.Close()
	size, err := file.Size()
	if err != nil {
		return segment{}, err
	}
	seg := segment{PageSize: pageSize, DBSize: size}
	last := uint32(size / int64(pageSize))
	for index := 0; index < len(pages); {
		first := pages[index]
		if first > last {
			index++
			continue
		}
		count := 1
		for index+count < len(pages) && pages[index+count] == first+uint32(count) && first+uint32(count) <= last && count*pageSize < readRunBytes {
			count++
		}
		data := make([]byte, count*pageSize)
		read, err := file.ReadAt(data, int64(first-1)*int64(pageSize))
		if err != nil && !errors.Is(err, io.EOF) {
			return segment{}, err
		}
		if read != len(data) {
			return segment{}, io.ErrUnexpectedEOF
		}
		for page := 0; page < count; page++ {
			seg.Pages = append(seg.Pages, first+uint32(page))
			seg.Data = append(seg.Data, data[page*pageSize:(page+1)*pageSize])
		}
		index += count
	}
	return seg, nil
}

// readRunBytes bounds one read of contiguous pages.
const readRunBytes = 4 << 20

func (r *Replica) fileSize() (int64, error) {
	file, err := r.files.Open(r.path, false)
	if err != nil {
		return 0, err
	}
	defer file.Close()
	return file.Size()
}

func validGeneration(id string) bool {
	if len(id) < 17 || generationTime(id).IsZero() {
		return false
	}
	return !strings.ContainsAny(id, "/\\ ")
}

// generationTime reads the moment out of a generation id.
func generationTime(id string) time.Time {
	stamp, _, _ := strings.Cut(id, "-")
	created, err := time.Parse("20060102T150405Z", stamp)
	if err != nil {
		return time.Time{}
	}
	return created
}

// pruneGenerations removes generations past the retention, and unfinished
// ones older than a day that are not this one (a snapshot another start
// never completed). The one kept always stays. It runs on the sync's context:
// a stop interrupts it, which leaves orphaned data at worst, and the next
// generation start tries again.
func (r *Replica) pruneGenerations(ctx context.Context, keep string) {
	r.archiveMu.Lock()
	defer r.archiveMu.Unlock()
	objects, err := r.s3.List(ctx, r.key("generations")+"/")
	if err != nil {
		r.logger.Warn("replica: list generations", "domain", r.domain, "error", err)
		return
	}
	now := r.now()
	complete := map[string]bool{}
	for _, object := range objects {
		if strings.HasSuffix(object.Key, "/snapshot") {
			rest := strings.TrimPrefix(object.Key, r.key("generations")+"/")
			id, _, _ := strings.Cut(rest, "/")
			complete[id] = true
		}
	}
	// Visibility goes first: a generation's snapshot manifest, then its
	// other manifests, then the data. An interrupted removal then leaves
	// only orphaned data, never a point that is advertised but cannot be
	// fetched.
	rank := func(key string) int {
		switch {
		case strings.HasSuffix(key, "/snapshot"):
			return 0
		case !strings.Contains(key, "/data/"):
			return 1
		default:
			return 2
		}
	}
	sort.SliceStable(objects, func(i, j int) bool { return rank(objects[i].Key) < rank(objects[j].Key) })
	removed, failed := 0, 0
	blocked := map[string]bool{}
	for _, object := range objects {
		rest := strings.TrimPrefix(object.Key, r.key("generations")+"/")
		id, _, _ := strings.Cut(rest, "/")
		if id == keep || blocked[id] {
			continue
		}
		created := generationTime(id)
		expired := !created.IsZero() && created.Before(now.Add(-r.config.Retention))
		abandoned := !complete[id] && !created.IsZero() && created.Before(now.Add(-24*time.Hour))
		if !expired && !abandoned {
			continue
		}
		if err := r.s3.Delete(ctx, object.Key); err != nil {
			failed++
			blocked[id] = true // Keep every dependency if removing visibility failed.
			r.logger.Warn("replica: delete old segment", "domain", r.domain, "key", object.Key, "error", err)
			continue
		}
		removed++
	}
	if failed > 0 {
		r.logger.Warn("replica: old generations not fully removed; the next generation start tries again", "domain", r.domain, "failed", failed)
	}
	if removed > 0 {
		r.logger.Info("replica: old generations removed", "domain", r.domain, "files", removed)
	}
}

func newGenerationID(now time.Time) string {
	var random [16]byte
	_, _ = rand.Read(random[:])
	return now.UTC().Format("20060102T150405Z") + "-" + hex.EncodeToString(random[:])
}

// The marker lives next to the database on the same storage.

func (r *Replica) markerPath() string { return r.path + ".replica" }

func (r *Replica) getMarker() marker {
	r.markerMu.Lock()
	defer r.markerMu.Unlock()
	return r.marker
}

// invalidateGeneration retains provenance but never lets a failed repair fall
// back to the damaged chain. Drop its witness so it cannot veto the repair.
func (r *Replica) invalidateGeneration(damaged string) error {
	return r.updateMarker(func(m *marker) {
		m.Complete, m.Previous = false, nil
		if damaged != "" {
			m.RepairFrom = damaged
		} else if m.RepairFrom == "" {
			m.RepairFrom = m.Generation
		}
		r.witnessKnown = false
	})
}

// A local clean marker only applies to the object-store namespace it synced.
func (r *Replica) destinationID() string {
	return sha256hex([]byte(strings.Join([]string{r.config.Endpoint, r.config.Bucket, r.config.Prefix, r.domain}, "\x00")))
}

func (r *Replica) setMarker(value marker) error {
	value.Destination = r.destinationID()
	return r.updateMarker(func(m *marker) { *m = value })
}

func (r *Replica) markUnclean() error {
	return r.updateMarker(func(m *marker) { m.Clean = false })
}

// updateMarker changes the marker under its lock and writes it. Memory keeps
// the change even when the write fails, unclean: what was decided stays
// decided (a commit is in the bucket whether its record here landed or not),
// and a failed clean write must never leave a clean state.
func (r *Replica) updateMarker(change func(*marker)) error {
	r.markerMu.Lock()
	defer r.markerMu.Unlock()
	next := r.marker
	change(&next)
	err := r.writeMarker(next)
	if err != nil {
		next.Clean = false
	}
	r.marker = next
	return err
}

func (r *Replica) writeMarker(value marker) error {
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	return r.writeLocal(r.markerPath(), data)
}

func (r *Replica) readMarker() (marker, error) {
	file, err := r.files.Open(r.markerPath(), false)
	if err != nil {
		return marker{}, err
	}
	defer file.Close()
	size, err := file.Size()
	if err != nil {
		return marker{}, err
	}
	if size <= 0 || size > 1<<16 {
		return marker{}, errors.New("marker has an odd size")
	}
	data := make([]byte, size)
	if _, err := file.ReadAt(data, 0); err != nil && !errors.Is(err, io.EOF) {
		return marker{}, err
	}
	var value marker
	if err := json.Unmarshal(data, &value); err != nil {
		return marker{}, err
	}
	return value, nil
}

// Domains lists the domains with a complete generation in the bucket, so a
// server can open (and restore) every Spin it holds before the first visit.
func Domains(ctx context.Context, config Config) ([]string, error) {
	config = config.withDefaults()
	client := &S3{Endpoint: config.Endpoint, Bucket: config.Bucket, Region: config.Region, AccessKey: config.AccessKey, SecretKey: config.SecretKey}
	objects, err := client.List(ctx, config.Prefix+"/")
	if err != nil {
		return nil, err
	}
	var domains []string
	for _, object := range objects {
		rest := strings.TrimPrefix(object.Key, config.Prefix+"/")
		domain, file, ok := strings.Cut(rest, "/")
		if ok && file == "current" && domain != "" {
			domains = append(domains, domain)
		}
	}
	return domains, nil
}
