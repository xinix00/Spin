package tenancy

import (
	"context"
	"encoding/json"
	"errors"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync"
	"testing"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/persistence"
	spinserver "easyacp/internal/server"
	"easyacp/internal/store"
	"easyacp/internal/worker"
	"easyacp/replica"
)

// A restore can outlast several lease renewals. Losing the real lease while
// Prepare waits for the bucket must cancel that request and fail the opening.
func TestLeaseLossCancelsTenantPrepare(t *testing.T) {
	requestStarted := make(chan struct{})
	requestCanceled := make(chan struct{})
	stop := make(chan struct{})
	var startedOnce, canceledOnce sync.Once
	bucket := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		startedOnce.Do(func() { close(requestStarted) })
		select {
		case <-r.Context().Done():
			canceledOnce.Do(func() { close(requestCanceled) })
		case <-stop:
			w.WriteHeader(http.StatusServiceUnavailable)
		}
	}))
	defer bucket.Close()
	defer close(stop)
	tenants := New(Config{
		DataDir: t.TempDir(),
		Replication: &replica.Config{
			Endpoint: bucket.URL, Bucket: "bucket", AccessKey: "access", SecretKey: "secret",
		},
	})
	defer tenants.Close()
	const domain = "lease-restore.test"
	done := tenants.startOpen(domain)
	select {
	case <-requestStarted:
	case <-time.After(10 * time.Second):
		t.Fatal("Prepare did not contact the bucket")
	}
	leasePath := tenants.databasePath(domain) + ".lease"
	replacement, err := json.Marshal(map[string]any{"owner": "successor", "expires_at": time.Now().Add(time.Minute)})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(leasePath, replacement, 0o600); err != nil {
		t.Fatal(err)
	}
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("lease loss did not cancel Prepare")
	}
	if tenant, stage := tenants.lookup(domain); tenant != nil || !strings.Contains(stage.Error, replica.ErrLeaseLost.Error()) {
		t.Fatalf("opening after lease loss: tenant=%v stage=%+v", tenant, stage)
	}
	select {
	case <-requestCanceled:
	case <-time.After(2 * time.Second):
		t.Fatal("the opening's bucket request was not canceled")
	}
	if _, err := os.Stat(tenants.databasePath(domain)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("the canceled opening created its database: %v", err)
	}
	data, err := os.ReadFile(leasePath)
	if err != nil || string(data) != string(replacement) {
		t.Fatalf("cleanup modified the successor's lease: %q %v", data, err)
	}
}

