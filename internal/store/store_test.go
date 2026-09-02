package store

import (
	"encoding/base64"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"easyacp/internal/domain"
)

type memoryStateBackend struct{ files map[string][]byte }

func (b *memoryStateBackend) ReadFile(path string) ([]byte, error) {
	data, ok := b.files[path]
	if !ok {
		return nil, os.ErrNotExist
	}
	return append([]byte(nil), data...), nil
}

func (b *memoryStateBackend) WriteFile(path string, data []byte) error {
	b.files[path] = append([]byte(nil), data...)
	return nil
}

func TestArchiveUserRevokesSessionsAndCanBeRestored(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	owner, err := st.CreateInitialUser(domain.User{Username: "derek", DisplayName: "Derek", PasswordHash: "owner-hash"})
	if err != nil {
		t.Fatal(err)
	}
	john, err := st.CreateUser(owner.ID, domain.User{Username: "john", DisplayName: "John", Role: domain.UserMember, PasswordHash: "john-hash"})
	if err != nil {
		t.Fatal(err)
	}
	expires := time.Now().Add(time.Hour)
	if _, err := st.CreateAuthSession(john.ID, "john-token", "john-csrf", expires); err != nil {
		t.Fatal(err)
	}
	archived, err := st.SetUserArchived(owner.ID, john.ID, true)
	if err != nil {
		t.Fatal(err)
	}
	if archived.ArchivedAt == nil {
		t.Fatal("archived user has no archived_at")
	}
	if _, _, err := st.AuthenticateSession("john-token"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("archived login session remains valid: %v", err)
	}
	if _, err := st.CreateAuthSession(john.ID, "new-token", "new-csrf", expires); !errors.Is(err, ErrConflict) {
		t.Fatalf("archived user received a new session: %v", err)
	}
	if _, err := st.SetUserArchived(owner.ID, owner.ID, true); !errors.Is(err, ErrConflict) {
		t.Fatalf("owner archived their own identity: %v", err)
	}
	restored, err := st.SetUserArchived(owner.ID, john.ID, false)
	if err != nil {
		t.Fatal(err)
	}
	if restored.ArchivedAt != nil {
		t.Fatalf("restored user remains archived: %+v", restored)
	}
	if _, err := st.CreateAuthSession(john.ID, "restored-token", "restored-csrf", expires); err != nil {
		t.Fatalf("restored user cannot receive a session: %v", err)
	}
}

func TestOpenWithBackendPersistsWithoutHostFilesystem(t *testing.T) {
	backend := &memoryStateBackend{files: map[string][]byte{}}
	key := base64.RawStdEncoding.EncodeToString(make([]byte, 32))
	st, err := OpenWithBackend("/data/state.json", OpenOptions{MasterKey: key}, backend)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.CreateInitialUser(domain.User{Username: "derek", DisplayName: "Derek", PasswordHash: "one-way"}); err != nil {
		t.Fatal(err)
	}
	reopened, err := OpenWithBackend("/data/state.json", OpenOptions{MasterKey: key}, backend)
	if err != nil {
		t.Fatal(err)
	}
	if users := reopened.Snapshot().Users; len(users) != 1 || users[0].Username != "derek" {
		t.Fatalf("users after backend reopen = %+v", users)
	}
}

