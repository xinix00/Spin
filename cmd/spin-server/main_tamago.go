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
	"easyacp/internal/persistence"
	spinserver "easyacp/internal/server"
	"easyacp/internal/store"
	"easyacp/internal/worker"

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
	databasePath := strings.TrimSpace(app.Env("SPIN_DATABASE"))
	if databasePath == "" {
		databasePath = "/data/spin.db"
	}
	database, err := persistence.Open(databasePath, persistence.OpenOptions{VFS: persistence.RegisterHopVFS(app)})
	if err != nil {
		app.Logf("spin-server: open database: %v", err)
		app.Exit(1)
	}
	defer database.Close()
	if _, err := database.ReadFile("state"); errors.Is(err, fs.ErrNotExist) {
		if legacy, legacyErr := app.ReadFile("/data/spin-state.json"); legacyErr == nil {
			if err := database.WriteFile("state", legacy); err != nil {
				app.Logf("spin-server: import legacy state: %v", err)
				app.Exit(1)
			}
			app.Logf("spin-server: imported /data/spin-state.json into %s", databasePath)
		}
	}
	st, err := store.OpenWithBackend("state", store.OpenOptions{MasterKey: masterKey}, database)
	if err != nil {
		app.Logf("spin-server: open state: %v", err)
		app.Exit(1)
	}
	attachmentFiles := database.Files("attachment:", "job-attachment", 15<<20)
	for _, attachment := range st.Snapshot().JobAttachments {
		if _, err := attachmentFiles.ReadFile(attachment.ID); err == nil {
			continue
		}
		if legacy, legacyErr := app.ReadFile("/data/job-attachments/" + attachment.ID); legacyErr == nil {
			if err := attachmentFiles.WriteFile(attachment.ID, legacy); err != nil {
				app.Logf("spin-server: import attachment %s: %v", attachment.ID, err)
				app.Exit(1)
			}
		}
	}

	broker := worker.NewBroker(st, logger)
	engine := worker.NewRemoteEngine(broker, database)
	options := spinserver.ServerOptionsFromEnvironment()
	options.WorkerToken = workerToken
	options.RunnerBroker = broker
	options.AttachmentStorage = attachmentFiles
	options.SnapshotArchive = database
	options.Database = database

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
	app.Logf("spin-server: listening on :%s; database=%s", port, databasePath)
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
