//go:build tamago

// hopsqlbench isoleert Spins chunk-INSERT op een HopOS-slot. Eén reeks bij de
// start, daarna staat het rapport op ER_PORT_HTTP (default 8091):
//
//	mem            16 × 1 MiB INSERT in :memory:              → SQLite-CPU op deze core
//	hop            zelfde in /data/bench.db via spin-hop VFS  → + VFS + ABI, Spins pragma's
//	hop-reuse      nog eens op dezelfde db (freelist-hergebruik, zoals Spin na aborts)
//	hop-nojournal  zelfde met journal_mode=OFF, synchronous=OFF → het aandeel van het journal
//
// De tcp_segs-delta's per fase tellen de system calls: dat is het enige
// TCP-verkeer tijdens de reeks.
package main

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/base64"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"runtime"
	"sort"
	"strings"
	"sync"
	"time"

	"easyacp/internal/persistence"
	spinserver "easyacp/internal/server"
	"easyacp/internal/store"
	"easyacp/internal/worker"

	"github.com/ncruces/go-sqlite3/vfs"
	"github.com/xinix00/HopOS/metal/v2/app/applib"
	"github.com/xinix00/HopOS/metal/v2/app/applib/appnet"
)

const (
	chunk = 1 << 20
	count = 16
)

// insertSQL is het statement van de reeks; diag varieert het.
var insertSQL = `INSERT OR REPLACE INTO spin_object_chunks(object_id, sequence, data) VALUES(?, ?, ?)`

// chunkSchema is de tabel van de reeks: Spins WITHOUT ROWID (blob in de
// indexsleutel) of een rowid-tabel met een aparte sleutelindex.
var chunkSchema = schemaWithoutRowid

const schemaWithoutRowid = `CREATE TABLE IF NOT EXISTS spin_object_chunks (
			object_id INTEGER NOT NULL REFERENCES spin_objects(id) ON DELETE CASCADE,
			sequence INTEGER NOT NULL, data BLOB NOT NULL,
			PRIMARY KEY(object_id, sequence)) WITHOUT ROWID`

const schemaRowid = `CREATE TABLE IF NOT EXISTS spin_object_chunks (
			id INTEGER PRIMARY KEY,
			object_id INTEGER NOT NULL REFERENCES spin_objects(id) ON DELETE CASCADE,
			sequence INTEGER NOT NULL, data BLOB NOT NULL,
			UNIQUE(object_id, sequence))`

