package main

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"flag"
	"log/slog"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"

	"easyacp/internal/buildinfo"
	"easyacp/internal/capsule"
	"easyacp/internal/security"
	"easyacp/internal/worker"
)

func main() {
	showVersion := flag.Bool("version", false, "print version and exit")
	serverURL := flag.String("server", envOr("SPIN_SERVER", "http://127.0.0.1:8080"), "Spin server URL")
	name := flag.String("name", envOr("SPIN_CLIENT_NAME", defaultName()), "client name")
	instanceID := flag.String("id", strings.TrimSpace(os.Getenv("SPIN_CLIENT_ID")), "stable client ID (generated when empty)")
	instanceIDFile := flag.String("id-file", envOr("SPIN_CLIENT_ID_FILE", "./var/spin-client.id"), "persistent client ID file")
	tools := flag.String("tools", "docker", "comma-separated advertised tools")
	maxWorkloads := flag.Int("max-workloads", envInt("SPIN_MAX_WORKLOADS", 4), "maximum concurrently materialized runtimes")
	capsuleBase := flag.String("capsule-base", "alpine:3.24", "clean substrate image for root Docker recordings")
	capsuleNetwork := flag.String("capsule-network", "bridge", "Docker network for capsule containers")
	envDir := flag.String("env-dir", envOr("SPIN_ENV_DIR", "./var/env"), "directory with <name>.env files that app services may use")
	advertiseHost := flag.String("advertise-host", strings.TrimSpace(os.Getenv("SPIN_ADVERTISE_HOST")), "address people use to reach published app ports (default: this machine's network address)")
	tokenFile := flag.String("token-file", envOr("SPIN_WORKER_TOKEN_FILE", "./var/spin-worker.token"), "shared worker bearer-token file")
	flag.Parse()
	if *showVersion {
		buildinfo.Print("spin-client")
		return
	}
	token := strings.TrimSpace(os.Getenv("SPIN_WORKER_TOKEN"))
	if token == "" {
		var err error
		token, err = security.WaitForToken(*tokenFile, 15*time.Second)
		if err != nil {
			logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{Level: slog.LevelInfo}))
			logger.Error("load worker token", "error", err)
			os.Exit(1)
		}
	}
	if *instanceID == "" {
		// An identity file from an earlier install is honoured. Without one the
		// identity follows the machine and the name: a runner redeployed from a
		// fresh directory (a HOP job) is the same runner, not a new card.
		if _, err := os.Stat(*instanceIDFile); err == nil {
			var loadErr error
			*instanceID, loadErr = security.LoadOrCreateToken(*instanceIDFile)
			if loadErr != nil {
				logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{Level: slog.LevelInfo}))
				logger.Error("load client identity", "error", loadErr)
				os.Exit(1)
			}
		} else {
			*instanceID = machineIdentity(*name)
		}
	}

	logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{Level: slog.LevelInfo}))
	probeCtx, probeCancel := context.WithTimeout(context.Background(), 15*time.Second)
	engine, err := capsule.NewDocker(probeCtx, capsule.DockerConfig{Binary: strings.TrimSpace(os.Getenv("SPIN_DOCKER")), BaseImage: *capsuleBase, Network: *capsuleNetwork, EnvDir: *envDir, AdvertiseHost: *advertiseHost, Logger: logger})
	probeCancel()
	if err != nil {
		logger.Error("start Docker capsule engine", "error", err)
		os.Exit(1)
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	config := worker.Config{
		ServerURL: *serverURL, InstanceID: *instanceID, Name: *name,
		Tools: splitList(*tools), MaxWorkloads: *maxWorkloads, Token: token, Engine: engine,
	}
	if err := worker.New(config, logger).Run(ctx); err != nil && !errors.Is(err, context.Canceled) {
		logger.Error("client stopped", "error", err)
		os.Exit(1)
	}
}

func envInt(name string, fallback int) int {
	if value, err := strconv.Atoi(strings.TrimSpace(os.Getenv(name))); err == nil && value > 0 {
		return value
	}
	return fallback
}

func envOr(name, fallback string) string {
	if value := strings.TrimSpace(os.Getenv(name)); value != "" {
		return value
	}
	return fallback
}

// machineIdentity is a stable instance ID for this host and runner name.
func machineIdentity(name string) string {
	host, _ := os.Hostname()
	sum := sha256.Sum256([]byte("spin-client\x00" + strings.ToLower(strings.TrimSpace(host)) + "\x00" + strings.TrimSpace(name)))
	return "host-" + hex.EncodeToString(sum[:12])
}

func defaultName() string {
	host, err := os.Hostname()
	if err != nil || host == "" {
		return "spin-client"
	}
	return host
}

func splitList(value string) []string {
	parts := strings.Split(value, ",")
	out := make([]string, 0, len(parts))
	for _, part := range parts {
		if value := strings.TrimSpace(part); value != "" {
			out = append(out, value)
		}
	}
	return out
}
