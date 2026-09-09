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
	"strings"
	"sync"
	"time"

	"github.com/ncruces/go-sqlite3/vfs"
)

// A Replica keeps one tenant's database on S3, page by page. Pages that
// SQLite writes are tracked in the VFS; every interval they are read under
// a read transaction and shipped as a segment. A generation starts with a
// snapshot (every page) and grows with the changes; when the changes
// outweigh the database, a new generation replaces it. At start, a missing
// database is rebuilt from the current generation.
//
// Layout in the bucket, under the prefix and the domain:
//
//	current                       the id of the complete generation
//	generations/<id>/<seq>.seg    the segments, in order

// Database lets the replica read pages while no write is in flight.
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
	Generation string    `json:"generation"`
	Seq        int64     `json:"seq"`
	Size       int64     `json:"size"`
	Bytes      int64     `json:"bytes"`
	Complete   bool      `json:"complete"`
	Clean      bool      `json:"clean"`
	StartedAt  time.Time `json:"started_at"`
}

type Replica struct {
	config  Config
	s3      *S3
	domain  string
	path    string
	inner   vfs.VFS
	files   storage
	tracker *tracker
	vfsName string
	logger  *slog.Logger

	markerMu sync.Mutex
	marker   marker

	mu          sync.Mutex
	db          Database
	syncing     bool
	status      Status
	stop        chan struct{}
	stopOnce    sync.Once
	lastCompact time.Time
	// now is the clock; tests move it.
	now func() time.Time
}

var registered sync.Map

// New prepares a replica for the database at path on the given storage VFS
// and registers the tracking VFS SQLite must open the database with.
func New(config Config, domain, path string, inner vfs.VFS, logger *slog.Logger) (*Replica, error) {
	config = config.withDefaults()
	if strings.TrimSpace(config.Endpoint) == "" || strings.TrimSpace(config.Bucket) == "" {
		return nil, errors.New("replica needs an S3 endpoint and bucket")
	}
	if inner == nil {
		return nil, errors.New("replica needs a storage VFS")
	}
	full := fullPath(inner, path)
	name := "spin-replica-" + safeName(domain)
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
	if !exists {
		current, err := r.s3.Get(ctx, r.currentKey())
		if errors.Is(err, ErrNotFound) {
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
	case !stored.Clean:
		r.logger.Warn("replica: the database changed after its last sync; a new generation starts", "domain", r.domain, "generation", stored.Generation)
	default:
		r.setMarker(stored)
		r.mu.Lock()
		r.status.Generation, r.status.Complete = stored.Generation, stored.Complete
		r.mu.Unlock()
	}
	return nil
}

// Attach gives the replica the database to read under, and starts the loop.
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
		close(r.stop)
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
	if r.syncing || r.db == nil {
		r.mu.Unlock()
		return nil
	}
	r.syncing = true
	db := r.db
	r.mu.Unlock()
	err := r.sync(ctx, db)
	r.mu.Lock()
	r.syncing = false
	if err != nil {
		r.status.LastError = err.Error()
	} else {
		r.status.LastError = ""
		r.status.LastSyncAt = time.Now().UTC()
	}
	r.mu.Unlock()
	return err
}

