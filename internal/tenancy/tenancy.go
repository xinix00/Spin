// Package tenancy runs one Spin per domain. Every domain a request arrives
// for has its own database named after it, its own store, runners, worker
// token and replica; the process holds them side by side and routes on the
// Host header.
package tenancy

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"strings"
	"sync"

	"easyacp/internal/buildinfo"
	"easyacp/internal/capsule"
	"easyacp/internal/persistence"
	spinserver "easyacp/internal/server"
	"easyacp/internal/store"
	"easyacp/internal/worker"
	"easyacp/replica"

	"github.com/ncruces/go-sqlite3/vfs"
)

type Config struct {
	// DataDir holds <domain>.db per tenant. SingleDatabase instead serves
	// every host from that one file, as a tenant named spin.
	DataDir        string
	SingleDatabase string
	// StorageVFS is the registered VFS of the storage (HopOS); empty means
	// ordinary files.
	StorageVFS string
	// MasterKey (or MasterKeyFile) encrypts the secrets of every tenant.
	MasterKey     string
	MasterKeyFile string
	// Domains optionally limits the hosts; empty admits any host, each with
	// its own database (whatever routes in front decides what arrives). An
	// IP address as host only ever answers the liveness check.
	Domains []string
	// WorkerTokenSeed becomes the worker token of a tenant that has none
	// yet: the token a deployment passed through the environment.
	WorkerTokenSeed string
	Replication     *replica.Config
	Logger          *slog.Logger
	// Options builds the server options of a tenant; the tenancy fills in
	// what it owns (token, runners, storage, replica).
	Options func(domain string) spinserver.ServerOptions
	// Engine builds the capsule engine of a tenant; nil means runners.
	Engine func(st *store.Store, database *persistence.SQLite, logger *slog.Logger) (capsule.Engine, *worker.Broker, error)
	// BeforeStore and AfterStore run once per tenant open, around the store:
	// the entrypoints import legacy files there.
	BeforeStore func(domain string, database *persistence.SQLite) error
	AfterStore  func(domain string, st *store.Store, attachments *persistence.FileStore) error
}

type Tenant struct {
	Domain   string
	Path     string
	Database *persistence.SQLite
	Store    *store.Store
	Broker   *worker.Broker
	Server   *spinserver.Server
	Handler  http.Handler
	Replica  *replica.Replica
}

type Tenants struct {
	config  Config
	logger  *slog.Logger
	mu      sync.Mutex
	tenants map[string]*Tenant
	opening map[string]chan struct{}
	ctx     context.Context
	cancel  context.CancelFunc
}

func New(config Config) *Tenants {
	if config.Logger == nil {
		config.Logger = slog.New(slog.NewTextHandler(io.Discard, nil))
	}
	ctx, cancel := context.WithCancel(context.Background())
	return &Tenants{config: config, logger: config.Logger, tenants: map[string]*Tenant{}, opening: map[string]chan struct{}{}, ctx: ctx, cancel: cancel}
}

// NormalizeHost turns a Host header into a tenant name: lower case, no
// port, no trailing dot, only letters, digits, dots and dashes.
func NormalizeHost(host string) (string, bool) {
	host = strings.ToLower(strings.TrimSpace(host))
	if strings.HasPrefix(host, "[") {
		return "", false
	}
	if index := strings.LastIndex(host, ":"); index >= 0 && strings.Count(host, ":") == 1 {
		host = host[:index]
	}
	host = strings.TrimSuffix(host, ".")
	if host == "" || len(host) > 253 || strings.HasPrefix(host, ".") || strings.HasPrefix(host, "-") || strings.Contains(host, "..") {
		return "", false
	}
	for _, char := range host {
		if (char < 'a' || char > 'z') && (char < '0' || char > '9') && char != '.' && char != '-' {
			return "", false
		}
	}
	return host, true
}

func (t *Tenants) allowed(host string) bool {
	if len(t.config.Domains) == 0 {
		return true
	}
	for _, domain := range t.config.Domains {
		if strings.EqualFold(strings.TrimSpace(domain), host) {
			return true
		}
	}
	return false
}

func (t *Tenants) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	domain := "spin"
	if t.config.SingleDatabase == "" {
		host, ok := NormalizeHost(r.Host)
		if !ok {
			http.Error(w, "invalid host", http.StatusBadRequest)
			return
		}
		// An address is never a Spin: whatever routes in front sends the
		// domain along, and probes on the address get the liveness check.
		if net.ParseIP(host) != nil {
			if r.URL.Path == "/healthz" {
				w.Header().Set("Content-Type", "application/json")
				_, _ = fmt.Fprintf(w, `{"status":"ok","version":%q,"tenants":%d}`, buildinfo.Version, t.count())
				return
			}
			http.Error(w, "Spin answers on its domains, not on an address", http.StatusNotFound)
			return
		}
		if !t.allowed(host) {
			http.Error(w, "unknown domain "+host, http.StatusNotFound)
			return
		}
		domain = host
	}
	tenant, err := t.Open(r.Context(), domain)
	if err != nil {
		t.logger.Error("open tenant", "domain", domain, "error", err)
		http.Error(w, "this Spin cannot open its database: "+err.Error(), http.StatusServiceUnavailable)
		return
	}
	tenant.Handler.ServeHTTP(w, r)
}

func (t *Tenants) count() int {
	t.mu.Lock()
	defer t.mu.Unlock()
	return len(t.tenants)
}