// Pause after openHeld has constructed the complete tenant, immediately before
// runOpen publishes it. Canceling only Prepare, or checking the lease earlier
// in openHeld, misses this race.
func TestLeaseLossBeforeTenantPublicationFailsClosed(t *testing.T) {
	ready := make(chan struct{})
	release := make(chan struct{})
	var readyOnce, releaseOnce sync.Once
	defer releaseOnce.Do(func() { close(release) })
	logger := slog.New(tenantLeaseTestLog{record: func(record slog.Record) {
		if record.Message == "tenant open" {
			readyOnce.Do(func() {
				close(ready)
				<-release
			})
		}
	}})
	databases := make(chan *persistence.SQLite, 2)
	tenants := New(Config{
		DataDir: t.TempDir(), Logger: logger,
		Options: func(string) spinserver.ServerOptions { return spinserver.ServerOptions{DisableAuthentication: true} },
		Engine: func(st *store.Store, database *persistence.SQLite, logger *slog.Logger) (capsule.Engine, *worker.Broker, error) {
			databases <- database
			broker := worker.NewBroker(st, logger)
			return worker.NewRemoteEngine(broker, database), broker, nil
		},
	})
	defer func() {
		releaseOnce.Do(func() { close(release) })
		tenants.Close()
	}()
	const domain = "lease-publication.test"
	type result struct {
		tenant *Tenant
		err    error
	}
	opened := make(chan result, 1)
	go func() {
		tenant, err := tenants.Open(context.Background(), domain)
		opened <- result{tenant, err}
	}()
	select {
	case <-ready:
	case <-time.After(10 * time.Second):
		t.Fatal("tenant did not reach publication")
	}
	tenants.mu.Lock()
	opening := tenants.opening[domain]
	tenants.mu.Unlock()
	if opening == nil {
		t.Fatal("opening disappeared before publication")
	}
	tenants.leaseLost(domain, opening, replica.ErrLeaseLost)
	releaseOnce.Do(func() { close(release) })
	select {
	case result := <-opened:
		if result.tenant != nil || result.err == nil || !strings.Contains(result.err.Error(), replica.ErrLeaseLost.Error()) {
			t.Fatalf("Open after lease loss returned tenant=%v err=%v", result.tenant, result.err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("failed opening did not finish cleanup")
	}
	if tenant, _ := tenants.lookup(domain); tenant != nil {
		t.Fatal("tenant was published after losing its lease")
	}
	if err := (<-databases).WriteFile("after-loss", []byte("must fail")); err == nil {
		t.Fatal("the failed opening left its database writable")
	}
	if _, err := os.Stat(tenants.databasePath(domain) + ".lease"); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("the failed opening did not release its lease: %v", err)
	}
	// A later successful attempt has its own identity; a stale loss signal
	// from the failed attempt cannot remove or close it.
	tenant, err := tenants.Open(context.Background(), domain)
	if err != nil {
		t.Fatal(err)
	}
	tenants.leaseLost(domain, opening, replica.ErrLeaseLost)
	if current, _ := tenants.lookup(domain); current != tenant {
		t.Fatal("an old lease callback removed the replacement tenant")
	}
	if err := tenant.Database.WriteFile("after-reopen", []byte("ok")); err != nil {
		t.Fatalf("the replacement tenant is not writable: %v", err)
	}
}

func TestCloseWaitsForTenantOpeningCleanup(t *testing.T) {
	ready := make(chan *persistence.SQLite, 1)
	release := make(chan struct{})
	var releaseOnce sync.Once
	tenants := New(Config{
		DataDir: t.TempDir(),
		Engine: func(st *store.Store, database *persistence.SQLite, logger *slog.Logger) (capsule.Engine, *worker.Broker, error) {
			ready <- database
			<-release
			broker := worker.NewBroker(st, logger)
			return worker.NewRemoteEngine(broker, database), broker, nil
		},
	})
	defer func() {
		releaseOnce.Do(func() { close(release) })
		tenants.Close()
	}()
	const domain = "close-opening.test"
	openingDone := tenants.startOpen(domain)
	var database *persistence.SQLite
	select {
	case database = <-ready:
	case <-time.After(10 * time.Second):
		t.Fatal("opening did not reach the engine")
	}
	tenants.mu.Lock()
	opening := tenants.opening[domain]
	tenants.mu.Unlock()
	closed := make(chan error, 1)
	go func() { closed <- tenants.Close() }()
	select {
	case <-opening.ctx.Done():
	case <-time.After(10 * time.Second):
		t.Fatal("Close did not cancel the opening")
	}
	select {
	case err := <-closed:
		t.Fatalf("Close returned before opening cleanup: %v", err)
	case <-time.After(100 * time.Millisecond):
	}
	releaseOnce.Do(func() { close(release) })
	select {
	case err := <-closed:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("Close did not finish after opening cleanup")
	}
	select {
	case <-openingDone:
	default:
		t.Fatal("Close returned before the opening completed")
	}
	if err := database.WriteFile("after-close", []byte("must fail")); err == nil {
		t.Fatal("Close left the opening's database writable")
	}
	if tenant, err := tenants.Open(context.Background(), domain); tenant != nil || err == nil {
		t.Fatalf("closed tenancy reopened a database: tenant=%v err=%v", tenant, err)
	}
	if _, err := os.Stat(tenants.databasePath(domain) + ".lease"); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("Close left the opening's lease held: %v", err)
	}
}

type tenantLeaseTestLog struct{ record func(slog.Record) }

func (tenantLeaseTestLog) Enabled(context.Context, slog.Level) bool { return true }
func (h tenantLeaseTestLog) Handle(_ context.Context, record slog.Record) error {
	h.record(record)
	return nil
}
func (h tenantLeaseTestLog) WithAttrs([]slog.Attr) slog.Handler { return h }
func (h tenantLeaseTestLog) WithGroup(string) slog.Handler      { return h }
