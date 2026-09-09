package replica

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"math/rand/v2"
	"net/http"
	"net/http/httptest"
	"os"
	"sort"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

// TestModel is a randomized, model-based test of the whole layer. A Go map
// mirrors what the test wrote to the database, and every sync that leaves
// a manifest in the bucket records a copy of that map with the replica's
// fake clock. Random steps write, delete and revert values (a few bytes to
// a mebibyte, so writes span many pages and the file grows and shrinks),
// sync, jump the clock so windows merge and expire, restart the process
// cleanly and uncleanly, remove or corrupt the page index, make the bucket
// and the local storage fail, restore into a fresh directory and fetch
// every advertised restore point. The invariants each check protects are
// written next to the check.
//
// The seed is logged: REPLICA_MODEL_SEED=<seed> replays a run and
// REPLICA_MODEL_STEPS=<n> lengthens it.
func TestModel(t *testing.T) {
	seed, steps := modelParameters(t)
	t.Logf("seed %d, %d steps: REPLICA_MODEL_SEED=%d REPLICA_MODEL_STEPS=%d go test ./replica/ -run TestModel", seed, steps, seed, steps)
	started := time.Now()
	h := newModelHarness(t, seed)
	defer h.shutdown()
	for step := 0; step < steps; step++ {
		h.step = step
		h.randomStep()
	}
	// Every run ends with the expensive checks, whatever the dice said.
	h.step = steps
	h.op("final-sync", func() { h.mustSync("final") })
	h.op("final-restore-check", h.checkRestore)
	h.op("final-fetch-every-point", h.checkEveryPoint)
	t.Logf("%d steps in %s; operations %v; %d states committed, %d points fetched, %d generations seen",
		steps, time.Since(started).Round(time.Millisecond), h.stats, len(h.committed), h.pointsFetched, len(h.generations))
}

// modelIndexWriteFaults adds the page index write to the local faults the
// model injects: a failed index write removes the index, and the index
// carries the marker sequence, so a stale one is refused at the next start
// (TestUncleanStopWithStaleIndexShipsAgainWithoutHarm in crash_test.go
// shows the case deterministically).
const modelIndexWriteFaults = true

func modelParameters(t *testing.T) (seed uint64, steps int) {
	seed = uint64(time.Now().UnixNano())
	if value := os.Getenv("REPLICA_MODEL_SEED"); value != "" {
		parsed, err := strconv.ParseUint(value, 10, 64)
		if err != nil {
			t.Fatalf("REPLICA_MODEL_SEED=%q: %v", value, err)
		}
		seed = parsed
	}
	steps = 150
	if value := os.Getenv("REPLICA_MODEL_STEPS"); value != "" {
		parsed, err := strconv.Atoi(value)
		if err != nil || parsed < 0 {
			t.Fatalf("REPLICA_MODEL_STEPS=%q: not a step count", value)
		}
		steps = parsed
	}
	return seed, steps
}

// modelBucket wraps the fake bucket: when armed, the n-th request of one
// kind answers 503. A lost reply stores the PUT first, the way a store
// behaves whose acknowledgement never reached the client.
type modelBucket struct {
	inner     *fakeBucket
	mu        sync.Mutex
	kind      string // PUT, GET, DELETE or LIST; "" when disarmed
	countdown int
	lost      bool
	fired     string // key of the request that failed, "" if none did
}

func modelRequestKind(r *http.Request) string {
	if r.Method == http.MethodGet && r.URL.Query().Get("list-type") == "2" {
		return "LIST"
	}
	return r.Method
}

func (b *modelBucket) arm(kind string, countdown int, lost bool) {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.kind, b.countdown, b.lost, b.fired = kind, countdown, lost, ""
}

// disarm reports which key failed, "" when the fault never fired.
func (b *modelBucket) disarm() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.kind = ""
	return b.fired
}

func (b *modelBucket) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	b.mu.Lock()
	fire := false
	if b.kind != "" && b.kind == modelRequestKind(r) {
		b.countdown--
		if b.countdown <= 0 {
			fire = true
			b.fired = strings.TrimPrefix(r.URL.Path, "/bucket/")
			b.kind = ""
		}
	}
	lost := b.lost
	b.mu.Unlock()
	if !fire {
		b.inner.ServeHTTP(w, r)
		return
	}
	if lost && r.Method == http.MethodPut {
		b.inner.ServeHTTP(httptest.NewRecorder(), r)
	}
	http.Error(w, "injected outage", http.StatusServiceUnavailable)
}

