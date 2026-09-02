package server

import (
	"bufio"
	"bytes"
	"encoding/base64"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
)

func TestSQLiteBackupRestoresStateSecretsAndAttachmentsUnderDestinationKey(t *testing.T) {
	sourceDirectory := t.TempDir()
	sourceKey := base64.RawStdEncoding.EncodeToString(bytes.Repeat([]byte{0x11}, 32))
	sourceDatabase, err := persistence.Open(filepath.Join(sourceDirectory, "spin.db"), persistence.OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer sourceDatabase.Close()
	source, err := store.OpenWithBackend("state", store.OpenOptions{MasterKey: sourceKey}, sourceDatabase)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := source.CreateInitialUser(domain.User{Username: "derek", DisplayName: "Derek", PasswordHash: "one-way-password-hash"}); err != nil {
		t.Fatal(err)
	}
	account, err := source.CreateGitAccount(domain.CreateGitAccountRequest{Operator: "derek", Provider: "github", Login: "derek", AccessToken: "portable-secret"})
	if err != nil {
		t.Fatal(err)
	}
	sourceAttachments := sourceDatabase.Files("attachment:", "job-attachment", maxJobAttachmentBytes)
	sourceServer := NewWithOptions(source, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{
		DisableAuthentication: true, Database: sourceDatabase, AttachmentStorage: sourceAttachments, SnapshotArchive: sourceDatabase,
	})
	png := append([]byte("\x89PNG\r\n\x1a\n"), bytes.Repeat([]byte{7}, 128)...)
	attachment := uploadAttachmentForTest(t, sourceServer.Handler(), "/api/job-attachments?operator=derek", "bewijs.png", png, http.StatusCreated)

	backupRequest := httptest.NewRequest(http.MethodPost, "/api/backup", nil)
	backupResponse := httptest.NewRecorder()
	sourceServer.Handler().ServeHTTP(backupResponse, backupRequest)
	if backupResponse.Code != http.StatusOK || !bytes.Contains([]byte(backupResponse.Header().Get("Content-Disposition")), []byte("spin-backup-")) {
		t.Fatalf("backup status=%d headers=%v body=%s", backupResponse.Code, backupResponse.Header(), backupResponse.Body.String())
	}
	if backupResponse.Header().Get("Content-Type") != "application/vnd.sqlite3" || backupResponse.Body.Len() < 4096 {
		t.Fatalf("backup content type=%q size=%d", backupResponse.Header().Get("Content-Type"), backupResponse.Body.Len())
	}

	destinationDirectory := t.TempDir()
	destinationKey := base64.RawStdEncoding.EncodeToString(bytes.Repeat([]byte{0x22}, 32))
	destinationPath := filepath.Join(destinationDirectory, "spin.db")
	destinationDatabase, err := persistence.Open(destinationPath, persistence.OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	destination, err := store.OpenWithBackend("state", store.OpenOptions{MasterKey: destinationKey}, destinationDatabase)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := destination.CreateInitialUser(domain.User{Username: "temporary", PasswordHash: "temporary-password-hash"}); err != nil {
		t.Fatal(err)
	}
	destinationAttachments := destinationDatabase.Files("attachment:", "job-attachment", maxJobAttachmentBytes)
	destinationServer := NewWithOptions(destination, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{
		DisableAuthentication: true, Database: destinationDatabase, AttachmentStorage: destinationAttachments, SnapshotArchive: destinationDatabase,
	})
	restoreRequest := httptest.NewRequest(http.MethodPost, "/api/restore", bytes.NewReader(backupResponse.Body.Bytes()))
	restoreRequest.Header.Set("Content-Type", "application/vnd.sqlite3")
	restoreRecorder := httptest.NewRecorder()
	destinationServer.Handler().ServeHTTP(restoreRecorder, restoreRequest)
	if restoreRecorder.Code != http.StatusOK {
		t.Fatalf("restore status=%d body=%s", restoreRecorder.Code, restoreRecorder.Body.String())
	}
	var restored restoreResponse
	if err := json.Unmarshal(restoreRecorder.Body.Bytes(), &restored); err != nil || restored.Status != "restored" || restored.Attachments != 1 {
		t.Fatalf("restore response=%+v error=%v", restored, err)
	}
	if _, err := destination.UserByUsername("derek"); err != nil {
		t.Fatalf("restored user: %v", err)
	}
	restoredAccount, err := destination.GitAccount(account.ID, "derek")
	if err != nil || restoredAccount.AccessToken != "portable-secret" {
		t.Fatalf("restored account=%+v error=%v", restoredAccount, err)
	}
	if got, err := destinationAttachments.ReadFile(attachment.ID); err != nil || !bytes.Equal(got, png) {
		t.Fatalf("restored attachment bytes=%d error=%v", len(got), err)
	}

	streamRequest := httptest.NewRequest(http.MethodPost, "/api/restore", bytes.NewReader(backupResponse.Body.Bytes()))
	streamRequest.Header.Set("Content-Type", "application/vnd.sqlite3")
	streamRequest.Header.Set("Accept", restoreProgressMediaType)
	streamRecorder := httptest.NewRecorder()
	destinationServer.Handler().ServeHTTP(streamRecorder, streamRequest)
	if streamRecorder.Code != http.StatusOK || streamRecorder.Header().Get("Content-Type") != restoreProgressMediaType {
		t.Fatalf("stream restore status=%d content-type=%q body=%s", streamRecorder.Code, streamRecorder.Header().Get("Content-Type"), streamRecorder.Body.String())
	}
	stages := map[string]bool{}
	var completed *restoreResponse
	scanner := bufio.NewScanner(bytes.NewReader(streamRecorder.Body.Bytes()))
	for scanner.Scan() {
		var event restoreProgressEvent
		if err := json.Unmarshal(scanner.Bytes(), &event); err != nil {
			t.Fatalf("decode progress %q: %v", scanner.Text(), err)
		}
		stages[event.Stage] = true
		if event.Type == "error" {
			t.Fatalf("stream restore error: %s", event.Error)
		}
		if event.Type == "complete" {
			completed = event.Result
		}
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	for _, stage := range []string{"open", "state", "attachments", "rollback", "install", "secrets", "complete"} {
		if !stages[stage] {
			t.Errorf("missing restore progress stage %q in %s", stage, streamRecorder.Body.String())
		}
	}
	if completed == nil || completed.Status != "restored" || completed.Attachments != 1 {
		t.Fatalf("stream completion = %+v", completed)
	}

	if err := destinationDatabase.Close(); err != nil {
		t.Fatal(err)
	}
	reopenedDatabase, err := persistence.Open(destinationPath, persistence.OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer reopenedDatabase.Close()
	reopened, err := store.OpenWithBackend("state", store.OpenOptions{MasterKey: destinationKey}, reopenedDatabase)
	if err != nil {
		t.Fatal(err)
	}
	reopenedAccount, err := reopened.GitAccount(account.ID, "derek")
	if err != nil || reopenedAccount.AccessToken != "portable-secret" {
		t.Fatalf("reopened account=%+v error=%v", reopenedAccount, err)
	}
}