func main() {
	app := applib.Init()
	if _, err := appnet.Up(app); err != nil {
		app.Logf("hopsqlbench: net: %v", err)
		app.Exit(1)
	}
	vfsName := persistence.RegisterHopVFS(app)

	var sb strings.Builder
	report := func(format string, a ...any) {
		line := fmt.Sprintf(format, a...)
		sb.WriteString(line + "\n")
		app.Logf("%s", line)
	}

	spin := []string{`PRAGMA journal_mode=DELETE`, `PRAGMA synchronous=FULL`}
	hopDSN := func(name string) string {
		return "file:/data/" + name + "?vfs=" + vfsName + "&nolock=1&_pragma=foreign_keys(1)"
	}
	if clean := app.Env("BENCH_CLEAN"); clean != "" {
		// Opruimen: de genoemde bestanden weg (komma-gescheiden), dan alleen het rapport.
		for _, f := range strings.Split(clean, ",") {
			if err := app.Remove(f); err != nil {
				report("clean: %s: %v", f, err)
			} else {
				report("clean: removed %s", f)
			}
		}
	} else if diagPath := app.Env("BENCH_DIAG"); diagPath != "" {
		// Diagnose van een bestaande db (read-only), kopie naar /data2, de
		// INSERT-reeks op de kopie, dan VACUUM en nog eens.
		diag(app, vfsName, report, diagPath)
	} else {
		variants(report, vfsName, hopDSN, spin)
	}
	report("done")

	port := app.Env("ER_PORT_HTTP")
	if port == "" {
		port = "8091"
	}
	// Spins request-pad nagebootst: PUT /put leest de body volledig en doet
	// dezelfde INSERT in een open hop-db; PUT /sink leest alleen. Beide
	// antwoorden met hun eigen tijden, zodat de client ze naast elkaar zet.
	live, err := openLive(hopDSN("bench3.db"), spin)
	if err != nil {
		report("live: %v", err)
	}
	var seq int64
	var mu sync.Mutex
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, sb.String())
	})
	http.HandleFunc("PUT /sink", func(w http.ResponseWriter, r *http.Request) {
		t0 := time.Now()
		data := make([]byte, r.ContentLength)
		if _, err := io.ReadFull(r.Body, data); err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		fmt.Fprintf(w, "sink: read %d bytes in %.1f ms\n", len(data), time.Since(t0).Seconds()*1e3)
	})
	http.HandleFunc("PUT /put", func(w http.ResponseWriter, r *http.Request) {
		if live == nil {
			http.Error(w, "no live db", 500)
			return
		}
		t0 := time.Now()
		data := make([]byte, r.ContentLength)
		if _, err := io.ReadFull(r.Body, data); err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		t1 := time.Now()
		mu.Lock()
		seq++
		n := seq
		mu.Unlock()
		if _, err := live.db.ExecContext(r.Context(), `INSERT OR REPLACE INTO spin_object_chunks(object_id, sequence, data) VALUES(?, ?, ?)`, live.objectID, n%64, data); err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
		t2 := time.Now()
		fmt.Fprintf(w, "put: read %d bytes in %.1f ms, insert %.1f ms\n", len(data), t1.Sub(t0).Seconds()*1e3, t2.Sub(t1).Seconds()*1e3)
	})
	// Een échte Spin-server (zelfde pakket als spin-server) op een eigen db,
	// achter een meet-wrapper: per PUT de body-leestijd tegenover de totale
	// handlertijd, plus GC-cijfers. Worker-token "bench". Rapport op /bench-report.
	spinHandler, err := openSpin(app, vfsName)
	if err != nil {
		report("spin: %v", err)
		spinHandler = http.NotFoundHandler()
	}
	tm := &timings{}
	root := http.NewServeMux()
	root.Handle("/", http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/bench-report":
			fmt.Fprint(w, sb.String())
			fmt.Fprint(w, tm.String())
		case r.URL.Path == "/sink" || r.URL.Path == "/put":
			http.DefaultServeMux.ServeHTTP(w, r)
		case r.Method == "PUT":
			tb := &timedBody{ReadCloser: r.Body}
			r.Body = tb
			var ms0 runtime.MemStats
			runtime.ReadMemStats(&ms0)
			t0 := time.Now()
			spinHandler.ServeHTTP(w, r)
			total := time.Since(t0)
			var ms1 runtime.MemStats
			runtime.ReadMemStats(&ms1)
			tm.add(total, tb.read, tb.n, ms1.NumGC-ms0.NumGC, time.Duration(ms1.PauseTotalNs-ms0.PauseTotalNs))
		default:
			spinHandler.ServeHTTP(w, r)
		}
	}))
	app.Logf("hopsqlbench: http: %v", http.ListenAndServe(":"+port, root))
	app.Exit(1)
}

type timedBody struct {
	io.ReadCloser
	read time.Duration
	n    int64
}

func (b *timedBody) Read(p []byte) (int, error) {
	t := time.Now()
	n, err := b.ReadCloser.Read(p)
	b.read += time.Since(t)
	b.n += int64(n)
	return n, err
}

type timings struct {
	mu    sync.Mutex
	lines []string
	total []float64
	read  []float64
	gcs   uint32
	pause time.Duration
}

func (t *timings) add(total, read time.Duration, n int64, gcs uint32, pause time.Duration) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.total = append(t.total, total.Seconds()*1e3)
	t.read = append(t.read, read.Seconds()*1e3)
	t.gcs += gcs
	t.pause += pause
	if len(t.lines) < 12 {
		t.lines = append(t.lines, fmt.Sprintf("PUT %d bytes: total %.1f ms, body read %.1f ms, rest %.1f ms, gc %d (pause %.1f ms)",
			n, total.Seconds()*1e3, read.Seconds()*1e3, (total-read).Seconds()*1e3, gcs, pause.Seconds()*1e3))
	}
}