func TestSecretsAreEncryptedAtRestAndReopenWithMasterKey(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "state.json")
	keyPath := filepath.Join(directory, "keys", "master.key")
	st, err := OpenWithOptions(path, OpenOptions{MasterKeyFile: keyPath})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := st.CreateInitialUser(domain.User{Username: "derek", DisplayName: "Derek", PasswordHash: "password-hash-is-one-way"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.SaveGitOAuthConfiguration(owner.ID, domain.SaveGitOAuthConfigurationRequest{Provider: "github", ClientID: "oauth-client-id", ClientSecret: "oauth-client-secret"}); err != nil {
		t.Fatal(err)
	}
	mcp, err := st.CreateMCPServer(domain.CreateMCPServerRequest{Operator: "derek", Name: "private-mcp", Transport: domain.MCPTransportHTTP, URL: "https://mcp.example.test", Headers: []domain.MCPSecret{{Name: "Authorization", Value: "Bearer mcp-secret"}}})
	if err != nil {
		t.Fatal(err)
	}
	account, err := st.SaveGitAccount(domain.GitAccount{Operator: "derek", Provider: "github", Host: "github.com", Login: "derek", AccessToken: "git-access-secret", RefreshToken: "git-refresh-secret"})
	if err != nil {
		t.Fatal(err)
	}

	stored, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	for _, plaintext := range []string{"oauth-client-secret", "Bearer mcp-secret", "git-access-secret", "git-refresh-secret"} {
		if strings.Contains(string(stored), plaintext) {
			t.Fatalf("state contains plaintext %q: %s", plaintext, stored)
		}
	}
	if strings.Count(string(stored), encryptedValuePrefix) < 4 {
		t.Fatalf("state does not contain expected encrypted envelopes: %s", stored)
	}

	reopened, err := OpenWithOptions(path, OpenOptions{MasterKeyFile: keyPath})
	if err != nil {
		t.Fatal(err)
	}
	privateMCP, err := reopened.MCPServersForOperator("derek", []string{mcp.ID})
	if err != nil || len(privateMCP) != 1 || privateMCP[0].Headers[0].Value != "Bearer mcp-secret" {
		t.Fatalf("decrypted MCP = %+v; error = %v", privateMCP, err)
	}
	privateAccount, err := reopened.GitAccount(account.ID, "derek")
	if err != nil || privateAccount.AccessToken != "git-access-secret" || privateAccount.RefreshToken != "git-refresh-secret" {
		t.Fatalf("decrypted Git account = %+v; error = %v", privateAccount, err)
	}
	configuration, err := reopened.GitOAuthConfiguration("github")
	if err != nil || configuration.ClientSecret != "oauth-client-secret" {
		t.Fatalf("decrypted OAuth configuration = %+v; error = %v", configuration, err)
	}

	wrongKey := base64.RawStdEncoding.EncodeToString(make([]byte, 32))
	if _, err := OpenWithOptions(path, OpenOptions{MasterKey: wrongKey}); err == nil || !strings.Contains(err.Error(), "master key") {
		t.Fatalf("wrong key error = %v", err)
	}
}

func TestOpenMigratesLegacyPlaintextSecrets(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "state.json")
	legacy := `{"mcp_servers":{"mcp_legacy":{"id":"mcp_legacy","operator":"derek","name":"legacy","transport":"http","url":"https://mcp.example.test","headers":[{"name":"Authorization","value":"legacy-mcp-secret"}]}},"git_accounts":{"gac_legacy":{"id":"gac_legacy","operator":"derek","provider":"github","host":"github.com","login":"derek","access_token":"legacy-git-secret"}}}`
	if err := os.WriteFile(path, []byte(legacy), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := OpenWithOptions(path, OpenOptions{MasterKeyFile: filepath.Join(directory, "master.key")}); err != nil {
		t.Fatal(err)
	}
	stored, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(stored), "legacy-mcp-secret") || strings.Contains(string(stored), "legacy-git-secret") || strings.Count(string(stored), encryptedValuePrefix) != 2 {
		t.Fatalf("legacy secrets were not migrated: %s", stored)
	}
}

func TestOpenPermanentlyDropsLegacyRecordingTranscripts(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	legacy := `{"recordings":{"rec_secret":{"id":"rec_secret","actor":"derek","kind":"credential","name":"codex","sensitivity":"secret","status":"recording","commands":[{"sequence":1,"input":"oauth-secret-code","output":"oauth-secret-transcript"}]}},"artifacts":{}}`
	if err := os.WriteFile(path, []byte(legacy), 0o600); err != nil {
		t.Fatal(err)
	}
	st, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	recording, err := st.Recording("rec_secret")
	if err != nil {
		t.Fatal(err)
	}
	if len(recording.Commands) != 1 || recording.Commands[0].Sequence != 1 {
		t.Fatalf("minimal execution ledger = %+v", recording.Commands)
	}
	stored, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(stored), "oauth-secret-code") || strings.Contains(string(stored), "oauth-secret-transcript") {
		t.Fatalf("legacy transcript survived migration: %s", stored)
	}
}