// modelState is the model as it was when a manifest was committed.
type modelState struct {
	at   time.Time
	data map[string][]byte
}

type modelHarness struct {
	t     *testing.T
	ctx   context.Context
	rng   *rand.Rand
	step  int
	op    func(name string, fn func())
	stats map[string]int

	bucket  *fakeBucket
	chaos   *modelBucket
	config  Config
	domain  string
	path    string
	scratch string
	clock   *testClock
	keys    []string

	rep *Replica
	db  *testDatabase
	// closedMarker is the marker as it was when close ran, for the
	// assertions of the reopen that follows.
	closedMarker marker
	closedStatus Status

	// model is what the database holds now. writes counts write
	// transactions since the last commit the replica acknowledged locally.
	model  map[string][]byte
	writes int
	// committed lists the states the bucket holds a manifest for, in
	// ascending clock order; current is the state of the last Sync that
	// returned nil; candidates are states that failed syncs committed
	// nevertheless (a lost reply, or an error after the commit).
	committed  []modelState
	current    map[string][]byte
	candidates []map[string][]byte
	// indexStale is set while an injected fault may have left the page
	// index on disk behind the bucket; pending-page assertions then relax.
	indexStale    bool
	generations   map[string]bool
	pointsFetched int
}

func newModelHarness(t *testing.T, seed uint64) *modelHarness {
	bucket := &fakeBucket{objects: map[string][]byte{}}
	chaos := &modelBucket{inner: bucket}
	server := httptest.NewServer(chaos)
	t.Cleanup(server.Close)
	config := testConfig(server)
	// A short schedule and generation so merging, expiry and generation
	// rollover all happen within a run of a few simulated days.
	config.Schedule = []Level{{Window: 15 * time.Minute, Keep: 2 * time.Hour}, {Window: time.Hour, Keep: 24 * time.Hour}}
	config.Generation = 12 * time.Hour
	config.Retention = 36 * time.Hour
	dir := t.TempDir()
	h := &modelHarness{
		t: t, ctx: context.Background(), rng: rand.New(rand.NewPCG(seed, 0x9e3779b97f4a7c15)), stats: map[string]int{},
		bucket: bucket, chaos: chaos, config: config, domain: "model.example.test", path: dir + "/model.db", scratch: t.TempDir(),
		clock: &testClock{at: time.Date(2026, 9, 9, 8, 0, 30, 0, time.UTC)},
		model: map[string][]byte{}, generations: map[string]bool{},
	}
	h.op = func(name string, fn func()) {
		h.stats[name]++
		fn()
	}
	for index := 0; index < 30; index++ {
		h.keys = append(h.keys, fmt.Sprintf("key-%02d", index))
	}
	h.open()
	// Half of the runs let SQLite give pages back to the file system on
	// delete, so truncation happens all the time, not only on VACUUM.
	if h.rng.IntN(2) == 0 {
		t.Log("auto_vacuum=FULL: the file shrinks on delete")
		h.exec("auto_vacuum", func() error {
			_, err := h.db.db.Exec(`PRAGMA auto_vacuum=FULL; VACUUM`)
			return err
		})
		h.writes++
	}
	return h
}

func (h *modelHarness) shutdown() {
	if h.db != nil {
		h.db.Close()
	}
	if h.rep != nil {
		h.rep.Close()
	}
}

func (h *modelHarness) fatalf(format string, args ...any) {
	h.t.Helper()
	h.t.Fatalf("step %d: "+format, append([]any{h.step}, args...)...)
}

func (h *modelHarness) exec(what string, fn func() error) {
	h.t.Helper()
	if err := fn(); err != nil {
		h.fatalf("%s: %v", what, err)
	}
}

func (h *modelHarness) open() {
	h.rep, h.db = openReplicated(h.t, h.config, h.domain, h.path)
	h.rep.now = h.clock.Now
	if generation := h.rep.Status().Generation; generation != "" {
		h.generations[generation] = true
	}
}

// close stops the database first, then the replica, and remembers the
// marker the next open will read. Without a sync before it this is an
// unclean stop whenever writes happened since the last sync.
func (h *modelHarness) close() {
	h.closedMarker = h.rep.getMarker()
	h.closedStatus = h.rep.Status()
	h.exec("close database", h.db.Close)
	h.rep.Close()
	h.rep, h.db = nil, nil
}