func (t *timings) String() string {
	t.mu.Lock()
	defer t.mu.Unlock()
	var sb strings.Builder
	for _, l := range t.lines {
		sb.WriteString(l + "\n")
	}
	if len(t.total) > 0 {
		fmt.Fprintf(&sb, "spin PUTs: %d; total p50 %.1f ms p90 %.1f ms; body read p50 %.1f ms p90 %.1f ms; gc cycles %d, pause total %.1f ms\n",
			len(t.total), pct(t.total, 50), pct(t.total, 90), pct(t.read, 50), pct(t.read, 90), t.gcs, t.pause.Seconds()*1e3)
	}
	return sb.String()
}

func openSpin(app *applib.App, vfsName string) (http.Handler, error) {
	logger := slog.New(slog.NewTextHandler(io.Discard, nil))
	database, err := persistence.Open("/data/spin-bench.db", persistence.OpenOptions{VFS: vfsName})
	if err != nil {
		return nil, fmt.Errorf("open database: %w", err)
	}
	key := make([]byte, 32)
	if _, err := rand.Read(key); err != nil {
		return nil, err
	}
	st, err := store.OpenWithBackend("state", store.OpenOptions{MasterKey: base64.RawStdEncoding.EncodeToString(key)}, database)
	if err != nil {
		return nil, fmt.Errorf("open state: %w", err)
	}
	broker := worker.NewBroker(st, logger)
	engine := worker.NewRemoteEngine(broker, database)
	options := spinserver.ServerOptionsFromEnvironment()
	options.WorkerToken = "bench"
	options.RunnerBroker = broker
	options.AttachmentStorage = database.Files("attachment:", "job-attachment", 15<<20)
	options.SnapshotArchive = database
	options.Database = database
	return spinserver.NewWithOptions(st, logger, engine, options).Handler(), nil
}

func run(report func(string, ...any), name, dsn string, pragmas []string) {
	db, err := sql.Open("sqlite3", dsn)
	if err != nil {
		report("%s: open: %v", name, err)
		return
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)
	ctx := context.Background()
	stmts := append(append([]string{}, pragmas...),
		`CREATE TABLE IF NOT EXISTS spin_objects (
			id INTEGER PRIMARY KEY, digest TEXT, kind TEXT NOT NULL,
			size INTEGER NOT NULL DEFAULT 0, complete INTEGER NOT NULL DEFAULT 0)`,
		chunkSchema,
	)
	for _, s := range stmts {
		if _, err := db.ExecContext(ctx, s); err != nil {
			report("%s: %.24s: %v", name, s, err)
			return
		}
	}
	res, err := db.ExecContext(ctx, `INSERT INTO spin_objects(kind) VALUES('bench')`)
	if err != nil {
		report("%s: object: %v", name, err)
		return
	}
	objectID, _ := res.LastInsertId()
	data := make([]byte, chunk)
	for i := range data {
		data[i] = byte(i * 7)
	}

	c0 := appnet.Counters()
	vc.reset(true)
	t0 := time.Now()
	lat := make([]float64, 0, count)
	for seq := 0; seq < count; seq++ {
		t := time.Now()
		if _, err := db.ExecContext(ctx, insertSQL, objectID, seq, data); err != nil {
			report("%s: insert %d: %v", name, seq, err)
			return
		}
		lat = append(lat, time.Since(t).Seconds()*1e3)
		vc.stopTrace()
	}
	el := time.Since(t0).Seconds()
	c1 := appnet.Counters()
	report("%s: %d × 1 MiB INSERT in %.2fs = %.1f MB/s; per insert p50 %.1f ms, max %.1f ms; syscall segs out %d in %d",
		name, count, el, float64(count*chunk)/el/1e6, pct(lat, 50), pct(lat, 100),
		c1["tcp_segs_out"]-c0["tcp_segs_out"], c1["tcp_segs_in"]-c0["tcp_segs_in"])
	if sum := vc.String(name + " inserts"); sum != "" {
		report("%s", sum)
	}
	vc.reset(false)

	t1 := time.Now()
	var got []byte
	if err := db.QueryRowContext(ctx, `SELECT data FROM spin_object_chunks WHERE object_id = ? AND sequence = ?`, objectID, count/2).Scan(&got); err != nil {
		report("%s: select: %v", name, err)
		return
	}
	c2 := appnet.Counters()
	report("%s: SELECT one chunk: %d bytes in %.1f ms; syscall segs out %d in %d", name, len(got), time.Since(t1).Seconds()*1e3,
		c2["tcp_segs_out"]-c1["tcp_segs_out"], c2["tcp_segs_in"]-c1["tcp_segs_in"])

	t2 := time.Now()
	if _, err := db.ExecContext(ctx, `DELETE FROM spin_objects WHERE id = ?`, objectID); err != nil {
		report("%s: delete: %v", name, err)
		return
	}
	c3 := appnet.Counters()
	report("%s: DELETE object (%d MiB cascade) in %.2fs; syscall segs out %d in %d", name, count, time.Since(t2).Seconds(),
		c3["tcp_segs_out"]-c2["tcp_segs_out"], c3["tcp_segs_in"]-c2["tcp_segs_in"])
	if sum := vc.String(name + " delete"); sum != "" {
		report("%s", sum)
	}
}

