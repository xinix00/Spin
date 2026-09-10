package tenancy

import (
	"context"
	"easyacp/internal/capsule"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
	"easyacp/internal/worker"
	"encoding/json"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync"
	"testing"
	"time"

	spinserver "easyacp/internal/server"
)

func TestTenantsRouteOnHostAndKeepOneDatabasePerDomain(t *testing.T) {
	dir := t.TempDir()
	tenants := New(Config{
		DataDir: dir, Domains: []string{"one.test", "two.test"}, WorkerTokenSeed: "seed-token",
		Options: func(string) spinserver.ServerOptions { return spinserver.ServerOptions{DisableAuthentication: true} },
	})
	defer tenants.Close()
	get := func(host, path string) *httptest.ResponseRecorder {
		request := httptest.NewRequest(http.MethodGet, "http://"+host+path, nil)
		request.Host = host
		response := httptest.NewRecorder()
		tenants.ServeHTTP(response, request)
		return response
	}
	if response := get("one.test:8080", "/healthz"); response.Code != http.StatusOK {
		t.Fatalf("one.test healthz = %d %s", response.Code, response.Body.String())
	}
	if response := get("TWO.test.", "/healthz"); response.Code != http.StatusOK {
		t.Fatalf("two.test healthz = %d %s", response.Code, response.Body.String())
	}
	if response := get("three.test", "/healthz"); response.Code != http.StatusNotFound {
		t.Fatalf("unknown domain = %d", response.Code)
	}
	if response := get("10.0.0.7", "/healthz"); response.Code != http.StatusOK {
		t.Fatalf("liveness on an address = %d", response.Code)
	}
	if response := get("10.0.0.7", "/api/state"); response.Code != http.StatusNotFound {
		t.Fatalf("api on an address = %d", response.Code)
	}
	for _, name := range []string{"one.test.db", "two.test.db"} {
		if _, err := os.Stat(dir + "/" + name); err != nil {
			t.Fatalf("database %s: %v", name, err)
		}
	}
	if _, err := os.Stat(dir + "/three.test.db"); err == nil {
		t.Fatal("an unknown domain got a database")
	}
	one, err := tenants.Open(context.Background(), "one.test")
	if err != nil {
		t.Fatal(err)
	}
	two, err := tenants.Open(context.Background(), "two.test")
	if err != nil {
		t.Fatal(err)
	}
	if one.Store.WorkerToken() != "seed-token" || two.Store.WorkerToken() != "seed-token" || one.Store == two.Store {
		t.Fatalf("tokens = %q / %q", one.Store.WorkerToken(), two.Store.WorkerToken())
	}
	if _, err := one.Store.RotateWorkerToken(); err != nil {
		t.Fatal(err)
	}
	if one.Store.WorkerToken() == two.Store.WorkerToken() {
		t.Fatal("rotating one tenant's token changed the other")
	}
}

// Without a list every domain is welcome; an address never is; and the next
// start finds the databases again before anyone visits.
func TestTenantsAcceptAnyDomainAndDiscoverTheirDatabasesAtStart(t *testing.T) {
	dir := t.TempDir()
	options := func(string) spinserver.ServerOptions { return spinserver.ServerOptions{DisableAuthentication: true} }
	first := New(Config{DataDir: dir, Options: options})
	for _, host := range []string{"alpha.example", "beta.example"} {
		request := httptest.NewRequest(http.MethodGet, "http://"+host+"/healthz", nil)
		request.Host = host
		response := httptest.NewRecorder()
		first.ServeHTTP(response, request)
		if response.Code != http.StatusOK {
			t.Fatalf("%s = %d", host, response.Code)
		}
	}
	request := httptest.NewRequest(http.MethodGet, "http://10.1.1.1/api/state", nil)
	request.Host = "10.1.1.1"
	response := httptest.NewRecorder()
	first.ServeHTTP(response, request)
	if response.Code != http.StatusNotFound {
		t.Fatalf("address got a Spin: %d", response.Code)
	}
	if err := first.Close(); err != nil {
		t.Fatal(err)
	}
	second := New(Config{DataDir: dir, Options: options})
	defer second.Close()
	domains, err := second.Discover(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(domains) != 2 || domains[0] != "alpha.example" || domains[1] != "beta.example" || second.count() != 2 {
		t.Fatalf("discovered %v, open %d", domains, second.count())
	}
}

func TestNormalizeHost(t *testing.T) {
	cases := map[string]string{"Bollenloods.GetSpin.app:443": "bollenloods.getspin.app", "localhost": "localhost", "127.0.0.1:8080": "127.0.0.1", "a..b": "", "": "", "bad host": "", "-x.test": ""}
	for input, want := range cases {
		got, ok := NormalizeHost(input)
		if got != want || ok != (want != "") {
			t.Fatalf("NormalizeHost(%q) = %q, %v", input, got, ok)
		}
	}
}

// A Spin whose open takes long (a restore from the bucket) answers at once
// with where it stands, and the page asks again; a quick open goes through.
func TestSlowOpenAnswersWithItsStage(t *testing.T) {
	dir := t.TempDir()
	release := make(chan struct{})
	var once sync.Once
	tenants := New(Config{
		DataDir: dir,
		Options: func(string) spinserver.ServerOptions { return spinserver.ServerOptions{DisableAuthentication: true} },
		Engine: func(st *store.Store, database *persistence.SQLite, logger *slog.Logger) (capsule.Engine, *worker.Broker, error) {
			once.Do(func() { <-release })
			broker := worker.NewBroker(st, logger)
			return worker.NewRemoteEngine(broker, database), broker, nil
		},
	})
	defer tenants.Close()
	get := func(path string) *httptest.ResponseRecorder {
		request := httptest.NewRequest(http.MethodGet, "http://slow.test"+path, nil)
		request.Host = "slow.test"
		response := httptest.NewRecorder()
		tenants.ServeHTTP(response, request)
		return response
	}
	// A probe and a loaded page get the stage as JSON.
	first := get("/healthz")
	var body map[string]any
	if first.Code != http.StatusServiceUnavailable || json.Unmarshal(first.Body.Bytes(), &body) != nil || body["opening"] != true || body["stage"] == "" {
		t.Fatalf("while opening = %d %s", first.Code, first.Body.String())
	}
	if status := get(openingStatusPath); status.Code != http.StatusOK || json.Unmarshal(status.Body.Bytes(), &body) != nil || body["opening"] != true || body["message"] == "" {
		t.Fatalf("opening status = %d %s", status.Code, status.Body.String())
	}
	// A browser arriving now gets the splash page, which asks the status.
	page := get("/")
	if page.Code != http.StatusServiceUnavailable || !strings.HasPrefix(page.Header().Get("Content-Type"), "text/html") || !strings.Contains(page.Body.String(), openingStatusPath) || !strings.Contains(page.Body.String(), "<html") {
		t.Fatalf("splash page = %d %s %s", page.Code, page.Header().Get("Content-Type"), page.Body.String())
	}
	close(release)
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if response := get("/healthz"); response.Code == http.StatusOK {
			if status := get(openingStatusPath); status.Code != http.StatusOK || json.Unmarshal(status.Body.Bytes(), &body) != nil || body["opening"] != false {
				t.Fatalf("status after opening = %d %s", status.Code, status.Body.String())
			}
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatal("the Spin never opened")
}