// reopen opens the same path again and checks what Prepare made of the
// marker and the page index it found.
func (h *modelHarness) reopen() {
	h.open()
	before, after := h.closedStatus, h.rep.Status()
	if before.Generation == "" {
		return
	}
	if !h.closedMarker.Complete {
		// An incomplete marker starts a new generation at the next sync;
		// Prepare may report the old one or none.
		if after.Generation != "" && after.Generation != before.Generation {
			h.fatalf("reopen after an incomplete marker reports generation %q, had %q", after.Generation, before.Generation)
		}
		return
	}
	// A failed index write may have left no usable index: then a new
	// generation is the right answer, and nothing is pending yet.
	if h.indexStale && after.Generation == "" && after.PendingPages == 0 {
		h.stats["reopen-without-index"]++
		return
	}
	// A restart continues the generation: a full snapshot after every
	// restart would be the failure this index exists to prevent.
	if after.Generation != before.Generation {
		h.fatalf("reopen did not continue generation %q: %+v (marker %+v)", before.Generation, after, h.closedMarker)
	}
	// Only what changed since the last commit is pending: page 1 carries
	// SQLite's change counter, so any write transaction shows up, and a
	// database that did not change ships nothing.
	if h.writes > 0 && after.PendingPages == 0 && !h.indexStale {
		h.fatalf("%d write transactions since the last commit but nothing pending after reopen: %+v", h.writes, after)
	}
	if h.writes == 0 && after.PendingPages != 0 {
		h.fatalf("nothing changed since the last commit but %d pages pending after reopen (marker %+v)", after.PendingPages, h.closedMarker)
	}
}

func (h *modelHarness) advanceClock(d time.Duration) { h.clock.Add(d) }

// jitter keeps commit times off exact window boundaries, as wall-clock
// nanoseconds do in production.
func (h *modelHarness) jitter() time.Duration { return time.Duration(1 + h.rng.IntN(999_999_999)) }

func (h *modelHarness) copyModel() map[string][]byte {
	out := make(map[string][]byte, len(h.model))
	for key, value := range h.model {
		out[key] = value
	}
	return out
}

func (h *modelHarness) randomStep() {
	roll := h.rng.IntN(100)
	switch {
	case roll < 33:
		h.op("write", h.write)
	case roll < 40:
		h.op("delete", h.delete)
	case roll < 44:
		h.op("revert", h.revert)
	case roll < 46:
		h.op("vacuum", h.vacuum)
	case roll < 54:
		h.op("advance-clock", h.advance)
	case roll < 68:
		h.op("sync", func() { h.mustSync("sync") })
	case roll < 78:
		h.op("sync-bucket-fault", h.syncWithBucketFault)
	case roll < 82:
		h.op("sync-local-fault", h.syncWithLocalFault)
	case roll < 86:
		h.op("unclean-restart", h.uncleanRestart)
	case roll < 88:
		h.op("clean-restart", h.cleanRestart)
	case roll < 92:
		h.op("restore-check", h.checkRestore)
	case roll < 95:
		h.op("fetch-every-point", h.checkEveryPoint)
	case roll < 97:
		h.op("index-removed-restart", func() { h.damagedIndexRestart("remove") })
	default:
		h.op("index-corrupt-restart", func() { h.damagedIndexRestart([]string{"flip", "truncate"}[h.rng.IntN(2)]) })
	}
}

// Values: mostly small, some tens of kilobytes, some hundreds, a few of
// about a mebibyte (256 pages of 4 KiB, several 256 KiB segments).
func (h *modelHarness) randomValue() []byte {
	var size int
	switch roll := h.rng.IntN(100); {
	case roll < 55:
		size = 1 + h.rng.IntN(200)
	case roll < 85:
		size = 1 + h.rng.IntN(48<<10)
	case roll < 97:
		size = 64<<10 + h.rng.IntN(448<<10)
	default:
		size = 1<<20 - h.rng.IntN(64<<10)
	}
	data := make([]byte, size)
	var word [8]byte
	for offset := 0; offset < size; offset += len(word) {
		binary.LittleEndian.PutUint64(word[:], h.rng.Uint64())
		copy(data[offset:], word[:])
	}
	return data
}

func (h *modelHarness) write() {
	key := h.keys[h.rng.IntN(len(h.keys))]
	value := h.randomValue()
	for bytes.Equal(value, h.model[key]) {
		value = h.randomValue()
	}
	h.exec("write "+key, func() error { return h.db.WriteFile(key, value) })
	h.model[key] = value
	h.writes++
}