// countVFS telt de VFS-calls die SQLite doet en traceert op verzoek de eerste
// transactie: welke op, welk bestand, welke offset/lengte.
type countVFS struct{ inner vfs.VFS }

type vfsCounts struct {
	mu      sync.Mutex
	n       map[string]int
	bytes   map[string]int64
	trace   []traceEntry
	tracing bool
}

type traceEntry struct {
	op, file string
	n, off   int64
}

var vc = &vfsCounts{n: map[string]int{}, bytes: map[string]int64{}}

func (c *vfsCounts) reset(trace bool) {
	c.mu.Lock()
	c.n, c.bytes, c.trace, c.tracing = map[string]int{}, map[string]int64{}, nil, trace
	c.mu.Unlock()
}

func (c *vfsCounts) stopTrace() { c.mu.Lock(); c.tracing = false; c.mu.Unlock() }

func (c *vfsCounts) hit(op, file string, n int64, off int64) {
	c.mu.Lock()
	c.n[op]++
	c.bytes[op] += n
	if c.tracing && len(c.trace) < 4000 {
		c.trace = append(c.trace, traceEntry{op, file, n, off})
	}
	c.mu.Unlock()
}

func (c *vfsCounts) String(label string) string {
	c.mu.Lock()
	defer c.mu.Unlock()
	if len(c.n) == 0 {
		return ""
	}
	ops := make([]string, 0, len(c.n))
	for k := range c.n {
		ops = append(ops, k)
	}
	sort.Strings(ops)
	var sb strings.Builder
	fmt.Fprintf(&sb, "vfs %s:", label)
	total := 0
	for _, k := range ops {
		total += c.n[k]
		if c.bytes[k] > 0 {
			fmt.Fprintf(&sb, " %s %d (%d KiB)", k, c.n[k], c.bytes[k]>>10)
		} else {
			fmt.Fprintf(&sb, " %s %d", k, c.n[k])
		}
	}
	fmt.Fprintf(&sb, " — total %d calls", total)
	if len(c.trace) > 0 {
		fmt.Fprintf(&sb, "\nvfs trace first transaction (%d ops, page = offset/4096):", len(c.trace))
		type seg struct {
			op, file string
			first    int64
			last     int64
			n        int
			bytes    int64
		}
		var segs []seg
		for _, e := range c.trace {
			pg := e.off / 4096
			if k := len(segs) - 1; k >= 0 && segs[k].op == e.op && segs[k].file == e.file && (pg == segs[k].last+1 || pg == segs[k].last) {
				segs[k].last, segs[k].n, segs[k].bytes = pg, segs[k].n+1, segs[k].bytes+e.n
				continue
			}
			segs = append(segs, seg{e.op, e.file, pg, pg, 1, e.n})
		}
		for i, g := range segs {
			if i >= 80 {
				fmt.Fprintf(&sb, "\n  … %d more segments", len(segs)-i)
				break
			}
			switch {
			case g.n == 1:
				fmt.Fprintf(&sb, "\n  %s %s p%d (%d B)", g.op, g.file, g.first, g.bytes)
			case g.first == g.last:
				fmt.Fprintf(&sb, "\n  %s %s p%d ×%d", g.op, g.file, g.first, g.n)
			default:
				fmt.Fprintf(&sb, "\n  %s %s p%d-p%d (%d ops, %d KiB)", g.op, g.file, g.first, g.last, g.n, g.bytes>>10)
			}
		}
	}
	return sb.String()
}