func TestJobSessionCheckpointResultAndFork(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	st, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	gitTool := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "demo", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{gitTool.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "demo-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "easyacp", RemoteURL: "https://example.com/easyacp.git", DefaultRef: "main"})
	if err != nil {
		t.Fatal(err)
	}

	created, err := st.CreateJob(domain.CreateJobRequest{
		Title:               "Login herstellen",
		Objective:           "Maak de login-flow betrouwbaar",
		AcceptanceCriteria:  []string{"Tests slagen"},
		Owner:               "Derek",
		Operator:            "Derek",
		GitRepositoryID:     repository.Repository.ID,
		EnvironmentSelector: "tool:demo",
	})
	if err != nil {
		t.Fatal(err)
	}
	client, err := st.RegisterClient(domain.RegisterClientRequest{
		Name:         "test-client",
		Capabilities: domain.ClientCapabilities{Tools: []string{"demo"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	assignment, err := st.Claim(domain.ClaimRequest{ClientID: client.ID, Tools: []string{"demo"}})
	if err != nil {
		t.Fatal(err)
	}
	if assignment.Session.ID != created.Session.ID {
		t.Fatalf("claimed session %s, want %s", assignment.Session.ID, created.Session.ID)
	}

	stale := domain.CreateCheckpointRequest{
		ActivationID: assignment.Activation.ID,
		Epoch:        assignment.Activation.Epoch + 1,
		Kind:         domain.CheckpointSessionStart,
	}
	if _, err := st.AddCheckpoint(assignment.Session.ID, stale); !errors.Is(err, ErrStaleActivation) {
		t.Fatalf("stale checkpoint error = %v", err)
	}

	active := domain.ActivationRequest{ActivationID: assignment.Activation.ID, Epoch: assignment.Activation.Epoch}
	if _, err := st.StartSession(assignment.Session.ID, active); err != nil {
		t.Fatal(err)
	}
	start, err := st.AddCheckpoint(assignment.Session.ID, domain.CreateCheckpointRequest{
		ActivationID: active.ActivationID,
		Epoch:        active.Epoch,
		Kind:         domain.CheckpointSessionStart,
		Capsule:      domain.CapsuleManifest{Restorable: true, ProcessCheckpointDigest: "sha256:process"},
	})
	if err != nil {
		t.Fatal(err)
	}
	resultCheckpoint, err := st.AddCheckpoint(assignment.Session.ID, domain.CreateCheckpointRequest{
		ActivationID: active.ActivationID,
		Epoch:        active.Epoch,
		Kind:         domain.CheckpointResult,
		Capsule:      domain.CapsuleManifest{Restorable: true, ProcessCheckpointDigest: "sha256:result"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if resultCheckpoint.ParentCheckpointID != start.ID {
		t.Fatalf("parent checkpoint = %s, want %s", resultCheckpoint.ParentCheckpointID, start.ID)
	}

	result, err := st.CompleteSession(assignment.Session.ID, domain.CreateResultRequest{
		ActivationID: active.ActivationID,
		Epoch:        active.Epoch,
		CheckpointID: resultCheckpoint.ID,
		Status:       domain.ResultSuccess,
		Summary:      "Login gerepareerd",
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.CompleteSession(assignment.Session.ID, domain.CreateResultRequest{
		ActivationID: active.ActivationID,
		Epoch:        active.Epoch,
		CheckpointID: resultCheckpoint.ID,
	}); !errors.Is(err, ErrStaleActivation) {
		t.Fatalf("second completion error = %v", err)
	}

	child, err := st.ForkSession(assignment.Session.ID, domain.ForkSessionRequest{
		CheckpointID:   resultCheckpoint.ID,
		InputResultIDs: []string{result.ID},
		ForkMode:       domain.ForkFull,
		ObjectiveDelta: "Probeer een kleinere wijziging",
	})
	if err != nil {
		t.Fatal(err)
	}
	if child.ParentSessionID != assignment.Session.ID || child.ContinuityScore != 95 {
		t.Fatalf("unexpected child: %+v", child)
	}
	job, err := st.SelectResult(created.Job.ID, domain.SelectResultRequest{ResultID: result.ID})
	if err != nil {
		t.Fatal(err)
	}
	if job.Status != domain.JobDone || job.FinalResultID != result.ID {
		t.Fatalf("unexpected final job: %+v", job)
	}

	reopened, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	snapshot := reopened.Snapshot()
	if len(snapshot.Jobs) != 1 || len(snapshot.Sessions) != 2 || len(snapshot.Results) != 1 || len(snapshot.Checkpoints) != 2 {
		t.Fatalf("unexpected persisted snapshot: %+v", snapshot)
	}
}

func TestUseResolvesCurrentOperatorsCredential(t *testing.T) {
	path := filepath.Join(t.TempDir(), "typed-snapshots.json")
	st, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}

	tool := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal,
	})
	johnCredential := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "john", Kind: domain.ArtifactCredential, Name: "codex", Scope: domain.ScopeUser, ParentArtifactIDs: []string{tool.ID},
	})
	derekCredential := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactCredential, Name: "codex", Scope: domain.ScopeUser, ParentArtifactIDs: []string{tool.ID},
	})
	if len(johnCredential.ParentArtifactIDs) != 1 || johnCredential.ParentArtifactIDs[0] != tool.ID {
		t.Fatalf("credential parent = %+v, want tool %s", johnCredential.ParentArtifactIDs, tool.ID)
	}

	john, err := st.Use(domain.UseRequest{Selector: "credential:codex", Operator: "john"})
	if err != nil {
		t.Fatal(err)
	}
	derek, err := st.Use(domain.UseRequest{Selector: "credential:codex", Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	if john.SlotBindings["tool:codex"] != tool.ID || derek.SlotBindings["tool:codex"] != tool.ID {
		t.Fatalf("tool binding differs: john=%+v derek=%+v", john.SlotBindings, derek.SlotBindings)
	}
	if john.SlotBindings["credential:codex"] != johnCredential.ID {
		t.Fatalf("john credential = %s, want %s", john.SlotBindings["credential:codex"], johnCredential.ID)
	}
	if derek.SlotBindings["credential:codex"] != derekCredential.ID {
		t.Fatalf("derek credential = %s, want %s", derek.SlotBindings["credential:codex"], derekCredential.ID)
	}
	if john.SlotBindings["credential:codex"] == derek.SlotBindings["credential:codex"] {
		t.Fatal("personal credentials resolved to the same artifact")
	}
	unarmed, err := st.Use(domain.UseRequest{Selector: "tool:codex", Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := unarmed.SlotBindings["credential:codex"]; ok {
		t.Fatalf("tool selector unexpectedly injected a credential: %+v", unarmed.SlotBindings)
	}

	if _, err := st.Use(domain.UseRequest{Selector: "credential:codex", Operator: "alice"}); !errors.Is(err, ErrNotFound) {
		t.Fatalf("alice using another user's credential error = %v", err)
	}

	reopened, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	persisted := reopened.Snapshot()
	if len(persisted.Artifacts) != 3 || len(persisted.Compositions) != 3 || len(persisted.Recordings) != 3 {
		t.Fatalf("typed state was not persisted: %+v", persisted)
	}
}

func TestEveryArtifactKindMayBeRecordedWithoutAParent(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	recording, err := st.CreateRecording(domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactCredential, Name: "codex", Scope: domain.ScopeUser,
	})
	if err != nil {
		t.Fatalf("credential without parent: %v", err)
	}
	if len(recording.ParentArtifactIDs) != 0 {
		t.Fatalf("unexpected parents: %+v", recording.ParentArtifactIDs)
	}
}

func TestUseResolvesUserScopedVariantForAnyLayerKind(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	global := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "admin", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal,
	})
	john := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "john", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeUser, ParentArtifactIDs: []string{global.ID},
	})

	johnComposition, err := st.Use(domain.UseRequest{Selector: "tool:git", Operator: "john"})
	if err != nil {
		t.Fatal(err)
	}
	derekComposition, err := st.Use(domain.UseRequest{Selector: "tool:git", Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	if johnComposition.EntryArtifactID != john.ID || johnComposition.SlotBindings["tool:git"] != john.ID {
		t.Fatalf("John did not receive his tool layer: %+v", johnComposition)
	}
	if derekComposition.EntryArtifactID != global.ID || derekComposition.SlotBindings["tool:git"] != global.ID {
		t.Fatalf("Derek did not fall back to the global tool layer: %+v", derekComposition)
	}
}

func TestUseSessionLateBindsCompositionToOperator(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	gitTool := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "admin", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	tool := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "admin", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{gitTool.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}}})
	derekCredential := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactCredential, Name: "codex", Scope: domain.ScopeUser, ParentArtifactIDs: []string{tool.ID}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "handoff", RemoteURL: "https://example.com/handoff.git"})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Handoff", Objective: "Ga verder", Owner: "john", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "credential:codex"})
	if err != nil {
		t.Fatal(err)
	}
	composition, err := st.Use(domain.UseRequest{SessionID: created.Session.ID, Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	if composition.SlotBindings["credential:codex"] != derekCredential.ID {
		t.Fatalf("composition = %+v", composition)
	}
	client, err := st.RegisterClient(domain.RegisterClientRequest{Name: "worker", Capabilities: domain.ClientCapabilities{Tools: []string{"codex"}}})
	if err != nil {
		t.Fatal(err)
	}
	assignment, err := st.Claim(domain.ClaimRequest{ClientID: client.ID, Tools: []string{"codex"}})
	if err != nil {
		t.Fatal(err)
	}
	if assignment.Activation.Operator != "derek" || assignment.Activation.CompositionID != composition.ID {
		t.Fatalf("activation did not inherit prepared composition: %+v", assignment.Activation)
	}
	if assignment.Activation.CredentialBindings["credential:codex"] != derekCredential.ID {
		t.Fatalf("activation bindings = %+v", assignment.Activation.CredentialBindings)
	}
}

func TestUseFallsBackToGlobalCredentialWithoutBorrowingAUserCredential(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	tool := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "admin", Kind: domain.ArtifactTool, Name: "cloudflared", Scope: domain.ScopeGlobal})
	globalCredential := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "admin", Kind: domain.ArtifactCredential, Name: "cloudflared", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{tool.ID}})

	composition, err := st.Use(domain.UseRequest{Selector: "credential:cloudflared", Operator: "john"})
	if err != nil {
		t.Fatal(err)
	}
	if composition.SlotBindings["credential:cloudflared"] != globalCredential.ID {
		t.Fatalf("global credential not resolved: %+v", composition)
	}
	if got := composition.ResolvedArtifacts[len(composition.ResolvedArtifacts)-1].Reason; got != "selected credential:cloudflared" {
		t.Fatalf("resolution reason = %q", got)
	}
}