func (h *modelHarness) delete() {
	keys := h.modelKeys()
	if len(keys) == 0 {
		h.write()
		return
	}
	key := keys[h.rng.IntN(len(keys))]
	h.exec("delete "+key, func() error {
		_, err := h.db.db.Exec(`DELETE FROM spin_kv WHERE key=?`, key)
		return err
	})
	delete(h.model, key)
	h.writes++
}

// revert writes a value a key had at an earlier commit: pages then return
// to bytes the bucket already held once, which is what a page comparison
// after an unclean stop must not be fooled by.
func (h *modelHarness) revert() {
	if len(h.committed) == 0 {
		h.write()
		return
	}
	old := h.committed[h.rng.IntN(len(h.committed))].data
	var choices []string
	for key, value := range old {
		if !bytes.Equal(value, h.model[key]) {
			choices = append(choices, key)
		}
	}
	if len(choices) == 0 {
		h.write()
		return
	}
	sort.Strings(choices)
	key := choices[h.rng.IntN(len(choices))]
	h.exec("revert "+key, func() error { return h.db.WriteFile(key, old[key]) })
	h.model[key] = old[key]
	h.writes++
}

func (h *modelHarness) vacuum() {
	h.exec("vacuum", func() error {
		_, err := h.db.db.Exec(`VACUUM`)
		return err
	})
	h.writes++
}