func short(name string) string {
	if i := strings.LastIndex(name, "/"); i >= 0 {
		return name[i+1:]
	}
	return name
}

func (v *countVFS) Open(name string, flags vfs.OpenFlag) (vfs.File, vfs.OpenFlag, error) {
	vc.hit("open", short(name), 0, 0)
	f, fl, err := v.inner.Open(name, flags)
	if err != nil {
		return nil, fl, err
	}
	return &countFile{File: f, name: short(name)}, fl, nil
}

func (v *countVFS) Delete(name string, syncDir bool) error {
	vc.hit("delete", short(name), 0, 0)
	return v.inner.Delete(name, syncDir)
}

func (v *countVFS) Access(name string, flags vfs.AccessFlag) (bool, error) {
	vc.hit("access", short(name), 0, 0)
	return v.inner.Access(name, flags)
}

func (v *countVFS) FullPathname(name string) (string, error) { return v.inner.FullPathname(name) }

type countFile struct {
	vfs.File
	name string
}

func (f *countFile) ReadAt(p []byte, off int64) (int, error) {
	vc.hit("read", f.name, int64(len(p)), off)
	return f.File.ReadAt(p, off)
}

func (f *countFile) WriteAt(p []byte, off int64) (int, error) {
	vc.hit("write", f.name, int64(len(p)), off)
	return f.File.WriteAt(p, off)
}

func (f *countFile) Truncate(size int64) error {
	vc.hit("truncate", f.name, 0, size)
	return f.File.Truncate(size)
}

func (f *countFile) Sync(flags vfs.SyncFlag) error {
	vc.hit("sync", f.name, 0, 0)
	return f.File.Sync(flags)
}

func (f *countFile) Size() (int64, error) {
	vc.hit("size", f.name, 0, 0)
	return f.File.Size()
}

func (f *countFile) Lock(l vfs.LockLevel) error {
	vc.hit("lock", f.name, 0, int64(l))
	return f.File.Lock(l)
}

func (f *countFile) Unlock(l vfs.LockLevel) error {
	vc.hit("unlock", f.name, 0, int64(l))
	return f.File.Unlock(l)
}

func (f *countFile) Close() error {
	vc.hit("close", f.name, 0, 0)
	return f.File.Close()
}

func pct(v []float64, p int) float64 {
	if len(v) == 0 {
		return 0
	}
	s := append([]float64{}, v...)
	sort.Float64s(s)
	i := len(s) * p / 100
	if i >= len(s) {
		i = len(s) - 1
	}
	return s[i]
}

type liveDB struct {
	db       *sql.DB
	objectID int64
}

func openLive(dsn string, pragmas []string) (*liveDB, error) {
	db, err := sql.Open("sqlite3", dsn)
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)
	ctx := context.Background()
	stmts := append(append([]string{}, pragmas...),
		`CREATE TABLE IF NOT EXISTS spin_objects (
			id INTEGER PRIMARY KEY, digest TEXT, kind TEXT NOT NULL,
			size INTEGER NOT NULL DEFAULT 0, complete INTEGER NOT NULL DEFAULT 0)`,
		`CREATE TABLE IF NOT EXISTS spin_object_chunks (
			object_id INTEGER NOT NULL REFERENCES spin_objects(id) ON DELETE CASCADE,
			sequence INTEGER NOT NULL, data BLOB NOT NULL,
			PRIMARY KEY(object_id, sequence)) WITHOUT ROWID`,
	)
	for _, s := range stmts {
		if _, err := db.ExecContext(ctx, s); err != nil {
			return nil, fmt.Errorf("%.24s: %w", s, err)
		}
	}
	res, err := db.ExecContext(ctx, `INSERT INTO spin_objects(kind) VALUES('live')`)
	if err != nil {
		return nil, err
	}
	id, _ := res.LastInsertId()
	return &liveDB{db: db, objectID: id}, nil
}

