//go:build tamago

package main

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"log/slog"
	"net/http"
	"os"
	"strings"
	"time"

	"easyacp/internal/buildinfo"
	"easyacp/internal/persistence"
	"easyacp/internal/replicaconfig"
	spinserver "easyacp/internal/server"
	"easyacp/internal/tenancy"

	"github.com/xinix00/HopOS/metal/v2/app/applib"
	"github.com/xinix00/HopOS/metal/v2/app/applib/appnet"
	_ "golang.org/x/crypto/x509roots/fallback"
)

// ringWriter keeps all service output in the HopOS task log.
type ringWriter struct{ app *applib.App }

func (w ringWriter) Write(p []byte) (int, error) {
	w.app.Logf("%s", strings.TrimRight(string(p), "\n"))
	return len(p), nil
}

func main() {
	app := applib.Init()
	app.Logf("%s", buildinfo.String("spin-server"))
	logger := slog.New(slog.NewTextHandler(ringWriter{app: app}, &slog.HandlerOptions{Level: slog.LevelInfo}))
	if _, err := appnet.Up(app); err != nil {
		fatal(app, "spin-server: net: %v", err)
	}
	app.Logf("spin-server: network up")

	bridgeEnvironment(app,
		"SPIN_PUBLIC_URL", "SPIN_INTERNAL_URL", "SPIN_GITHUB_CLIENT_ID", "SPIN_GITHUB_CLIENT_SECRET",
		"SPIN_GITLAB_CLIENT_ID", "SPIN_GITLAB_CLIENT_SECRET", "SPIN_WORKER_TOKEN",
	)
	masterKey := strings.TrimSpace(app.Env("SPIN_MASTER_KEY"))
	if masterKey == "" {
		masterKey = ephemeralMasterKey()
		app.Logf("spin-server: WARNING: SPIN_MASTER_KEY is empty; encrypted credentials cannot survive a restart")
	}
	replication, enabled, err := replicaconfig.FromEnvironment(app.Env)
	if err != nil {
		fatal(app, "spin-server: %v", err)
	}
	if !enabled {
		app.Logf("spin-server: WARNING: replication is off; the databases live on this volume only")
	} else {
		app.Logf("spin-server: replication to %s bucket %s prefix %s", replication.Endpoint, replication.Bucket, replication.Prefix)
	}
	dataDir := strings.TrimSpace(app.Env("SPIN_DATA_DIR"))
	if dataDir == "" {
		dataDir = "/data"
	}
	single := strings.TrimSpace(app.Env("SPIN_DATABASE"))
	config := tenancy.Config{
		DataDir: dataDir, SingleDatabase: single, StorageVFS: persistence.RegisterHopVFS(app),
		MasterKey: masterKey, Domains: splitList(app.Env("SPIN_DOMAINS")),
		WorkerTokenSeed: strings.TrimSpace(app.Env("SPIN_WORKER_TOKEN")), Logger: logger,
		Options: func(domain string) spinserver.ServerOptions {
			options := spinserver.ServerOptionsFromEnvironment()
			if options.PublicURL == "" && single == "" {
				options.PublicURL = "https://" + domain
			}
			if options.InternalURL == "" && single == "" {
				// Agents reach the workflow tools over the same public URL.
				options.InternalURL = options.PublicURL
			}
			return options
		},
	}
	if enabled {
		config.Replication = &replication
	}
	tenants := tenancy.New(config)
	// The known Spins open in the background: a restore from the bucket
	// can take a while, and the port must answer meanwhile.
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Hour)
		defer cancel()
		app.Logf("spin-server: opening the Spins this server holds")
		domainsOpen, err := tenants.Discover(ctx)
		if err != nil {
			app.Logf("spin-server: open tenants: %v (open so far: %v)", err, domainsOpen)
			return
		}
		app.Logf("spin-server: tenants open: %v", domainsOpen)
	}()

	port := strings.TrimSpace(app.Env("ER_PORT_HTTP"))
	if port == "" {
		port = "8080"
	}
	server := &http.Server{
		Addr:              ":" + port,
		Handler:           tenants,
		ReadHeaderTimeout: 5 * time.Second,
		IdleTimeout:       60 * time.Second,
	}
	app.Logf("spin-server: listening on :%s; data=%s single=%q domains=%v replication=%v", port, dataDir, single, config.Domains, enabled)
	fatal(app, "spin-server: http: %v", server.ListenAndServe())
}

// fatal logs the reason and leaves; the pause lets the line reach the
// task log, an exit right after a write loses it.
func fatal(app *applib.App, format string, args ...any) {
	app.Logf(format, args...)
	time.Sleep(time.Second)
	app.Exit(1)
}

func bridgeEnvironment(app *applib.App, names ...string) {
	for _, name := range names {
		if value := app.Env(name); value != "" {
			_ = os.Setenv(name, value)
		}
	}
}

func splitList(value string) []string {
	var out []string
	for _, part := range strings.Split(value, ",") {
		if part = strings.TrimSpace(part); part != "" {
			out = append(out, part)
		}
	}
	return out
}

func ephemeralMasterKey() string {
	key := make([]byte, 32)
	if _, err := rand.Read(key); err != nil {
		panic(err)
	}
	return base64.RawStdEncoding.EncodeToString(key)
}
