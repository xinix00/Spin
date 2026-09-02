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
	"easyacp/internal/security"
	spinserver "easyacp/internal/server"
	"easyacp/internal/store"
	"easyacp/internal/worker"
)

func main() {
	showVersion := flag.Bool("version", false, "print version and exit")
	addr := flag.String("addr", "127.0.0.1:8080", "HTTP listen address")
	databasePath := flag.String("database", envOr("SPIN_DATABASE", "./var/spin.db"), "persistent Spin SQLite database")
	legacyStatePath := flag.String("state", "./var/spin-state.json", "legacy JSON state to import once")
	legacyAttachmentDir := flag.String("attachments", "./var/job-attachments", "legacy Job attachment directory to import once")
	capsuleDriver := flag.String("capsule-driver", "runner", "capsule engine: runner, docker or journal")
	capsuleBase := flag.String("capsule-base", "alpine:3.24", "clean substrate image for root Docker recordings")
	capsuleNetwork := flag.String("capsule-network", "bridge", "Docker network for capsule containers")
	masterKeyFile := flag.String("master-key-file", envOr("SPIN_MASTER_KEY_FILE", "./var/spin-master.key"), "AES master-key file for encrypted state secrets")
	workerTokenFile := flag.String("worker-token-file", envOr("SPIN_WORKER_TOKEN_FILE", "./var/spin-worker.token"), "shared worker bearer-token file")
	flag.Parse()
	if *showVersion {
		buildinfo.Print("spin-server")
		return
	}

	logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{Level: slog.LevelInfo}))
	database, err := persistence.Open(*databasePath, persistence.OpenOptions{})
	if err != nil {
		logger.Error("open database", "error", err)
		os.Exit(1)
	}
	defer database.Close()
	if imported, err := database.ImportFileIfMissing("state", *legacyStatePath); err != nil {
		logger.Error("import legacy JSON state", "error", err)
		os.Exit(1)
	} else if imported {
		logger.Info("imported legacy JSON state", "source", *legacyStatePath, "database", *databasePath)
	}
	st, err := store.OpenWithBackend("state", store.OpenOptions{MasterKey: os.Getenv("SPIN_MASTER_KEY"), MasterKeyFile: *masterKeyFile}, database)
	if err != nil {
		logger.Error("open store", "error", err)
		os.Exit(1)
	}
	attachmentFiles := database.Files("attachment:", "job-attachment", 15<<20)
	if err := importLegacyAttachments(st, attachmentFiles, *legacyAttachmentDir); err != nil {
		logger.Error("import legacy Job attachments", "error", err)
		os.Exit(1)
	}
	workerToken := strings.TrimSpace(os.Getenv("SPIN_WORKER_TOKEN"))
	if workerToken == "" {
		workerToken, err = security.LoadOrCreateToken(*workerTokenFile)
		if err != nil {
			logger.Error("load worker token", "error", err)
			os.Exit(1)
		}
	}
	var engine capsule.Engine = capsule.Journal{}
	var runnerBroker *worker.Broker
	if *capsuleDriver == "runner" {
		runnerBroker = worker.NewBroker(st, logger)
		engine = worker.NewRemoteEngine(runnerBroker, database)
	} else if *capsuleDriver == "docker" {
		probeCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		engine, err = capsule.NewDocker(probeCtx, capsule.DockerConfig{BaseImage: *capsuleBase, Network: *capsuleNetwork})
		if err != nil {
			logger.Error("start Docker capsule engine", "error", err)
			os.Exit(1)
		}
	} else if *capsuleDriver != "journal" {
		logger.Error("unknown capsule driver", "driver", *capsuleDriver)
		os.Exit(1)
	}

	httpServer := &http.Server{
		Addr: *addr,
		Handler: func() http.Handler {
			options := spinserver.ServerOptionsFromEnvironment()
			options.WorkerToken = workerToken
			options.AttachmentStorage = attachmentFiles
			options.SnapshotArchive = database
			options.Database = database
			options.RunnerBroker = runnerBroker
			return spinserver.NewWithOptions(st, logger, engine, options).Handler()
		}(),
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

	logger.Info("Spin server listening", "addr", *addr, "database", *databasePath, "capsule_driver", engine.Info().Driver, "capsule_base", engine.Info().BaseImage)
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
