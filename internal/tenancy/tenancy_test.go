package tenancy

import (
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"testing"

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

func TestNormalizeHost(t *testing.T) {
	cases := map[string]string{"Bollenloods.GetSpin.app:443": "bollenloods.getspin.app", "localhost": "localhost", "127.0.0.1:8080": "127.0.0.1", "a..b": "", "": "", "bad host": "", "-x.test": ""}
	for input, want := range cases {
		got, ok := NormalizeHost(input)
		if got != want || ok != (want != "") {
			t.Fatalf("NormalizeHost(%q) = %q, %v", input, got, ok)
		}
	}
}
