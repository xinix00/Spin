//go:build !tamago

package main

import (
	"context"
	"flag"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"easyacp/internal/buildinfo"
	"easyacp/internal/capsule"
	"easyacp/internal/security"
	spinserver "easyacp/internal/server"
	"easyacp/internal/store"
	"easyacp/internal/worker"
)

func main() {
	showVersion := flag.Bool("version", false, "print version and exit")
	addr := flag.String("addr", "127.0.0.1:8080", "HTTP listen address")
	statePath := flag.String("state", "./var/spin-state.json", "persistent concept-state file")
	attachmentDir := flag.String("attachments", "./var/job-attachments", "persistent Job attachment directory")
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
	st, err := store.OpenWithOptions(*statePath, store.OpenOptions{MasterKey: os.Getenv("SPIN_MASTER_KEY"), MasterKeyFile: *masterKeyFile})
	if err != nil {
		logger.Error("open store", "error", err)
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
		engine = worker.NewRemoteEngine(runnerBroker)
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
			options.AttachmentDir = *attachmentDir
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

	logger.Info("Spin server listening", "addr", *addr, "state", *statePath, "capsule_driver", engine.Info().Driver, "capsule_base", engine.Info().BaseImage)
	if err := httpServer.ListenAndServe(); err != nil && err != http.ErrServerClosed {
		logger.Error("serve", "error", err)
		os.Exit(1)
	}
}

func envOr(name, fallback string) string {
	if value := strings.TrimSpace(os.Getenv(name)); value != "" {
		return value
	}
	return fallback
}