func (r *Replica) sync(ctx context.Context, db Database) error {
	pageSize := r.tracker.currentPageSize()
	if pageSize == 0 {
		return nil
	}
	current := r.getMarker()
	if current.Generation == "" || r.compactionDue(current) {
		size, err := r.fileSize()
		if err != nil {
			return err
		}
		generation := newGenerationID(r.now())
		r.tracker.markAll(size)
		current = marker{Generation: generation, StartedAt: r.now()}
		if err := r.setMarker(current); err != nil {
			return err
		}
		r.mu.Lock()
		r.status.Generation, r.status.Complete = generation, false
		r.mu.Unlock()
		r.logger.Info("replica: new generation", "domain", r.domain, "generation", generation, "bytes", size)
	}
	limit := max(1, r.config.SegmentBytes/pageSize)
	for {
		if err := ctx.Err(); err != nil {
			return err
		}
		pages := r.tracker.take(limit)
		if len(pages) == 0 {
			break
		}
		var seg segment
		err := db.WithReadTransaction(ctx, func() error {
			var readErr error
			seg, readErr = r.readPages(pageSize, pages)
			return readErr
		})
		if err != nil {
			r.tracker.putBack(pages)
			return err
		}
		current = r.getMarker()
		if len(seg.Pages) == 0 && seg.DBSize == current.Size {
			continue
		}
		encoded := encodeSegment(seg)
		if err := r.s3.Put(ctx, r.rawKey(current.Generation, current.Seq+1, r.now()), encoded); err != nil {
			r.tracker.putBack(pages)
			return err
		}
		current.Seq++
		current.Size = seg.DBSize
		current.Bytes += int64(len(encoded))
		if err := r.setMarker(current); err != nil {
			return err
		}
		r.mu.Lock()
		r.status.UploadedBytes += int64(len(encoded))
		r.mu.Unlock()
	}
	if r.tracker.settle() {
		r.markerMu.Lock()
		r.marker.Clean = true
		clean := r.marker
		r.markerMu.Unlock()
		if err := r.writeMarker(clean); err != nil {
			return err
		}
	}
	current = r.getMarker()
	if !current.Complete && r.tracker.pendingPages() == 0 && current.Seq > 0 {
		note, _ := json.Marshal(snapshotNote{Seq: current.Seq, At: r.now().Truncate(time.Second)})
		if err := r.s3.Put(ctx, r.snapshotKey(current.Generation), note); err != nil {
			return err
		}
		if err := r.s3.Put(ctx, r.currentKey(), []byte(current.Generation)); err != nil {
			return err
		}
		current.Complete = true
		if err := r.setMarker(current); err != nil {
			return err
		}
		r.mu.Lock()
		r.status.Complete = true
		r.mu.Unlock()
		r.logger.Info("replica: generation complete", "domain", r.domain, "generation", current.Generation, "segments", current.Seq, "bytes", current.Bytes)
		r.pruneGenerations(current.Generation)
	}
	if current.Complete && r.now().Sub(r.lastCompact) >= time.Minute {
		r.lastCompact = r.now()
		if err := r.compact(ctx, current.Generation); err != nil {
			return fmt.Errorf("compact: %w", err)
		}
	}
	return nil
}

// compactionDue: the changes outweigh the database, or the generation is a
// day old; a fresh snapshot keeps restores short.
func (r *Replica) compactionDue(current marker) bool {
	if !current.Complete {
		return false
	}
	if current.Bytes > 2*current.Size+64<<20 {
		return true
	}
	return r.now().Sub(current.StartedAt) > r.config.Generation
}

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
	for _, page := range pages {
		offset := int64(page-1) * int64(pageSize)
		if offset >= size {
			continue
		}
		data := make([]byte, pageSize)
		count, err := file.ReadAt(data, offset)
		if err != nil && !errors.Is(err, io.EOF) {
			return segment{}, err
		}
		if count == 0 {
			continue
		}
		seg.Pages = append(seg.Pages, page)
		seg.Data = append(seg.Data, data)
	}
	return seg, nil
}

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
	removed := 0
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
			r.logger.Warn("replica: delete old segment", "domain", r.domain, "key", object.Key, "error", err)
			return
		}
		removed++
	}
	if removed > 0 {
		r.logger.Info("replica: old generations removed", "domain", r.domain, "files", removed)
	}
}

func newGenerationID(now time.Time) string {
	var random [3]byte
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

func (r *Replica) setMarker(value marker) error {
	r.markerMu.Lock()
	r.marker = value
	r.markerMu.Unlock()
	return r.writeMarker(value)
}

// markUnclean runs on the first write after a sync: the marker on storage
// says so before the write lands.
func (r *Replica) markUnclean() {
	r.markerMu.Lock()
	r.marker.Clean = false
	value := r.marker
	r.markerMu.Unlock()
	if err := r.writeMarker(value); err != nil {
		r.logger.Warn("replica: mark unclean", "domain", r.domain, "error", err)
	}
}

func (r *Replica) writeMarker(value marker) error {
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	file, err := r.files.Open(r.markerPath(), true)
	if err != nil {
		return err
	}
	if _, err := file.WriteAt(data, 0); err != nil {
		_ = file.Close()
		return err
	}
	if err := file.Truncate(int64(len(data))); err != nil {
		_ = file.Close()
		return err
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return err
	}
	return file.Close()
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