// Discover opens every Spin this server already holds: the databases in
// the data directory, the domains with a replica in the bucket (restored
// when missing here), and the configured domains. Their replicas run from
// the start, not from the first visit.
func (t *Tenants) Discover(ctx context.Context) ([]string, error) {
	if t.config.SingleDatabase != "" {
		_, err := t.Open(ctx, "spin")
		return []string{"spin"}, err
	}
	seen := map[string]bool{}
	var domains []string
	add := func(candidates []string) {
		for _, candidate := range candidates {
			domain, ok := NormalizeHost(candidate)
			if ok && !seen[domain] && net.ParseIP(domain) == nil {
				seen[domain] = true
				domains = append(domains, domain)
			}
		}
	}
	add(t.config.Domains)
	if local, err := persistence.ListDatabases(t.config.DataDir); err == nil {
		add(local)
	}
	if t.config.Replication != nil {
		remote, err := replica.Domains(ctx, *t.config.Replication)
		if err != nil {
			return nil, fmt.Errorf("list replicas: %w", err)
		}
		add(remote)
	}
	for _, domain := range domains {
		if _, err := t.Open(ctx, domain); err != nil {
			return domains, fmt.Errorf("%s: %w", domain, err)
		}
	}
	return domains, nil
}

// Open returns the tenant of a domain, opening it once; concurrent callers
// wait for that one open.
func (t *Tenants) Open(ctx context.Context, domain string) (*Tenant, error) {
	for {
		t.mu.Lock()
		if tenant, ok := t.tenants[domain]; ok {
			t.mu.Unlock()
			return tenant, nil
		}
		wait, opening := t.opening[domain]
		if !opening {
			wait = make(chan struct{})
			t.opening[domain] = wait
		}
		t.mu.Unlock()
		if opening {
			select {
			case <-wait:
				continue
			case <-ctx.Done():
				return nil, ctx.Err()
			}
		}
		tenant, err := t.open(domain)
		t.mu.Lock()
		if err == nil {
			t.tenants[domain] = tenant
		}
		delete(t.opening, domain)
		close(wait)
		t.mu.Unlock()
		return tenant, err
	}
}

func (t *Tenants) databasePath(domain string) string {
	if t.config.SingleDatabase != "" {
		return t.config.SingleDatabase
	}
	return strings.TrimRight(t.config.DataDir, "/") + "/" + domain + ".db"
}

func (t *Tenants) open(domain string) (*Tenant, error) {
	path := t.databasePath(domain)
	logger := t.logger.With("tenant", domain)
	vfsName, fsPath := t.config.StorageVFS, ""
	if vfsName == "" {
		fsPath = path
	}
	var rep *replica.Replica
	if t.config.Replication != nil {
		var err error
		rep, err = replica.New(*t.config.Replication, domain, path, vfs.Find(t.config.StorageVFS), logger)
		if err != nil {
			return nil, err
		}
		if err := rep.Prepare(t.ctx); err != nil {
			rep.Close()
			return nil, fmt.Errorf("replica: %w", err)
		}
		vfsName = rep.VFSName()
	}
	database, err := persistence.Open(path, persistence.OpenOptions{VFS: vfsName, FSPath: fsPath})
	if err != nil {
		if rep != nil {
			rep.Close()
		}
		return nil, fmt.Errorf("open database %s: %w", path, err)
	}
	for _, line := range database.Migrations() {
		logger.Info("database", "migration", line)
	}
	fail := func(err error) (*Tenant, error) {
		_ = database.Close()
		if rep != nil {
			rep.Close()
		}
		return nil, err
	}
	if t.config.BeforeStore != nil {
		if err := t.config.BeforeStore(domain, database); err != nil {
			return fail(err)
		}
	}
	st, err := store.OpenWithBackend("state", store.OpenOptions{MasterKey: t.config.MasterKey, MasterKeyFile: t.config.MasterKeyFile}, database)
	if err != nil {
		return fail(fmt.Errorf("open store: %w", err))
	}
	attachments := database.Files("attachment:", "job-attachment", 15<<20)
	if t.config.AfterStore != nil {
		if err := t.config.AfterStore(domain, st, attachments); err != nil {
			return fail(err)
		}
	}
	token, err := st.EnsureWorkerToken(t.config.WorkerTokenSeed)
	if err != nil {
		return fail(fmt.Errorf("worker token: %w", err))
	}
	var engine capsule.Engine
	var broker *worker.Broker
	if t.config.Engine != nil {
		engine, broker, err = t.config.Engine(st, database, logger)
		if err != nil {
			return fail(err)
		}
	} else {
		broker = worker.NewBroker(st, logger)
		engine = worker.NewRemoteEngine(broker, database)
	}
	var options spinserver.ServerOptions
	if t.config.Options != nil {
		options = t.config.Options(domain)
	}
	options.WorkerToken = token
	options.RunnerBroker = broker
	options.AttachmentStorage = attachments
	options.SnapshotArchive = database
	options.Database = database
	if rep != nil {
		options.Replica = rep
	}
	server := spinserver.NewWithOptions(st, logger, engine, options)
	tenant := &Tenant{Domain: domain, Path: path, Database: database, Store: st, Broker: broker, Server: server, Handler: server.Handler(), Replica: rep}
	if rep != nil {
		rep.Attach(database)
		rep.Start(t.ctx)
	}
	logger.Info("tenant open", "database", path, "replicated", rep != nil)
	return tenant, nil
}

// Close stops every tenant's replica and closes its database.
func (t *Tenants) Close() error {
	t.cancel()
	t.mu.Lock()
	defer t.mu.Unlock()
	var errs []error
	for domain, tenant := range t.tenants {
		// The database closes through the replica's VFS: database first.
		if err := tenant.Database.Close(); err != nil {
			errs = append(errs, fmt.Errorf("%s: %w", domain, err))
		}
		if tenant.Replica != nil {
			tenant.Replica.Close()
		}
		delete(t.tenants, domain)
	}
	return errors.Join(errs...)
}