func diag(app *applib.App, vfsName string, report func(string, ...any), path string) {
	ro, err := sql.Open("sqlite3", "file:"+path+"?vfs="+vfsName+"&nolock=1&mode=ro")
	if err != nil {
		report("diag: open %s: %v", path, err)
		return
	}
	ro.SetMaxOpenConns(1)
	ctx := context.Background()
	for _, pr := range []string{"page_size", "page_count", "freelist_count", "auto_vacuum", "journal_mode", "cache_size", "secure_delete", "user_version"} {
		var v string
		if err := ro.QueryRowContext(ctx, "PRAGMA "+pr).Scan(&v); err != nil {
			report("diag: pragma %s: %v", pr, err)
			continue
		}
		report("diag: %s = %s", pr, v)
	}
	for _, q := range []string{
		`SELECT count(*) FROM spin_objects`,
		`SELECT count(*) FROM spin_objects WHERE complete = 0`,
		`SELECT count(*), coalesce(sum(length(data)),0)/1048576 FROM spin_object_chunks`,
		`SELECT count(*) FROM spin_object_refs`,
		`SELECT count(*), coalesce(sum(length(value)),0) FROM spin_kv`,
		`SELECT name, tbl_name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL`,
	} {
		rows, err := ro.QueryContext(ctx, q)
		if err != nil {
			report("diag: %s: %v", q, err)
			continue
		}
		cols, _ := rows.Columns()
		for rows.Next() {
			vals := make([]any, len(cols))
			ptrs := make([]any, len(cols))
			for i := range vals {
				ptrs[i] = &vals[i]
			}
			_ = rows.Scan(ptrs...)
			report("diag: %s → %v", q, vals)
		}
		rows.Close()
	}
	ro.Close()

	const copyPath = "/data/copy.db"
	if _, err := app.Stat(copyPath); err != nil {
		size, err := app.Stat(path)
		if err != nil {
			report("diag: stat: %v", err)
			return
		}
		buf := make([]byte, chunk)
		t0 := time.Now()
		for off := uint64(0); off < size; off += chunk {
			n, err := app.ReadInto(path, off, buf)
			if err != nil {
				report("diag: copy read at %d: %v", off, err)
				return
			}
			if _, err := app.WriteAt(copyPath, off, buf[:n]); err != nil {
				report("diag: copy write at %d: %v", off, err)
				return
			}
			if n < chunk {
				break
			}
		}
		report("diag: copied %d MiB to %s in %.1fs", size>>20, copyPath, time.Since(t0).Seconds())
	} else {
		report("diag: reusing %s", copyPath)
	}

	spin := []string{`PRAGMA journal_mode=DELETE`, `PRAGMA synchronous=FULL`}
	vfs.Register("count-hop", &countVFS{inner: vfs.Find(vfsName)})
	counted := func(file, extra string) string {
		return "file:" + file + "?vfs=count-hop&nolock=1" + extra
	}
	orReplace := insertSQL
	plain := `INSERT INTO spin_object_chunks(object_id, sequence, data) VALUES(?, ?, ?)`

	insertSQL = orReplace
	run(report, "copy", counted(copyPath, "&_pragma=foreign_keys(1)"), spin)
	insertSQL = plain
	run(report, "copy-plain-nofk", counted(copyPath, ""), spin)
	insertSQL = orReplace
	run(report, "copy-bigcache", counted(copyPath, "&_pragma=foreign_keys(1)"), append([]string{`PRAGMA cache_size=-65536`}, spin...))
	run(report, "fresh", counted("/data/bench-fresh.db", "&_pragma=foreign_keys(1)"), spin)
}

