//go:build tamago

package main

import (
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
	spinserver "easyacp/internal/server"
	"easyacp/internal/store"
	"easyacp/internal/worker"

	"github.com/xinix00/HopOS/metal/app/applib"
	"github.com/xinix00/HopOS/metal/app/applib/appnet"
	_ "golang.org/x/crypto/x509roots/fallback"
)

// ringWriter keeps all service output in the HopOS task log.
type ringWriter struct{ app *applib.App }

func (w ringWriter) Write(p []byte) (int, error) {
	w.app.Logf("%s", strings.TrimRight(string(p), "\n"))
	return len(p), nil
}

// hopStateBackend stores Spin's JSON state through the hop-ABI filesystem.
// A mounted /data volume therefore survives an app replacement without
// pretending the freestanding runtime has a Unix filesystem.
type hopStateBackend struct{ app *applib.App }

func (b hopStateBackend) ReadFile(path string) ([]byte, error) {
	return b.app.ReadFile(path)
}

func (b hopStateBackend) WriteFile(path string, data []byte) error {
	return b.app.WriteFile(path, data)
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
	workerToken := strings.TrimSpace(app.Env("SPIN_WORKER_TOKEN"))
	if workerToken == "" {
		app.Logf("spin-server: SPIN_WORKER_TOKEN is required so remote clients can authenticate")
		app.Exit(1)
	}
	masterKey := strings.TrimSpace(app.Env("SPIN_MASTER_KEY"))
	if masterKey == "" {
		masterKey = ephemeralMasterKey()
		app.Logf("spin-server: WARNING: SPIN_MASTER_KEY is empty; encrypted credentials cannot survive a restart")
	}
	statePath := strings.TrimSpace(app.Env("SPIN_STATE"))
	if statePath == "" {
		statePath = "/data/spin-state.json"
	}
	st, err := store.OpenWithBackend(statePath, store.OpenOptions{MasterKey: masterKey}, hopStateBackend{app: app})
	if err != nil && errors.Is(err, fs.ErrNotExist) {
		st, err = store.OpenWithBackend("", store.OpenOptions{MasterKey: masterKey}, hopStateBackend{app: app})
	}
	if err != nil {
		app.Logf("spin-server: open state: %v", err)
		app.Exit(1)
	}

	broker := worker.NewBroker(st, logger)
	engine := worker.NewRemoteEngine(broker)
	options := spinserver.ServerOptionsFromEnvironment()
	options.WorkerToken = workerToken
	options.RunnerBroker = broker
	options.AttachmentDir = "" // HopOS attachments need an explicit volume adapter; fail closed for now.

	port := strings.TrimSpace(app.Env("ER_PORT_HTTP"))
	if port == "" {
		port = "8080"
	}
	server := &http.Server{
		Addr:              ":" + port,
		Handler:           spinserver.NewWithOptions(st, logger, engine, options).Handler(),
		ReadHeaderTimeout: 5 * time.Second,
		IdleTimeout:       60 * time.Second,
	}
	app.Logf("%s", buildinfo.String("spin-server"))
	app.Logf("spin-server: listening on :%s; state=%s", port, statePath)
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

func ephemeralMasterKey() string {
	key := make([]byte, 32)
	if _, err := rand.Read(key); err != nil {
		panic(err)
	}
	return base64.RawStdEncoding.EncodeToString(key)
}
