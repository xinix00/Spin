//go:build !tamago

package main

import (
	"context"
	"errors"
	"flag"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"easyacp/internal/buildinfo"
	"easyacp/internal/capsule"
	"easyacp/internal/persistence"
	"easyacp/internal/replica"
	"easyacp/internal/security"
	spinserver "easyacp/internal/server"
	"easyacp/internal/store"
	"easyacp/internal/tenancy"
	"easyacp/internal/worker"
)

func main() {
	showVersion := flag.Bool("version", false, "print version and exit")
	addr := flag.String("addr", "127.0.0.1:8080", "HTTP listen address")
	dataDir := flag.String("data-dir", envOr("SPIN_DATA_DIR", "./var"), "directory with one database per domain (<domain>.db)")
	databasePath := flag.String("database", envOr("SPIN_DATABASE", ""), "serve every host from this one database instead of one per domain")
	domains := flag.String("domains", envOr("SPIN_DOMAINS", ""), "comma-separated domains this Spin answers on; empty admits any host")
	legacyStatePath := flag.String("state", "./var/spin-state.json", "legacy JSON state to import once (single database)")
	legacyAttachmentDir := flag.String("attachments", "./var/job-attachments", "legacy Job attachment directory to import once (single database)")
	capsuleDriver := flag.String("capsule-driver", "runner", "capsule engine: runner, docker or journal")
	capsuleBase := flag.String("capsule-base", "alpine:3.24", "clean substrate image for root Docker recordings")
	capsuleNetwork := flag.String("capsule-network", "bridge", "Docker network for capsule containers")
	masterKeyFile := flag.String("master-key-file", envOr("SPIN_MASTER_KEY_FILE", "./var/spin-master.key"), "AES master-key file for encrypted state secrets")
	workerTokenFile := flag.String("worker-token-file", envOr("SPIN_WORKER_TOKEN_FILE", "./var/spin-worker.token"), "worker token of an earlier deployment, seeds a database that has none")
	flag.Parse()
	if *showVersion {
		buildinfo.Print("spin-server")
		return
	}

	logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{Level: slog.LevelInfo}))
	replication, enabled, err := replica.ConfigFromEnvironment(os.Getenv)
	if err != nil {
		logger.Error("replication", "error", err)
		os.Exit(1)
	}
	if !enabled {
		logger.Warn("replication is off; the databases live on this machine only")
	}
	seed := strings.TrimSpace(os.Getenv("SPIN_WORKER_TOKEN"))
	if seed == "" {
		if token, err := security.ReadToken(*workerTokenFile); err == nil {
			seed = token
		}
	}
	single := strings.TrimSpace(*databasePath)
	config := tenancy.Config{
		DataDir: *dataDir, SingleDatabase: single,
		MasterKey: os.Getenv("SPIN_MASTER_KEY"), MasterKeyFile: *masterKeyFile,
		Domains: splitList(*domains), WorkerTokenSeed: seed, Logger: logger,
		Options: func(domain string) spinserver.ServerOptions {
			options := spinserver.ServerOptionsFromEnvironment()
			if options.PublicURL == "" && single == "" {
				options.PublicURL = "https://" + domain
			}
			return options
		},
	}
	if enabled {
		config.Replication = &replication
	}
	switch *capsuleDriver {
	case "runner":
	case "docker":
		config.Engine = func(*store.Store, *persistence.SQLite, *slog.Logger) (capsule.Engine, *worker.Broker, error) {
			probeCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()
			engine, err := capsule.NewDocker(probeCtx, capsule.DockerConfig{BaseImage: *capsuleBase, Network: *capsuleNetwork})
			return engine, nil, err
		}
	case "journal":
		config.Engine = func(*store.Store, *persistence.SQLite, *slog.Logger) (capsule.Engine, *worker.Broker, error) {
			return capsule.Journal{}, nil, nil
		}
	default:
		logger.Error("unknown capsule driver", "driver", *capsuleDriver)
		os.Exit(1)
	}
	if single != "" {
		config.BeforeStore = func(_ string, database *persistence.SQLite) error {
			imported, err := database.ImportFileIfMissing("state", *legacyStatePath)
			if imported {
				logger.Info("imported legacy JSON state", "source", *legacyStatePath, "database", single)
			}
			return err
		}
		config.AfterStore = func(_ string, st *store.Store, attachments *persistence.FileStore) error {
			return importLegacyAttachments(st, attachments, *legacyAttachmentDir)
		}
	}
	tenants := tenancy.New(config)
	defer tenants.Close()
	// Every known tenant opens at start: its replica restores and syncs
	// from the first minute, not from the first visitor.
	if single != "" {
		if _, err := tenants.Open(context.Background(), "spin"); err != nil {
			logger.Error("open database", "error", err)
			os.Exit(1)
		}
	}
	for _, domain := range config.Domains {
		if _, err := tenants.Open(context.Background(), domain); err != nil {
			logger.Error("open tenant", "domain", domain, "error", err)
			os.Exit(1)
		}
	}

	httpServer := &http.Server{
		Addr:              *addr,
		Handler:           tenants,
		ReadHeaderTimeout: 5 * time.Second,
		IdleTimeout:       60 * time.Second,
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = httpServer.Shutdown(shutdownCtx)
	}()

	logger.Info("Spin server listening", "addr", *addr, "data_dir", *dataDir, "single_database", single, "domains", config.Domains, "capsule_driver", *capsuleDriver, "replication", enabled)
	if err := httpServer.ListenAndServe(); err != nil && err != http.ErrServerClosed {
		logger.Error("serve", "error", err)
		os.Exit(1)
	}
}

type legacyAttachmentStore interface {
	ReadFile(string) ([]byte, error)
	WriteFile(string, []byte) error
}

func importLegacyAttachments(st *store.Store, destination legacyAttachmentStore, directory string) error {
	for _, attachment := range st.Snapshot().JobAttachments {
		if _, err := destination.ReadFile(attachment.ID); err == nil {
			continue
		}
		data, err := os.ReadFile(filepath.Join(directory, attachment.ID))
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return err
		}
		if err := destination.WriteFile(attachment.ID, data); err != nil {
			return err
		}
	}
	return nil
}

func envOr(name, fallback string) string {
	if value := strings.TrimSpace(os.Getenv(name)); value != "" {
		return value
	}
	return fallback
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
