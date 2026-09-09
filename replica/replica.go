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
}

type Replica struct {
	config  Config
	s3      ObjectStore
	domain  string
	path    string
	inner   vfs.VFS
	files   Storage
	tracker *tracker
	vfsName string
	logger  *slog.Logger

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
	stop        chan struct{}
	stopOnce    sync.Once
	lastCompact time.Time
	// now is the clock; tests move it.
	now     func() time.Time
	indexMu sync.Mutex
	index   pageIndex
}

var registered sync.Map

// Options supplies host dependencies. Zero fields select the standard adapters.
type Options struct {
	Objects ObjectStore
	Storage Storage
	Now     func() time.Time
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
		config: config, domain: domain, path: full, inner: inner, files: storageFor(inner), tracker: newTracker(), vfsName: name, logger: logger,
		s3:     &S3{Endpoint: config.Endpoint, Bucket: config.Bucket, Region: config.Region, AccessKey: config.AccessKey, SecretKey: config.SecretKey},
		status: Status{Enabled: true, Bucket: config.Bucket}, stop: make(chan struct{}),
		now: func() time.Time { return time.Now().UTC() },
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
	replica.lifetime, replica.cancel = context.WithCancel(context.Background())
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
			r.logger.Info("replica: no generation in the bucket; the database starts empty", "domain", r.domain)
			return nil
		}
		if err != nil {
			return fmt.Errorf("read current generation: %w", err)
		}
		generation := strings.TrimSpace(string(current))
		restored, err := r.restoreInto(ctx, generation, time.Time{}, r.path)
		if err != nil {
			return fmt.Errorf("restore generation %s: %w", generation, err)
		}
		if err := r.setMarker(restored); err != nil {
			return err
		}
		if err := r.rebuildIndex(restored.PageSize, restored.Seq); err != nil {
			return fmt.Errorf("index the restored database: %w", err)
		}
		r.mu.Lock()
		r.status.Restored, r.status.Generation, r.status.Complete = true, generation, true
		r.mu.Unlock()
		r.logger.Info("replica: database restored from the bucket", "domain", r.domain, "generation", generation, "segments", restored.Seq, "bytes", restored.Size)
		return nil
	}
	stored, err := r.readMarker()
	switch {
	case err != nil:
		r.logger.Warn("replica: no usable marker next to the database; a new generation starts", "domain", r.domain, "error", err)
	case stored.Version != formatVersion:
		r.logger.Warn("replica: legacy local marker; starting a generation with commit manifests", "domain", r.domain)
	case stored.Destination != r.destinationID():
		r.logger.Info("replica: object-store destination changed; starting a fresh generation", "domain", r.domain)
	case !stored.Clean:
		// Writes after the last sync: compare with what the bucket holds
		// and continue with the difference.
		index, err := r.usableIndex(stored)
		if err != nil {
			r.logger.Warn("replica: the database changed after its last sync and the page index is unusable; a new generation starts", "domain", r.domain, "generation", stored.Generation, "error", err)
			return nil
		}
		pages, total, err := r.differingPages(index)
		if err != nil {
			r.logger.Warn("replica: the database changed after its last sync and could not be compared; a new generation starts", "domain", r.domain, "generation", stored.Generation, "error", err)
			return nil
		}
		if err := r.setMarker(stored); err != nil {
			return err
		}
		r.indexMu.Lock()
		r.index = index
		r.indexMu.Unlock()
		r.tracker.markPages(pages)
		r.mu.Lock()
		r.status.Generation, r.status.Complete = stored.Generation, stored.Complete
		r.mu.Unlock()
		r.logger.Info("replica: the database changed after its last sync; the generation continues with the pages that differ", "domain", r.domain, "generation", stored.Generation, "pages", len(pages), "of", total)
	default:
		if err := r.setMarker(stored); err != nil {
			return err
		}
		if index, err := r.usableIndex(stored); err == nil {
			r.indexMu.Lock()
			r.index = index
			r.indexMu.Unlock()
		} else if err := r.rebuildIndex(stored.PageSize, stored.Seq); err != nil {
			r.logger.Warn("replica: page index rebuilt with an error; the next unclean stop costs a full snapshot", "domain", r.domain, "error", err)
		}
		r.mu.Lock()
		r.status.Generation, r.status.Complete = stored.Generation, stored.Complete
		r.mu.Unlock()
	}
	return nil
}

// SnapshotDue reports whether the next sync copies the whole database: a
// Spin runs that copy before it opens, because it holds the database.
func (r *Replica) SnapshotDue() bool {
	current := r.getMarker()
	return current.Generation == "" || !current.Complete || r.compactionDue(current)
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
			case <-r.stop:
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
		close(r.stop)
		r.mu.Unlock()
		r.active.Wait()
		vfs.Unregister(r.vfsName)
		registered.Delete(r.vfsName)
	})
}

func (r *Replica) Status() Status {
	r.mu.Lock()
	defer r.mu.Unlock()
	status := r.status
	status.PendingPages = r.tracker.pendingPages()
	return status
}

// Sync ships what changed: one pass, segments of at most SegmentBytes.
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