func (h *modelHarness) modelKeys() []string {
	keys := make([]string, 0, len(h.model))
	for key := range h.model {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}

// advance jumps the clock: quarter hours to hours, now and then a day or
// more, so raw commits merge into windows, windows into hours, and
// generations and old generations expire.
func (h *modelHarness) advance() {
	switch roll := h.rng.IntN(100); {
	case roll < 60:
		h.advanceClock(time.Duration(15+h.rng.IntN(105))*time.Minute + h.jitter())
	case roll < 94:
		h.advanceClock(time.Duration(2+h.rng.IntN(6))*time.Hour + h.jitter())
	default:
		h.advanceClock(time.Duration(12+h.rng.IntN(30))*time.Hour + h.jitter())
	}
}

// manifestKeys lists the commit manifests in the bucket: whatever appears
// during a sync is a state a reader can restore from then on.
func (h *modelHarness) manifestKeys() map[string]bool {
	h.bucket.mu.Lock()
	defer h.bucket.mu.Unlock()
	keys := map[string]bool{}
	for key := range h.bucket.objects {
		if strings.HasSuffix(key, "/snapshot") || (strings.Contains(key, "/L0/") && strings.HasSuffix(key, ".json")) {
			keys[key] = true
		}
	}
	return keys
}

func (h *modelHarness) bucketCurrent() (string, bool) {
	h.bucket.mu.Lock()
	defer h.bucket.mu.Unlock()
	value, ok := h.bucket.objects[h.config.Prefix+"/"+h.domain+"/current"]
	return string(value), ok
}

// sync runs one Sync at a fresh clock time and reconciles the model with
// what the replica and the bucket say afterwards. It reports whether the
// bucket gained a commit manifest.
func (h *modelHarness) sync(label string) (published bool, err error) {
	h.advanceClock(time.Duration(1+h.rng.IntN(15*60))*time.Second + h.jitter())
	now := h.clock.Now()
	before := h.manifestKeys()
	writesBefore := h.writes
	err = h.rep.Sync(h.ctx)
	for key := range h.manifestKeys() {
		if !before[key] {
			published = true
		}
	}
	m := h.rep.getMarker()
	acknowledged := m.Complete && m.At.Equal(now)
	// A manifest's time is the clock at capture: that is what makes the
	// recorded states comparable with the points the bucket advertises.
	if m.Complete && m.At.After(now) {
		h.fatalf("%s: marker time %s is after the clock %s", label, m.At, now)
	}
	// The local marker never claims a commit the bucket does not have.
	if acknowledged && !published {
		h.fatalf("%s: marker acknowledges seq %d at %s but the bucket gained no manifest", label, m.Seq, m.At)
	}
	if published {
		h.committed = append(h.committed, modelState{at: now, data: h.copyModel()})
	}
	if acknowledged {
		h.writes = 0
		h.indexStale = false
	}
	if generation := h.rep.Status().Generation; generation != "" {
		h.generations[generation] = true
	}
	if err != nil {
		if published {
			h.candidates = append(h.candidates, h.copyModel())
		}
		return published, err
	}
	// A sync that returns nil has shipped everything: nothing pending, a
	// complete generation and a clean marker.
	if writesBefore > 0 && !acknowledged {
		h.fatalf("%s: sync returned nil after %d write transactions without committing (marker %+v)", label, writesBefore, m)
	}
	if status := h.rep.Status(); status.PendingPages != 0 || !status.Complete || !m.Clean {
		h.fatalf("%s: after a successful sync status = %+v, marker = %+v", label, status, m)
	}
	// After a successful sync the bucket's current generation is the one
	// the replica reports.
	if current, ok := h.bucketCurrent(); !ok || current != h.rep.Status().Generation {
		h.fatalf("%s: bucket current = %q (%v), replica reports %q", label, current, ok, h.rep.Status().Generation)
	}
	h.current = h.copyModel()
	h.candidates = nil
	return published, nil
}

func (h *modelHarness) mustSync(label string) {
	if _, err := h.sync(label); err != nil {
		h.fatalf("%s: sync without injected faults failed: %v", label, err)
	}
}

// syncWithBucketFault makes one request of one kind fail during the sync.
// The sync must report it (only pruning tolerates failures), a failed
// manifest PUT must leave no manifest, and a lost reply must leave one.
func (h *modelHarness) syncWithBucketFault() {
	kind := "PUT"
	switch roll := h.rng.IntN(100); {
	case roll < 50:
	case roll < 70:
		kind = "GET"
	case roll < 85:
		kind = "DELETE"
	default:
		kind = "LIST"
	}
	countdown := 1 + h.rng.IntN(8)
	lost := kind == "PUT" && h.rng.IntN(3) == 0
	h.chaos.arm(kind, countdown, lost)
	label := fmt.Sprintf("bucket fault %s #%d lost=%v", kind, countdown, lost)
	published, err := h.sync(label)
	fired := h.chaos.disarm()
	if fired == "" {
		if err != nil {
			h.fatalf("%s: the fault never fired but the sync failed: %v", label, err)
		}
		return
	}
	if err == nil && (kind == "PUT" || kind == "GET") {
		h.fatalf("%s: %s failed on %q but the sync returned nil", label, kind, fired)
	}
	isManifest := strings.HasSuffix(fired, "/snapshot") || (strings.Contains(fired, "/L0/") && strings.HasSuffix(fired, ".json"))
	if kind == "PUT" && isManifest && published != lost {
		h.fatalf("%s: manifest PUT %q failed, lost=%v, but the bucket gained a manifest: %v", label, fired, lost, published)
	}
	h.stats["bucket-fault-fired"]++
}

// syncWithLocalFault makes the spool or the marker fail during the sync:
// a spool failure keeps the dirty pages and publishes nothing; a marker
// failure is reported, and the bucket is consistent either way.
func (h *modelHarness) syncWithLocalFault() {
	kinds := []string{"spool-sync", "spool-write", "marker-open"}
	if modelIndexWriteFaults {
		kinds = append(kinds, "index-open")
	}
	kind := kinds[h.rng.IntN(len(kinds))]
	// A sync writes the marker several times and the index once.
	countdown := 1 + h.rng.IntN(4)
	if kind == "index-open" {
		countdown = 1
	}
	fired := false
	fault := faultStorage{Storage: OSStorage()}
	switch kind {
	case "spool-sync", "spool-write":
		fault.wrap = func(path string, file File) File {
			if !strings.HasSuffix(path, ".replica-spool") {
				return file
			}
			if kind == "spool-sync" {
				return faultFile{File: file, sync: func() error { fired = true; return errInjected }}
			}
			return faultFile{File: file, write: func([]byte, int64) (int, error) { fired = true; return 0, errInjected }}
		}
	case "marker-open", "index-open":
		suffix := ".replica"
		if kind == "index-open" {
			suffix = ".replica-index"
		}
		fault.open = func(path string, _ bool) error {
			if !strings.HasSuffix(path, suffix) {
				return nil
			}
			countdown--
			if countdown == 0 {
				fired = true
				return errInjected
			}
			return nil
		}
	}
	h.rep.files = fault
	label := "local fault " + kind
	published, err := h.sync(label)
	h.rep.files = OSStorage()
	if !fired {
		if err != nil {
			h.fatalf("%s: the fault never fired but the sync failed: %v", label, err)
		}
		return
	}
	h.stats["local-fault-fired"]++
	switch kind {
	case "index-open":
		h.indexStale = true
	default:
		if err == nil {
			h.fatalf("%s: the fault fired but the sync returned nil", label)
		}
		if strings.HasPrefix(kind, "spool") && published {
			h.fatalf("%s: the spool failed but the bucket gained a manifest", label)
		}
	}
}

func (h *modelHarness) uncleanRestart() {
	h.close()
	h.reopen()
}

// cleanRestart: a sync, then the stop; the reopen must continue with
// nothing pending.
func (h *modelHarness) cleanRestart() {
	h.mustSync("clean restart")
	h.close()
	h.reopen()
	if status := h.rep.Status(); status.PendingPages != 0 {
		h.fatalf("clean restart left %d pages pending: %+v", status.PendingPages, status)
	}
}

// damagedIndexRestart: writes, an unclean stop, and an index the next
// start cannot use. It must not guess: a new generation holds everything,
// and the bucket's current generation becomes that one.
func (h *modelHarness) damagedIndexRestart(damage string) {
	if h.current == nil {
		return
	}
	h.write()
	h.close()
	indexPath := h.path + ".replica-index"
	previous := h.closedStatus.Generation
	if _, err := os.Stat(indexPath); errors.Is(err, os.ErrNotExist) {
		// A failed index write may already have removed it: that is the
		// "remove" case, and the same must hold.
		damage = "remove"
		h.stats["index-already-missing"]++
	}
	switch damage {
	case "remove":
		if err := os.Remove(indexPath); err != nil && !errors.Is(err, os.ErrNotExist) {
			h.fatalf("remove index: %v", err)
		}
	case "flip":
		data, err := os.ReadFile(indexPath)
		if err != nil {
			h.fatalf("read index: %v", err)
		}
		data[h.rng.IntN(len(data))] ^= 0xff
		h.exec("corrupt index", func() error { return os.WriteFile(indexPath, data, 0o600) })
	case "truncate":
		info, err := os.Stat(indexPath)
		if err != nil {
			h.fatalf("stat index: %v", err)
		}
		h.exec("truncate index", func() error { return os.Truncate(indexPath, h.rng.Int64N(info.Size())) })
	}
	h.open()
	if status := h.rep.Status(); status.Generation != "" || status.PendingPages != 0 {
		h.fatalf("start with a %sd index did not abandon the generation: %+v", damage, status)
	}
	if _, err := h.sync("after " + damage + "d index"); err != nil {
		h.fatalf("sync after a %sd index failed: %v", damage, err)
	}
	if status := h.rep.Status(); status.Generation == previous || !status.Complete {
		h.fatalf("sync after a %sd index did not start a new generation: %+v", damage, status)
	}
	h.stats["index-"+damage]++
}

// checkRestore closes the live replica, restores the domain into a fresh
// directory the way a new server does, and compares. The bucket's current
// generation always restores to a state the model had at a commit: the
// last successful sync, or one a failed sync committed anyway.
func (h *modelHarness) checkRestore() {
	h.close()
	dir, err := os.MkdirTemp(h.scratch, "restore-")
	if err != nil {
		h.fatalf("%v", err)
	}
	rep, db := openReplicated(h.t, h.config, h.domain, dir+"/model.db")
	_, hasCurrent := h.bucketCurrent()
	if rep.Status().Restored != hasCurrent {
		h.fatalf("restore into a fresh directory: restored=%v, bucket has current=%v", rep.Status().Restored, hasCurrent)
	}
	h.checkIntegrity(db, "restored database")
	got := h.rows(db, "restored database")
	h.exec("close restored database", db.Close)
	rep.Close()
	os.RemoveAll(dir)
	expected := h.candidates
	if h.current != nil {
		expected = append(expected, h.current)
	} else {
		expected = append(expected, map[string][]byte{})
	}
	var reasons []string
	for _, want := range expected {
		diff := modelDiff(got, want)
		if diff == "" {
			h.reopen()
			return
		}
		reasons = append(reasons, diff)
	}
	h.fatalf("restored database matches none of the %d committed states: %s", len(expected), strings.Join(reasons, "; "))
}

// checkEveryPoint fetches every advertised restore point. Each must be a
// sound SQLite file holding exactly the model as of the newest commit at
// or before the point's time, however many merges and expiries happened.
func (h *modelHarness) checkEveryPoint() {
	points, err := h.rep.Points(h.ctx)
	if err != nil {
		h.fatalf("points: %v", err)
	}
	for index, point := range points {
		want, ok := h.stateAt(point.At)
		if !ok {
			h.fatalf("point %+v precedes every committed state", point)
		}
		path := fmt.Sprintf("%s/point-%d-%d.db", h.scratch, h.step, index)
		if err := h.rep.Fetch(h.ctx, point.Generation, point.At, path); err != nil {
			hint := ""
			if errors.Is(err, ErrNotFound) && generationTime(point.Generation).Before(h.clock.Now().Add(-h.config.Retention)) {
				hint = " (an expired generation whose pruning was interrupted: see TestInterruptedPruneNeverAdvertisesAnUnfetchablePoint)"
			}
			h.fatalf("fetch point %d of %d %+v: %v%s", index+1, len(points), point, err, hint)
		}
		db, err := openTestDatabase(path, "")
		if err != nil {
			h.fatalf("open fetched point %+v: %v", point, err)
		}
		what := fmt.Sprintf("point %+v (state of %s)", point, want.at)
		h.checkIntegrity(db, what)
		got := h.rows(db, what)
		h.exec("close fetched point", db.Close)
		os.Remove(path)
		if diff := modelDiff(got, want.data); diff != "" {
			h.fatalf("%s: %s", what, diff)
		}
		h.pointsFetched++
	}
}

// stateAt is the model as of the newest commit at or before at.
func (h *modelHarness) stateAt(at time.Time) (modelState, bool) {
	for index := len(h.committed) - 1; index >= 0; index-- {
		if !h.committed[index].at.After(at) {
			return h.committed[index], true
		}
	}
	return modelState{}, false
}

func (h *modelHarness) checkIntegrity(db *testDatabase, what string) {
	var result string
	if err := db.db.QueryRow(`PRAGMA integrity_check`).Scan(&result); err != nil || result != "ok" {
		h.fatalf("%s: integrity_check = %q, %v", what, result, err)
	}
}

func (h *modelHarness) rows(db *testDatabase, what string) map[string][]byte {
	rows, err := db.db.Query(`SELECT key, value FROM spin_kv`)
	if err != nil {
		h.fatalf("%s: %v", what, err)
	}
	defer rows.Close()
	out := map[string][]byte{}
	for rows.Next() {
		var key string
		var value []byte
		if err := rows.Scan(&key, &value); err != nil {
			h.fatalf("%s: %v", what, err)
		}
		out[key] = bytes.Clone(value)
	}
	if err := rows.Err(); err != nil {
		h.fatalf("%s: %v", what, err)
	}
	return out
}

// modelDiff describes how got differs from want, "" when it does not.
func modelDiff(got, want map[string][]byte) string {
	var problems []string
	keys := map[string]bool{}
	for key := range got {
		keys[key] = true
	}
	for key := range want {
		keys[key] = true
	}
	for key := range keys {
		g, inGot := got[key]
		w, inWant := want[key]
		switch {
		case !inGot:
			problems = append(problems, fmt.Sprintf("%s missing (want %d bytes)", key, len(w)))
		case !inWant:
			problems = append(problems, fmt.Sprintf("%s unexpected (%d bytes)", key, len(g)))
		case !bytes.Equal(g, w):
			problems = append(problems, fmt.Sprintf("%s differs (%d bytes, want %d)", key, len(g), len(w)))
		}
	}
	sort.Strings(problems)
	return strings.Join(problems, ", ")
}

// The two tests below fail on purpose: each pins a bug the model found
// while it was being written. Their names keep them out of
// "-run 'TestModel$'", which selects the randomized test alone.

func modelFixture(t *testing.T, domain string) (*fakeBucket, *modelBucket, *Replica, *testDatabase, *testClock, string) {
	t.Helper()
	bucket := &fakeBucket{objects: map[string][]byte{}}
	chaos := &modelBucket{inner: bucket}
	server := httptest.NewServer(chaos)
	t.Cleanup(server.Close)
	config := testConfig(server)
	config.Schedule = []Level{{Window: 15 * time.Minute, Keep: 2 * time.Hour}, {Window: time.Hour, Keep: 24 * time.Hour}}
	config.Generation = 12 * time.Hour
	config.Retention = 36 * time.Hour
	dir := t.TempDir()
	rep, db := openReplicated(t, config, domain, dir+"/"+domain+".db")
	t.Cleanup(func() { db.Close(); rep.Close() })
	clock := &testClock{at: time.Date(2026, 9, 9, 8, 0, 30, 0, time.UTC)}
	rep.now = clock.Now
	return bucket, chaos, rep, db, clock, dir
}

// An interrupted prune of an expired generation removes visibility first
// (the snapshot manifest), so a point is never advertised that cannot be
// fetched; the leftover data goes at the next generation start.
func TestInterruptedPruneNeverAdvertisesAnUnfetchablePoint(t *testing.T) {
	bucket, chaos, rep, db, clock, dir := modelFixture(t, "prune.example.test")
	if err := db.WriteFile("k", bytes.Repeat([]byte("x"), 300<<10)); err != nil {
		t.Fatal(err)
	}
	if err := rep.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	old := rep.Status().Generation
	// Past the retention: the next sync starts a generation and prunes
	// the old one. The second DELETE fails, so the prune is interrupted
	// after the snapshot manifest went.
	clock.Add(40 * time.Hour)
	chaos.arm("DELETE", 2, false)
	err := rep.Sync(context.Background())
	fired := chaos.disarm()
	if err != nil || !strings.Contains(fired, old+"/") {
		t.Fatalf("sync = %v, failed delete = %q", err, fired)
	}
	bucket.mu.Lock()
	_, snapshotLeft := bucket.objects[rep.snapshotKey(old)]
	leftovers := 0
	for key := range bucket.objects {
		if strings.Contains(key, old+"/") {
			leftovers++
		}
	}
	bucket.mu.Unlock()
	if snapshotLeft {
		t.Fatalf("the interrupted prune left the snapshot manifest of %s; visibility must go first", old)
	}
	if leftovers == 0 {
		t.Fatal("the interrupted prune removed everything; the scenario needs a leftover")
	}
	points, err := rep.Points(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	for _, point := range points {
		if point.Generation == old {
			t.Errorf("Points advertises %+v of the pruned generation", point)
		}
		if err := rep.Fetch(context.Background(), point.Generation, point.At, dir+"/point-"+point.Generation[:15]+".db"); err != nil {
			t.Errorf("Points advertises %+v, but Fetch fails: %v", point, err)
		}
	}
	// The next generation start clears the leftovers.
	clock.Add(40 * time.Hour)
	if err := db.WriteFile("k", []byte("again")); err != nil {
		t.Fatal(err)
	}
	if err := rep.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	bucket.mu.Lock()
	for key := range bucket.objects {
		if strings.Contains(key, old+"/") {
			t.Errorf("leftover of the pruned generation still in the bucket: %s", key)
		}
	}
	bucket.mu.Unlock()
}

// A commit whose time is exactly a window boundary belongs to the window
// that ends there, the same rule plan uses for a point at that boundary:
// the point restores the same database before and after compaction.
func TestCommitOnWindowBoundaryRestoresTheSameBeforeAndAfterCompaction(t *testing.T) {
	_, _, rep, db, clock, dir := modelFixture(t, "boundary.example.test")
	commit := func(at time.Time, value string) {
		clock.mu.Lock()
		clock.at = at
		clock.mu.Unlock()
		if err := db.WriteFile("state", []byte(value)); err != nil {
			t.Fatal(err)
		}
		if err := rep.Sync(context.Background()); err != nil {
			t.Fatal(err)
		}
	}
	commit(time.Date(2026, 9, 9, 8, 0, 30, 0, time.UTC), "snapshot")
	commit(time.Date(2026, 9, 9, 8, 5, 0, 0, time.UTC), "A")
	commit(time.Date(2026, 9, 9, 8, 15, 0, 0, time.UTC), "B") // exactly the quarter boundary
	commit(time.Date(2026, 9, 9, 8, 20, 0, 0, time.UTC), "C")
	boundary := time.Date(2026, 9, 9, 8, 15, 0, 0, time.UTC)
	generation := rep.Status().Generation
	stateAtBoundary := func(name string) string {
		path := dir + "/" + name + ".db"
		if err := rep.Fetch(context.Background(), generation, boundary, path); err != nil {
			t.Fatal(err)
		}
		fetched, err := openTestDatabase(path, "")
		if err != nil {
			t.Fatal(err)
		}
		defer fetched.Close()
		value, err := fetched.ReadFile("state")
		if err != nil {
			t.Fatal(err)
		}
		return string(value)
	}
	advertised := func() bool {
		points, err := rep.Points(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		for _, point := range points {
			if point.At.Equal(boundary) {
				return true
			}
		}
		return false
	}
	before := stateAtBoundary("before")
	if !advertised() {
		t.Fatal("the boundary is not an advertised point before compaction")
	}
	clock.mu.Lock()
	clock.at = time.Date(2026, 9, 9, 9, 40, 0, 0, time.UTC)
	clock.mu.Unlock()
	rep.lastCompact = time.Time{}
	if err := rep.Sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	after := stateAtBoundary("after")
	if !advertised() {
		t.Fatal("the boundary is not an advertised point after compaction")
	}
	if before != after {
		t.Errorf("the advertised point %s restored %q before compaction and %q after it", boundary.Format(time.TimeOnly), before, after)
	}
}