func TestArtifactDeletionIsOwnedAndDependencySafe(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	parent := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactTool, Name: "node", Scope: domain.ScopeGlobal,
	})
	child := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{parent.ID},
	})

	if _, err := st.DeleteArtifact(parent.ID, "derek"); !errors.Is(err, ErrConflict) {
		t.Fatalf("deleting a referenced parent error = %v", err)
	}
	if _, err := st.DeleteArtifact(child.ID, "john"); !errors.Is(err, ErrConflict) {
		t.Fatalf("deleting somebody else's artifact error = %v", err)
	}
	if _, err := st.DeleteArtifact(child.ID, "derek"); err != nil {
		t.Fatal(err)
	}
	snapshot := st.Snapshot()
	if len(snapshot.Artifacts) != 1 || snapshot.Artifacts[0].ID != parent.ID {
		t.Fatalf("artifacts after delete = %+v", snapshot.Artifacts)
	}
	if len(snapshot.Recordings) != 1 || snapshot.Recordings[0].ArtifactID != parent.ID {
		t.Fatalf("recording history after delete = %+v", snapshot.Recordings)
	}
}

func TestGitRepositoryLateResolvesEachOperatorsProviderAccount(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	gitTool := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "admin", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	agent := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "admin", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{gitTool.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}}})
	derekAccount, err := st.CreateGitAccount(domain.CreateGitAccountRequest{Operator: "derek", Provider: "github", Login: "derek", Name: "Derek", Email: "derek@example.com", AccessToken: "secret-derek"})
	if err != nil {
		t.Fatal(err)
	}
	johnAccount, err := st.CreateGitAccount(domain.CreateGitAccountRequest{Operator: "john", Provider: "github", Login: "john", Name: "John", Email: "john@example.com", AccessToken: "secret-john"})
	if err != nil {
		t.Fatal(err)
	}
	if derekAccount.AccessToken != "" || johnAccount.AccessToken != "" {
		t.Fatal("Git account create response leaked a token")
	}
	privateDerekAccount, err := st.GitAccount(derekAccount.ID, "derek")
	if err != nil || privateDerekAccount.AccessToken != "secret-derek" {
		t.Fatalf("private account lookup = %+v, %v", privateDerekAccount, err)
	}
	if _, err := st.GitAccount(derekAccount.ID, "john"); !errors.Is(err, ErrConflict) {
		t.Fatalf("cross-user Git account lookup error = %v", err)
	}
	foreignRepository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "foreign", RemoteURL: "https://example.com/foreign.git", CredentialScope: domain.CredentialScopeUser})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.ResolveGitAccount(foreignRepository.Repository.ID, "derek"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("cross-host Git account resolution error = %v", err)
	}

	createdRepository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{
		Operator: "derek", Name: "easyacp", RemoteURL: "https://github.com/example/easyacp.git",
		CredentialScope: domain.CredentialScopeUser,
	})
	if err != nil {
		t.Fatal(err)
	}
	if createdRepository.Repository.Provider != "github" || createdRepository.Repository.CredentialScope != domain.CredentialScopeUser {
		t.Fatalf("repository identity metadata = %+v", createdRepository.Repository)
	}

	created, err := st.CreateJob(domain.CreateJobRequest{
		Title: "Real Git", Objective: "Use isolated branches", Operator: "derek",
		GitRepositoryID: createdRepository.Repository.ID, EnvironmentSelector: "tool:codex",
	})
	if err != nil {
		t.Fatal(err)
	}
	derekComposition, err := st.Use(domain.UseRequest{SessionID: created.Session.ID, Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	if derekComposition.Git == nil || derekComposition.Git.AccountID != "" || derekComposition.Git.CredentialScope != domain.CredentialScopeUser || derekComposition.Git.Login != "derek" || derekComposition.SlotBindings["tool:git"] != gitTool.ID {
		t.Fatalf("Derek composition Git = %+v; slots=%+v", derekComposition.Git, derekComposition.SlotBindings)
	}
	if err := st.DiscardComposition(derekComposition.ID, "derek"); err != nil {
		t.Fatal(err)
	}
	child, err := st.CreateJobSession(created.Job.ID, domain.CreateJobSessionRequest{
		Operator: "john", EnvironmentSelector: "tool:codex", ObjectiveDelta: "Continue as John",
	})
	if err != nil {
		t.Fatal(err)
	}
	johnComposition, err := st.Use(domain.UseRequest{SessionID: child.ID, Operator: "john"})
	if err != nil {
		t.Fatal(err)
	}
	if johnComposition.Git == nil || johnComposition.Git.AccountID != "" || johnComposition.Git.CredentialScope != domain.CredentialScopeUser || johnComposition.Git.Login != "john" {
		t.Fatalf("John composition Git = %+v", johnComposition.Git)
	}
	if created.Job.Branch == created.Session.GitRef || !strings.HasSuffix(created.Job.Branch, "/main") || !strings.Contains(child.GitRef, "/sessions/") {
		t.Fatalf("invalid real branch topology: job=%s root=%s child=%s", created.Job.Branch, created.Session.GitRef, child.GitRef)
	}
	for _, account := range st.Snapshot().GitAccounts {
		if account.AccessToken != "" || account.RefreshToken != "" {
			t.Fatalf("public snapshot leaked Git account secret: %+v", account)
		}
	}
	if agent.ID == "" {
		t.Fatal("agent fixture was not created")
	}
}

func TestGitRepositoryGlobalScopeResolvesSharedProviderAccount(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	global, err := st.CreateGitAccount(domain.CreateGitAccountRequest{
		Operator: "admin", Provider: "github", Host: "github.com", Login: "spin-bot",
		AccessToken: "global-secret", CredentialScope: domain.CredentialScopeGlobal,
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.CreateGitAccount(domain.CreateGitAccountRequest{
		Operator: "john", Provider: "github", Host: "github.com", Login: "john",
		AccessToken: "john-secret", CredentialScope: domain.CredentialScopeUser,
	}); err != nil {
		t.Fatal(err)
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{
		Operator: "admin", Name: "shared", RemoteURL: "https://github.com/example/shared.git",
		CredentialScope: domain.CredentialScopeGlobal,
	})
	if err != nil {
		t.Fatal(err)
	}
	resolved, err := st.ResolveGitAccount(repository.Repository.ID, "john")
	if err != nil {
		t.Fatal(err)
	}
	if resolved.ID != global.ID || resolved.Login != "spin-bot" || resolved.AccessToken != "global-secret" {
		t.Fatalf("global account = %+v", resolved)
	}
}

func TestGitRepositoryEditOnlyAffectsNewJobs(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	gitTool := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{gitTool.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{
		Operator: "derek", Name: "old-name", RemoteURL: "https://example.com/old.git", DefaultRef: "main",
		CredentialScope: domain.CredentialScopePublic,
	})
	if err != nil {
		t.Fatal(err)
	}
	oldJob, err := st.CreateJob(domain.CreateJobRequest{Title: "Old target", Objective: "Keep it stable", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent"})
	if err != nil {
		t.Fatal(err)
	}
	updated, err := st.UpdateGitRepository(repository.Repository.ID, domain.UpdateGitRepositoryRequest{
		Operator: "derek", Name: "new-name", RemoteURL: "https://example.com/new.git", DefaultRef: "trunk",
		CredentialScope: domain.CredentialScopePublic,
	})
	if err != nil || updated.Name != "new-name" || updated.RemoteURL != "https://example.com/new.git" || updated.DefaultRef != "trunk" {
		t.Fatalf("updated repository = %+v, error = %v", updated, err)
	}
	oldComposition, err := st.Use(domain.UseRequest{SessionID: oldJob.Session.ID, Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	if oldComposition.Git.RepositoryName != "old-name" || oldComposition.Git.RemoteURL != "https://example.com/old.git" || oldComposition.Git.BootstrapRef != "main" {
		t.Fatalf("old Job target changed after repository edit: %+v", oldComposition.Git)
	}
	if err := st.DiscardComposition(oldComposition.ID, "derek"); err != nil {
		t.Fatal(err)
	}
	newJob, err := st.CreateJob(domain.CreateJobRequest{Title: "New target", Objective: "Use edited settings", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent"})
	if err != nil {
		t.Fatal(err)
	}
	newComposition, err := st.Use(domain.UseRequest{SessionID: newJob.Session.ID, Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	if newComposition.Git.RepositoryName != "new-name" || newComposition.Git.RemoteURL != "https://example.com/new.git" || newComposition.Git.BootstrapRef != "trunk" {
		t.Fatalf("new Job did not use edited target: %+v", newComposition.Git)
	}
}

func TestJobRejectsMissingRepositoryAndGitLayer(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "project", RemoteURL: "https://example.com/project.git"})
	if err != nil {
		t.Fatal(err)
	}
	request := domain.CreateJobRequest{Title: "No Git", Objective: "Must fail", Operator: "derek", EnvironmentSelector: "tool:codex"}
	if _, err := st.CreateJob(request); !errors.Is(err, ErrConflict) {
		t.Fatalf("missing repository error = %v", err)
	}
	request.GitRepositoryID = repository.Repository.ID
	if _, err := st.CreateJob(request); !errors.Is(err, ErrConflict) || !strings.Contains(err.Error(), "ENABLE git") {
		t.Fatalf("missing Git layer error = %v", err)
	}
}

func TestMCPBindingAndJobSessionBranchTopology(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	gitTool := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}},
	})
	tool := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{gitTool.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}},
	})
	dotnet := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactTool, Name: "dotnet", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{tool.ID},
	})
	server, err := st.CreateMCPServer(domain.CreateMCPServerRequest{
		Operator:  "Derek",
		Name:      "GitHub personal",
		Transport: domain.MCPTransportStdio,
		Command:   "/usr/local/bin/github-mcp",
		Args:      []string{"--feature", "issues", "--feature", "pulls"},
		Env:       []domain.MCPSecret{{Name: "GITHUB_TOKEN", Value: "never-return-this"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if server.Env[0].Value != "" {
		t.Fatalf("create response leaked MCP secret: %+v", server)
	}
	private, err := st.MCPServersForOperator("derek", []string{server.ID})
	if err != nil {
		t.Fatal(err)
	}
	if private[0].Env[0].Value != "never-return-this" || strings.Join(private[0].Args, " ") != "--feature issues --feature pulls" {
		t.Fatalf("private MCP handoff lost data: %+v", private[0])
	}
	if _, err := st.MCPServersForOperator("john", []string{server.ID}); !errors.Is(err, ErrNotFound) {
		t.Fatalf("other user reading MCP config error = %v", err)
	}
	if got := st.Snapshot().MCPServers[0].Env[0].Value; got != "" {
		t.Fatalf("public snapshot leaked MCP secret %q", got)
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "mcp-handoff", RemoteURL: "https://example.com/mcp-handoff.git", DefaultRef: "main"})
	if err != nil {
		t.Fatal(err)
	}

	created, err := st.CreateJob(domain.CreateJobRequest{
		Title: "Review MCP handoff", Objective: "Verify the generic handoff", Operator: "derek",
		GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:codex", WithSelectors: []string{"tool:dotnet"}, MCPServerIDs: []string{server.ID},
	})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(created.Job.Branch, "jobs/review-mcp-handoff-") || !strings.HasSuffix(created.Job.Branch, "/main") || created.Session.GitRef != sessionGitRef(created.Job, created.Session.ID) || created.Session.TargetBranch != created.Job.Branch {
		t.Fatalf("root branch topology = job:%+v session:%+v", created.Job, created.Session)
	}
	composition, err := st.Use(domain.UseRequest{SessionID: created.Session.ID, Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	if composition.EntryArtifactID != tool.ID || len(composition.RequestedArtifactIDs) != 2 || composition.RequestedArtifactIDs[1] != dotnet.ID || len(composition.MCPServerIDs) != 1 || composition.MCPServerIDs[0] != server.ID {
		t.Fatalf("composition bindings = %+v", composition)
	}

	child, err := st.CreateJobSession(created.Job.ID, domain.CreateJobSessionRequest{
		Operator: "derek", EnvironmentSelector: "tool:codex", ObjectiveDelta: "Review the primary output",
		Role: "reviewer", SpawnedBySessionID: created.Session.ID,
	})
	if err != nil {
		t.Fatal(err)
	}
	if child.ParentSessionID != created.Session.ID || child.SpawnedBySessionID != created.Session.ID || child.Role != "reviewer" || child.GitRef != sessionGitRef(created.Job, child.ID) || child.TargetBranch != created.Job.Branch || child.GitRepositoryID != repository.Repository.ID {
		t.Fatalf("child branch topology = %+v", child)
	}
	if len(child.MCPServerIDs) != 1 || child.MCPServerIDs[0] != server.ID {
		t.Fatalf("child did not inherit job MCP binding: %+v", child.MCPServerIDs)
	}
	if len(child.WithSelectors) != 1 || child.WithSelectors[0] != "tool:dotnet" {
		t.Fatalf("child did not inherit job WITH layers: %+v", child.WithSelectors)
	}
	if _, err := st.CreateJobSession(created.Job.ID, domain.CreateJobSessionRequest{
		Operator: "derek", EnvironmentSelector: "tool:codex",
	}); !errors.Is(err, ErrConflict) {
		t.Fatalf("empty child objective error = %v", err)
	}
}

func TestJobRecipeUsesEnabledCapabilitiesAcrossTemplateRepositoryAndJob(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	gitLayer := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactConfig, Name: "forge", Scope: domain.ScopeGlobal,
		Enables: []domain.Enablement{{Name: "git"}},
	})
	dotnet := recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactTool, Name: "dotnet", Scope: domain.ScopeGlobal,
		ParentArtifactIDs: []string{gitLayer.ID},
	})
	recordArtifact(t, st, domain.CreateRecordingRequest{
		Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal,
		ParentArtifactIDs: []string{dotnet.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}},
	})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{
		Operator: "derek", Name: "capability-driven", RemoteURL: "https://example.com/capability-driven.git",
	})
	if err != nil {
		t.Fatal(err)
	}
	updatedRepository, err := st.UpdateGitRepository(repository.Repository.ID, domain.UpdateGitRepositoryRequest{
		Operator: "derek", LayerSelectors: []string{"tool:dotnet"},
	})
	if err != nil || len(updatedRepository.LayerSelectors) != 1 || updatedRepository.LayerSelectors[0] != "tool:dotnet" {
		t.Fatalf("updated repository layers = %+v, error = %v", updatedRepository.LayerSelectors, err)
	}
	if _, err := st.UpdateGitRepository(repository.Repository.ID, domain.UpdateGitRepositoryRequest{
		Operator: "derek", LayerSelectors: []string{"tool:codex"},
	}); !errors.Is(err, ErrConflict) {
		t.Fatalf("ACP environment accepted as project layer: %v", err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{
		Operator: "derek", Name: "Generic Git", GitSelector: "config:forge",
		Phases: []domain.WorkflowPhase{{ID: "build", Name: "Build", Instructions: "Build it", AllowChanges: true}},
	})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{
		Title: "Compose by capability", Objective: "Keep responsibilities separate", Operator: "derek",
		GitRepositoryID: repository.Repository.ID, TemplateID: template.ID, EnvironmentSelector: "tool:codex",
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(created.Job.WithSelectors) != 2 || created.Job.WithSelectors[0] != "config:forge" || created.Job.WithSelectors[1] != "tool:dotnet" {
		t.Fatalf("frozen Job recipe = %+v", created.Job.WithSelectors)
	}
	composition, err := st.Use(domain.UseRequest{SessionID: created.Session.ID, Operator: "derek"})
	if err != nil {
		t.Fatal(err)
	}
	if !enablementsContain(composition.Enabled, "git") || !enablementsContain(composition.Enabled, "acp") {
		t.Fatalf("enabled capabilities = %+v", composition.Enabled)
	}
}

func recordArtifact(t *testing.T, st *Store, req domain.CreateRecordingRequest) domain.Artifact {
	t.Helper()
	recording, err := st.CreateRecording(req)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.RecordExecution(recording.ID, req.Actor, nil); err != nil {
		t.Fatal(err)
	}
	artifact, err := st.EndRecording(recording.ID, domain.EndRecordingRequest{Actor: req.Actor})
	if err != nil {
		t.Fatal(err)
	}
	return artifact
}

func TestRunnerIdentityReconnectsAndExpiredLeaseDoesNotReassignSession(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "demo", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "demo"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "sticky", RemoteURL: "https://example.com/sticky.git"})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Sticky", Objective: "Stay put", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:demo"})
	if err != nil {
		t.Fatal(err)
	}
	first, err := st.RegisterClient(domain.RegisterClientRequest{InstanceID: "stable-one", Name: "laptop", Capabilities: domain.ClientCapabilities{Tools: []string{"demo"}}})
	if err != nil {
		t.Fatal(err)
	}
	reconnected, err := st.RegisterClient(domain.RegisterClientRequest{InstanceID: "stable-one", Name: "laptop-renamed", Capabilities: domain.ClientCapabilities{Tools: []string{"demo"}}})
	if err != nil || reconnected.ID != first.ID || len(st.Snapshot().Clients) != 1 {
		t.Fatalf("reconnected client = %+v, first = %+v, error = %v", reconnected, first, err)
	}
	assignment, err := st.Claim(domain.ClaimRequest{ClientID: first.ID, Tools: []string{"demo"}})
	if err != nil {
		t.Fatal(err)
	}
	past := time.Now().Add(-time.Minute)
	st.mu.Lock()
	session := st.state.Sessions[created.Session.ID]
	session.LeaseExpiresAt = &past
	st.state.Sessions[session.ID] = session
	st.mu.Unlock()
	second, err := st.RegisterClient(domain.RegisterClientRequest{InstanceID: "stable-two", Name: "server", Capabilities: domain.ClientCapabilities{Tools: []string{"demo"}}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.Claim(domain.ClaimRequest{ClientID: second.ID, Tools: []string{"demo"}}); !errors.Is(err, ErrNoWork) {
		t.Fatalf("expired Session silently failed over: %v", err)
	}
	retained := st.state.Sessions[created.Session.ID]
	if retained.ClientID != first.ID || retained.ActivationID != assignment.Activation.ID || retained.Status != domain.SessionClaimed {
		t.Fatalf("Session affinity changed after lease expiry: %+v", retained)
	}
}

func TestRunnerDrainSurvivesReconnectAndBlocksNewClaims(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "demo", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "demo"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "drain", RemoteURL: "https://example.com/drain.git"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.CreateJob(domain.CreateJobRequest{Title: "Queued", Objective: "Wait for a runner", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:demo"}); err != nil {
		t.Fatal(err)
	}
	client, err := st.RegisterClient(domain.RegisterClientRequest{InstanceID: "drain-me", Name: "laptop", Capabilities: domain.ClientCapabilities{Tools: []string{"demo"}}})
	if err != nil {
		t.Fatal(err)
	}
	drained, err := st.SetClientDraining(client.ID, true, true)
	if err != nil {
		t.Fatal(err)
	}
	if !drained.Draining || drained.Status != "draining" {
		t.Fatalf("drained client = %+v", drained)
	}
	reconnected, err := st.RegisterClient(domain.RegisterClientRequest{InstanceID: "drain-me", Name: "laptop", Capabilities: domain.ClientCapabilities{Tools: []string{"demo"}}})
	if err != nil {
		t.Fatal(err)
	}
	if !reconnected.Draining || reconnected.Status != "draining" {
		t.Fatalf("drain did not survive reconnect: %+v", reconnected)
	}
	if _, err := st.Claim(domain.ClaimRequest{ClientID: client.ID, Tools: []string{"demo"}}); !errors.Is(err, ErrNoWork) {
		t.Fatalf("drained runner claimed new work: %v", err)
	}
	resumed, err := st.SetClientDraining(client.ID, false, true)
	if err != nil {
		t.Fatal(err)
	}
	if resumed.Draining || resumed.Status != "online" {
		t.Fatalf("resumed client = %+v", resumed)
	}
	if _, err := st.Claim(domain.ClaimRequest{ClientID: client.ID, Tools: []string{"demo"}}); err != nil {
		t.Fatalf("resumed runner cannot claim queued work: %v", err)
	}
}
