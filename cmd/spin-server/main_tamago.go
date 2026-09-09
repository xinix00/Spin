//go:build tamago

package main

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"errors"
	"io/fs"
	"log/slog"
	"net/http"
	"os"
	"strings"
	"time"

	"easyacp/internal/buildinfo"
	"easyacp/internal/persistence"
	"easyacp/internal/replica"
	spinserver "easyacp/internal/server"
	"easyacp/internal/store"
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
	logger := slog.New(slog.NewTextHandler(ringWriter{app: app}, &slog.HandlerOptions{Level: slog.LevelInfo}))
	if _, err := appnet.Up(app); err != nil {
		app.Logf("spin-server: net: %v", err)
		app.Exit(1)
	}

	bridgeEnvironment(app,
		"SPIN_PUBLIC_URL", "SPIN_INTERNAL_URL", "SPIN_GITHUB_CLIENT_ID", "SPIN_GITHUB_CLIENT_SECRET",
		"SPIN_GITLAB_CLIENT_ID", "SPIN_GITLAB_CLIENT_SECRET", "SPIN_WORKER_TOKEN",
	)
	masterKey := strings.TrimSpace(app.Env("SPIN_MASTER_KEY"))
	if masterKey == "" {
		masterKey = ephemeralMasterKey()
		app.Logf("spin-server: WARNING: SPIN_MASTER_KEY is empty; encrypted credentials cannot survive a restart")
	}
	replication, enabled, err := replica.ConfigFromEnvironment(app.Env)
	if err != nil {
		app.Logf("spin-server: %v", err)
		app.Exit(1)
	}
	if !enabled {
		app.Logf("spin-server: WARNING: replication is off; the databases live on this volume only")
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
		BeforeStore: func(_ string, database *persistence.SQLite) error {
			if single == "" {
				return nil
			}
			if _, err := database.ReadFile("state"); !errors.Is(err, fs.ErrNotExist) {
				return nil
			}
			legacy, legacyErr := app.ReadFile("/data/spin-state.json")
			if legacyErr != nil {
				return nil
			}
			if err := database.WriteFile("state", legacy); err != nil {
				return err
			}
			app.Logf("spin-server: imported /data/spin-state.json into %s", single)
			return nil
		},
		AfterStore: func(_ string, st *store.Store, attachments *persistence.FileStore) error {
			if single == "" {
				return nil
			}
			for _, attachment := range st.Snapshot().JobAttachments {
				if _, err := attachments.ReadFile(attachment.ID); err == nil {
					continue
				}
				if legacy, legacyErr := app.ReadFile("/data/job-attachments/" + attachment.ID); legacyErr == nil {
					if err := attachments.WriteFile(attachment.ID, legacy); err != nil {
						return err
					}
				}
			}
			return nil
		},
	}
	if enabled {
		config.Replication = &replication
	}
	tenants := tenancy.New(config)
	domainsOpen, err := tenants.Discover(context.Background())
	if err != nil {
		app.Logf("spin-server: open tenants: %v", err)
		app.Exit(1)
	}
	app.Logf("spin-server: tenants open: %v", domainsOpen)

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
	app.Logf("%s", buildinfo.String("spin-server"))
	app.Logf("spin-server: listening on :%s; data=%s single=%q domains=%v replication=%v", port, dataDir, single, config.Domains, enabled)
	app.Logf("spin-server: http: %v", server.ListenAndServe())
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