// variants zet de kandidaat-oplossingen naast elkaar op verse db's, elk met
// warm-up-rijen zodat de b-tree diepte krijgt zoals bij Spin.
func variants(report func(string, ...any), vfsName string, hopDSN func(string) string, spin []string) {
	vfs.Register("count-hop", &countVFS{inner: vfs.Find(vfsName)})
	vfs.Register("count-ra-hop", &countVFS{inner: &raVFS{inner: vfs.Find(vfsName), size: 1 << 20}})
	dsn := func(file, v string) string {
		return "file:/data/" + file + "?vfs=" + v + "&nolock=1&_pragma=foreign_keys(1)"
	}
	cache := []string{`PRAGMA cache_size=-65536`}
	page64 := []string{`PRAGMA page_size=65536`}
	type variant struct {
		name, file, vfs string
		schema          string
		pragmas         []string
	}
	all := []variant{
		{"worowid-4k", "v1.db", "count-hop", schemaWithoutRowid, spin},
		{"rowid-4k", "v2.db", "count-hop", schemaRowid, spin},
		{"rowid-4k-cache64m", "v3.db", "count-hop", schemaRowid, append(append([]string{}, cache...), spin...)},
		{"rowid-64k-cache64m", "v4.db", "count-hop", schemaRowid, append(append(append([]string{}, page64...), cache...), spin...)},
		{"rowid-4k-cache64m-ra1m", "v5.db", "count-ra-hop", schemaRowid, append(append([]string{}, cache...), spin...)},
		{"worowid-4k-cache64m-ra1m", "v6.db", "count-ra-hop", schemaWithoutRowid, append(append([]string{}, cache...), spin...)},
	}
	for _, v := range all {
		chunkSchema = v.schema
		run(report, v.name, dsn(v.file, v.vfs), v.pragmas)
	}
}

// raVFS leest vooruit: een leesvraag buiten het venster haalt max(size, vraag)
// bytes op en bedient de volgende pagina's uit dat venster. Elke schrijf- of
// truncate-actie maakt het venster ongeldig; één verbinding, dus dat volstaat.
type raVFS struct {
	inner vfs.VFS
	size  int
}

func (v *raVFS) Open(name string, flags vfs.OpenFlag) (vfs.File, vfs.OpenFlag, error) {
	f, fl, err := v.inner.Open(name, flags)
	if err != nil {
		return nil, fl, err
	}
	return &raFile{File: f, buf: make([]byte, 0, v.size)}, fl, nil
}

func (v *raVFS) Delete(name string, syncDir bool) error             { return v.inner.Delete(name, syncDir) }
func (v *raVFS) Access(name string, f vfs.AccessFlag) (bool, error) { return v.inner.Access(name, f) }
func (v *raVFS) FullPathname(name string) (string, error)           { return v.inner.FullPathname(name) }

type raFile struct {
	vfs.File
	buf []byte // geldig venster
	off int64  // bestandsoffset van buf[0]
}

func (f *raFile) ReadAt(p []byte, off int64) (int, error) {
	end := off + int64(len(p))
	if len(f.buf) > 0 && off >= f.off && end <= f.off+int64(len(f.buf)) {
		vc.hit("read-hit", "", int64(len(p)), off)
		return copy(p, f.buf[off-f.off:end-f.off]), nil
	}
	f.buf = f.buf[:0]
	want := cap(f.buf)
	if len(p) > want {
		return f.File.ReadAt(p, off)
	}
	n, err := f.File.ReadAt(f.buf[:want], off)
	if n < len(p) {
		f.buf = f.buf[:0]
		if err == nil {
			err = io.EOF
		}
		copy(p, f.buf[:n])
		return n, err
	}
	f.buf, f.off = f.buf[:n], off
	return copy(p, f.buf[:len(p)]), nil
}

func (f *raFile) WriteAt(p []byte, off int64) (int, error) {
	f.buf = f.buf[:0]
	return f.File.WriteAt(p, off)
}

func (f *raFile) Truncate(size int64) error {
	f.buf = f.buf[:0]
	return f.File.Truncate(size)
}
