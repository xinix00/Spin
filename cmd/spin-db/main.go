//go:build !tamago

// spin-db performs explicit, offline database migrations. It never mutates a
// Docker image: image save is streamed straight into Spin's central archive.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
)

type legacyState struct {
	Artifacts      map[string]domain.Artifact      `json:"artifacts"`
	JobAttachments map[string]domain.JobAttachment `json:"job_attachments"`
}

func main() {
	databasePath := flag.String("database", "./var/spin.db", "Spin SQLite database")
	statePath := flag.String("state", "./var/spin-state.json", "legacy Spin JSON state")
	attachmentDir := flag.String("attachments", "./var/job-attachments", "legacy attachment directory")
	dockerCommand := flag.String("docker", "docker", "Docker command")
	backupOutput := flag.String("backup", "", "optional portable .db backup output")
	masterKeyFile := flag.String("master-key-file", "", "master key included only in the portable backup copy")
	flag.Parse()

	database, err := persistence.Open(*databasePath, persistence.OpenOptions{})
	check(err)
	defer database.Close()
	imported, err := database.ImportFileIfMissing("state", *statePath)
	check(err)
	if imported {
		fmt.Printf("state: imported %s\n", *statePath)
	}
	stateJSON, err := database.ReadFile("state")
	check(err)
	var state legacyState
	check(json.Unmarshal(stateJSON, &state))

	attachments := database.Files("attachment:", "job-attachment", 15<<20)
	for _, attachment := range state.JobAttachments {
		if _, err := attachments.ReadFile(attachment.ID); err == nil {
			continue
		}
		data, err := os.ReadFile(filepath.Join(*attachmentDir, attachment.ID))
		if errors.Is(err, fs.ErrNotExist) {
			fmt.Printf("attachment %s: source missing, skipped\n", attachment.ID)
			continue
		}
		check(err)
		check(attachments.WriteFile(attachment.ID, data))
		fmt.Printf("attachment %s: %d bytes\n", attachment.ID, len(data))
	}

	ctx := context.Background()
	for _, artifact := range state.Artifacts {
		snapshot := artifact.Snapshot
		if !snapshot.Restorable || strings.TrimSpace(snapshot.Ref) == "" {
			continue
		}
		if present, err := database.HasSnapshot(ctx, snapshot); err != nil {
			check(err)
		} else if present {
			check(database.RestoreSnapshot(ctx, snapshot, io.Discard))
			fmt.Printf("snapshot %s: archived and verified\n", artifact.ID)
			continue
		}
		started := time.Now()
		command := exec.CommandContext(ctx, *dockerCommand, "image", "save", snapshot.Ref)
		command.Stderr = os.Stderr
		stdout, err := command.StdoutPipe()
		check(err)
		check(command.Start())
		storeErr := database.StoreSnapshot(ctx, snapshot, stdout)
		waitErr := command.Wait()
		check(errors.Join(storeErr, waitErr))
		info, err := database.BlobInfo(ctx, "snapshot:"+snapshot.Digest)
		check(err)
		check(database.RestoreSnapshot(ctx, snapshot, io.Discard))
		fmt.Printf("snapshot %s: %d bytes in %s\n", artifact.ID, info.Size, time.Since(started).Round(time.Second))
	}
	if strings.TrimSpace(*backupOutput) != "" {
		if strings.TrimSpace(*masterKeyFile) == "" {
			check(errors.New("-master-key-file is required with -backup"))
		}
		key, err := os.ReadFile(*masterKeyFile)
		check(err)
		output, err := os.OpenFile(*backupOutput, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
		check(err)
		backupErr := database.WriteBackup(ctx, output, strings.TrimSpace(string(key)))
		closeErr := output.Close()
		check(errors.Join(backupErr, closeErr))
		fmt.Printf("backup: %s\n", *backupOutput)
	}
	fmt.Printf("ready: %s\n", *databasePath)
}

func check(err error) {
	if err == nil {
		return
	}
	fmt.Fprintln(os.Stderr, "spin-db:", err)
	os.Exit(1)
}