func (r *Replica) sync(ctx context.Context, db Database) error {
	current := r.getMarker()
	// An incomplete attempt is never resumed: a fresh snapshot and fresh keys
	// also make a timeout after a successful PUT safe to retry.
	fresh := !current.Complete || r.compactionDue(current)
	if fresh {
		current = marker{Version: formatVersion, Generation: newGenerationID(r.now()), StartedAt: r.now()}
		if err := r.setMarker(current); err != nil {
			return err
		}
	}
	started := r.now()
	if fresh {
		r.logger.Info("replica: snapshot copy starts; the database is usable meanwhile", "domain", r.domain, "generation", current.Generation)
	}
	captured, err := r.capture(ctx, db, fresh)
	if err != nil {
		return err
	}
	defer captured.close(r.files)
	if captured.snapshot {
		r.logger.Info("replica: snapshot copied; the upload starts", "domain", r.domain, "pages", len(captured.pages), "bytes", captured.size, "took", r.now().Sub(started).Round(time.Millisecond))
	}
	if captured.snapshot && !fresh {
		fresh = true
		current = marker{Version: formatVersion, Generation: newGenerationID(r.now()), StartedAt: r.now()}
		if err := r.setMarker(current); err != nil {
			r.tracker.putBack(captured.pages)
			return err
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
		m := manifest{MinSize: captured.size, Version: formatVersion, FirstSeq: current.Seq + 1, Seq: current.Seq + 1, At: at}
		prefix := r.generationPrefix(current.Generation) + "data/" + newGenerationID(r.now()) + "/"
		committed, publishing := false, false
		defer func() {
			if !committed {
				r.tracker.putBack(captured.pages)
				if publishing {
					r.markerMu.Lock()
					r.marker.Complete = false
					r.marker.Clean = false
					_ = r.writeMarker(r.marker)
					r.markerMu.Unlock()
				}
			}
		}()
		for index, part := range captured.parts {
			data, err := r.readSpool(captured.path, part)
			if err != nil {
				return err
			}
			ref := partRef{Key: fmt.Sprintf("%s%06d.seg", prefix, index+1), Size: int64(len(data)), Hash: sha256hex(data)}
			if err := r.s3.Put(ctx, ref.Key, data); err != nil {
				return err
			}
			m.Parts = append(m.Parts, ref)
		}
		key := r.rawKey(current.Generation, m.Seq, m.At)
		if fresh {
			key = r.snapshotKey(current.Generation)
		}
		publishing = true
		if err := r.putManifest(ctx, key, m); err != nil {
			// The server may have accepted the commit despite a lost response. Never
			// reuse its sequence for different data on the next attempt.
			current.Complete = false
			current.Clean = false
			_ = r.setMarker(current)
			return err
		}
		if fresh {
			if err := r.s3.Put(ctx, r.currentKey(), []byte(current.Generation)); err != nil {
				return err
			}
		}
		current.Seq = m.Seq
		current.At = m.At
		current.Size = captured.size
		current.PageSize = captured.pageSize
		current.Complete = true
		current.Clean = false
		for _, part := range m.Parts {
			current.Bytes += part.Size
		}
		if err := r.setMarker(current); err != nil {
			return err
		}
		committed = true
		r.indexMu.Lock()
		r.index.apply(captured.pageSize, captured.size, captured.shipped, captured.hashes)
		r.index.seq = current.Seq
		r.indexMu.Unlock()
		if err := r.writeIndex(); err != nil {
			r.logger.Warn("replica: write page index", "domain", r.domain, "error", err)
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
		if err := r.tracker.settle(captured.revision, func() error { current.Clean = true; return r.setMarker(current) }); err != nil {
			return err
		}
	}
	if len(captured.parts) == 0 && current.Complete {
		if err := r.tracker.settle(captured.revision, func() error { current = r.getMarker(); current.Clean = true; return r.setMarker(current) }); err != nil {
			return err
		}
	}
	if fresh && current.Complete {
		r.pruneGenerations(current.Generation)
	}
	if current.Complete && r.now().Sub(r.lastCompact) >= time.Minute {
		// Persist the frontier before merging; after restart no commit can land in
		// a window which may already have been published.
		frontier := r.now()
		r.markerMu.Lock()
		next := r.marker
		next.SealedAt = frontier
		err := r.writeMarker(next)
		if err == nil {
			r.marker = next
		}
		r.markerMu.Unlock()
		if err != nil {
			return err
		}
		if err := r.compact(ctx, current.Generation); err != nil {
			return fmt.Errorf("compact: %w", err)
		}
		r.lastCompact = frontier
	}
	return nil
}

// compactionDue: the changes outweigh the database, or the generation is a
// configured generation age is reached; a fresh snapshot keeps restores short.
func (r *Replica) compactionDue(current marker) bool {
	if !current.Complete {
		return false
	}
	if current.Bytes > 2*current.Size+64<<20 {
		return true
	}
	return r.now().Sub(current.StartedAt) > r.config.Generation
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
		if first == 0 || first > last {
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
// never completed). The one kept always stays.
func (r *Replica) pruneGenerations(keep string) {
	r.archiveMu.Lock()
	defer r.archiveMu.Unlock()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
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
	for _, object := range objects {
		rest := strings.TrimPrefix(object.Key, r.key("generations")+"/")
		id, _, _ := strings.Cut(rest, "/")
		if id == keep {
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

// A local clean marker only applies to the object-store namespace it synced.
func (r *Replica) destinationID() string {
	return sha256hex([]byte(strings.Join([]string{r.config.Endpoint, r.config.Bucket, r.config.Prefix, r.domain}, "\x00")))
}

func (r *Replica) setMarker(value marker) error {
	value.Destination = r.destinationID()
	r.markerMu.Lock()
	defer r.markerMu.Unlock()
	// A failed clean write must never leave a clean in-memory state.
	if err := r.writeMarker(value); err != nil {
		r.marker.Clean = false
		return err
	}
	r.marker = value
	return nil
}

func (r *Replica) markUnclean() error {
	r.markerMu.Lock()
	defer r.markerMu.Unlock()
	value := r.marker
	value.Clean = false
	if err := r.writeMarker(value); err != nil {
		return err
	}
	r.marker = value
	return nil
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
