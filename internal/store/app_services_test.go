package store

import (
	"errors"
	"slices"
	"strings"
	"testing"
	"time"

	"easyacp/internal/domain"
)

// A repository's app recipe is checked when it is saved: names, one of run
// or image, ports and env names.
func TestRepositoryServicesAreValidated(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	create := func(services []domain.AppService) error {
		_, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "app-" + strings.ReplaceAll(t.Name(), "/", "-") + services[0].Name, RemoteURL: "https://github.com/derek/" + services[0].Name + ".git", Services: services})
		return err
	}
	if err := create([]domain.AppService{{Name: "web", Run: "npm start", Ports: []int{3000}, Env: "easyflor"}, {Name: "db", Image: "postgres:16"}}); err != nil {
		t.Fatalf("valid recipe refused: %v", err)
	}
	for name, services := range map[string][]domain.AppService{
		"no run or image":    {{Name: "x"}},
		"both run and image": {{Name: "x", Run: "a", Image: "b"}},
		"bad port":           {{Name: "x", Run: "a", Ports: []int{70000}}},
		"duplicate name":     {{Name: "x", Run: "a"}, {Name: "x", Image: "b"}},
		"bad env":            {{Name: "x", Run: "a", Env: "../etc"}},
	} {
		if err := create(services); err == nil {
			t.Fatalf("%s was accepted", name)
		}
	}
	updated, err := st.UpdateGitRepository(func() string {
		for _, repository := range st.Snapshot().GitRepositories {
			return repository.ID
		}
		return ""
	}(), domain.UpdateGitRepositoryRequest{Operator: "derek", Name: "renamed", RemoteURL: "https://github.com/derek/renamed.git", DefaultRef: "main", Services: []domain.AppService{{Name: "api", Prepare: []string{" dotnet restore ", ""}, Run: "dotnet run", Ports: []int{5000, 5000}}}})
	if err != nil {
		t.Fatal(err)
	}
	if len(updated.Services) != 1 || len(updated.Services[0].Prepare) != 1 || updated.Services[0].Prepare[0] != "dotnet restore" || len(updated.Services[0].Ports) != 1 {
		t.Fatalf("updated services = %+v", updated.Services)
	}
	// Host entries are a repository setting: "name ip" or "name:ip", kept as name:ip.
	hosted, err := st.UpdateGitRepository(updated.ID, domain.UpdateGitRepositoryRequest{Operator: "derek", Name: updated.Name, RemoteURL: updated.RemoteURL, DefaultRef: "main", ServiceHosts: []string{"SQLServer.easyflor.local 192.168.1.40", "cache:10.0.0.7", ""}})
	if err != nil {
		t.Fatal(err)
	}
	if len(hosted.ServiceHosts) != 2 || hosted.ServiceHosts[0] != "sqlserver.easyflor.local:192.168.1.40" || hosted.ServiceHosts[1] != "cache:10.0.0.7" {
		t.Fatalf("service hosts = %v", hosted.ServiceHosts)
	}
	if _, err := st.UpdateGitRepository(updated.ID, domain.UpdateGitRepositoryRequest{Operator: "derek", Name: updated.Name, RemoteURL: updated.RemoteURL, DefaultRef: "main", ServiceHosts: []string{"db not-an-ip"}}); err == nil {
		t.Fatal("host entry without an IP was accepted")
	}
}

// An expose phase needs no instructions and always waits for a person.
func TestExposePhaseNormalisesAndWaitsForAPerson(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Met test", Phases: []domain.WorkflowPhase{
		{ID: "build", Name: "Bouwen", Instructions: "Bouw het", AllowChanges: true, Accept: domain.WorkflowTransition{Target: "NEXT"}, Reject: domain.WorkflowTransition{Target: "SELF"}},
		{ID: "test", Name: "Testen", Executor: domain.WorkflowExecutorExpose, Model: "ignored", Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: "build"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if template.Phases[1].Executor != domain.WorkflowExecutorExpose || template.Phases[1].Model != "" {
		t.Fatalf("expose phase = %+v", template.Phases[1])
	}
}

// Probing an agent happens as the operator runs it: under their own
// credential layer built on the tool, which is where the login lives.
func TestIdentityLayerForPrefersTheOperatorsCredentialLayer(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	codex := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}}})
	if entry, err := st.IdentityLayerFor(codex.ID, "derek"); err != nil || entry.ID != codex.ID {
		t.Fatalf("without a credential layer = %+v, %v", entry, err)
	}
	login := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactCredential, Name: "codex", Scope: domain.ScopeUser, ParentArtifactIDs: []string{codex.ID}})
	if entry, err := st.IdentityLayerFor(codex.ID, "derek"); err != nil || entry.ID != login.ID {
		t.Fatalf("with a credential layer = %+v, %v", entry, err)
	}
	if entry, err := st.IdentityLayerFor(codex.ID, "john"); err != nil || entry.ID != codex.ID {
		t.Fatalf("another operator borrowed derek's login: %+v, %v", entry, err)
	}
	if agent, ok := st.EnablingLayer(login.ID, "acp"); !ok || agent.ID != codex.ID {
		t.Fatalf("enabling layer of the credential = %+v, %v", agent, ok)
	}

	// An EDIT of the tool: the credential still counts as built on it, and
	// the enabling layer is the newest version.
	edited := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{codex.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}}, ReplacesArtifactID: codex.ID})
	if entry, err := st.IdentityLayerFor(edited.ID, "derek"); err != nil || entry.ID != login.ID {
		t.Fatalf("identity layer after an EDIT = %+v, %v", entry, err)
	}
	if agent, ok := st.EnablingLayer(login.ID, "acp"); !ok || agent.ID != edited.ID {
		t.Fatalf("enabling layer after an EDIT = %+v, want the newest version %s", agent, edited.ID)
	}
}

// The chosen agent settings live on the enabling layer and follow an EDIT.
func TestAgentSettingsFollowAnEdit(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	codex := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}}})
	if _, err := st.SetArtifactAgentSettings(codex.ID, domain.AgentSettings{Mode: " agent-full-access ", Model: "gpt-6-astra"}); err != nil {
		t.Fatal(err)
	}
	edited := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{codex.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}}, ReplacesArtifactID: codex.ID})
	stored, err := st.Artifact(edited.ID)
	if err != nil || stored.AgentSettings == nil || stored.AgentSettings.Mode != "agent-full-access" || stored.AgentSettings.Model != "gpt-6-astra" {
		t.Fatalf("settings after EDIT = %+v, %v", stored.AgentSettings, err)
	}
	if cleared, err := st.SetArtifactAgentSettings(edited.ID, domain.AgentSettings{}); err != nil || cleared.AgentSettings != nil {
		t.Fatalf("clearing settings = %+v, %v", cleared.AgentSettings, err)
	}
}

// A Job lies with its owner until a colleague hands it on; only known,
// active users can hold it.
func TestAssignJobHandsItToAKnownUser(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://github.com/derek/shop.git", DefaultRef: "main"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Code", Phases: []domain.WorkflowPhase{{ID: "dev", Name: "Dev", Instructions: "Bouw", AllowChanges: true, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: "SELF"}}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Objective: "Werkend", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if created.Job.Assignee != "derek" {
		t.Fatalf("new job assignee = %q, want the owner", created.Job.Assignee)
	}
	if _, err := st.AssignJob(created.Job.ID, "derek", "john"); err == nil {
		t.Fatal("assigned to an unknown user")
	}
	st.mu.Lock()
	st.state.Users["usr_john"] = domain.User{ID: "usr_john", Username: "john", DisplayName: "John", Role: domain.UserMember}
	st.mu.Unlock()
	assigned, err := st.AssignJob(created.Job.ID, "derek", "John")
	if err != nil || assigned.Assignee != "john" {
		t.Fatalf("assign = %+v, %v", assigned, err)
	}
	// The running phase finishes as derek; the next one runs as john, with
	// john's environment and Git identity.
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	retry, err := st.CompleteWorkflowPhase(created.Session.ID, "reject", "nog niet af")
	if err != nil || retry.NextSession == nil {
		t.Fatalf("retry = %+v, %v", retry, err)
	}
	if retry.NextSession.Operator != "john" || created.Session.Operator != "derek" {
		t.Fatalf("next session runs as %q, first ran as %q", retry.NextSession.Operator, created.Session.Operator)
	}
	if _, _, err := st.RetryWorkflowSession(retry.NextSession.ID, "john"); err != nil && !errors.Is(err, ErrConflict) {
		t.Fatalf("assignee retry = %v", err)
	}
	back, err := st.AssignJob(created.Job.ID, "john", "derek")
	if err != nil || back.Assignee != "derek" {
		t.Fatalf("assign back = %+v, %v", back, err)
	}
}

// A merge is a step of its own: an agent that accepts on its own moves
// the Job to it without a person choosing anything, and DONE is DONE.
func TestMergeIsAStepAndDoneIsDone(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://github.com/derek/shop.git", DefaultRef: "develop"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Code", Phases: []domain.WorkflowPhase{
		{ID: "dev", Name: "Dev", Instructions: "Bouw", AllowChanges: true, Accept: domain.WorkflowTransition{Target: "NEXT"}, Reject: domain.WorkflowTransition{Target: "SELF"}},
		{ID: "merge", Name: "Merge", Executor: domain.WorkflowExecutorAction, Action: &domain.WorkflowAction{Type: domain.WorkflowActionGitMerge}, Accept: domain.WorkflowTransition{Target: "DONE"}, Reject: domain.WorkflowTransition{Target: "dev"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if len(template.Phases) != 2 || template.Phases[1].Accept.Target != domain.WorkflowTargetDone {
		t.Fatalf("phases = %+v", template.Phases)
	}
	for _, bad := range []string{"DONE:merge", "DONE:pull_request", "spin-pull-request"} {
		if _, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Fout", Phases: []domain.WorkflowPhase{{ID: "dev", Name: "Dev", Instructions: "Bouw", Accept: domain.WorkflowTransition{Target: bad}, Reject: domain.WorkflowTransition{Target: "SELF"}}}}); err == nil {
			t.Fatalf("target %q was accepted", bad)
		}
	}
	if _, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Fout", Phases: []domain.WorkflowPhase{{ID: "x", Name: "X", Executor: domain.WorkflowExecutorAction, Action: &domain.WorkflowAction{Type: "git.fax"}}}}); err == nil {
		t.Fatal("an unknown action was accepted as a step")
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Objective: "Werkend", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	advance, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "klaar")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("accept = %+v, %v", advance, err)
	}
	_, _, _, next, _, _, err := st.WorkflowForSession(advance.NextSession.ID)
	if err != nil || next.ID != "merge" || next.Action == nil || next.Action.Type != domain.WorkflowActionGitMerge {
		t.Fatalf("next phase = %+v, %v", next, err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(advance.NextSession.ID); err != nil {
		t.Fatal(err)
	}
	done, err := st.CompleteWorkflowPhase(advance.NextSession.ID, "accept", "gemerged")
	if err != nil || done.NextSession != nil || done.Question != nil || done.Job.WorkflowStatus != domain.WorkflowDone {
		t.Fatalf("after the merge step = %+v, %v", done, err)
	}
}

// An admin can give a user a new password; the user's sessions end with it,
// and only an admin may do it.
func TestResetUserPasswordEndsTheUsersSessions(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	admin, err := st.CreateInitialUser(domain.User{Username: "derek", DisplayName: "Derek", PasswordHash: "hash-derek"})
	if err != nil {
		t.Fatal(err)
	}
	john, err := st.CreateUser(admin.ID, domain.User{Username: "john", DisplayName: "John", Role: domain.UserMember, PasswordHash: "hash-old"})
	if err != nil {
		t.Fatal(err)
	}
	session, err := st.CreateAuthSession(john.ID, "token-hash", "csrf-hash", time.Now().Add(time.Hour))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.ResetUserPassword(john.ID, john.ID, "hash-new"); err == nil {
		t.Fatal("a member reset a password")
	}
	if _, err := st.ResetUserPassword(admin.ID, john.ID, "hash-new"); err != nil {
		t.Fatal(err)
	}
	if _, _, err := st.AuthenticateSession(session.TokenHash); err == nil {
		t.Fatal("the old session survived the password reset")
	}
	stored, err := st.UserByUsername("john")
	if err != nil || stored.PasswordHash != "hash-new" {
		t.Fatalf("stored user = %+v, %v", stored, err)
	}
}

// A layer recorded with --enable=acp but without --command gets its
// entrypoint afterwards; an EDIT of the layer inherits it.
func TestEnablementCommandCanBeSetAfterwards(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	claude := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "claude", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "acp"}}})
	if _, err := st.SetArtifactEnablementCommand(claude.ID, "git", "x"); err == nil {
		t.Fatal("set a command on a capability the layer does not enable")
	}
	updated, err := st.SetArtifactEnablementCommand(claude.ID, "acp", " claude-code-acp ")
	if err != nil || updated.Enables[0].Command != "claude-code-acp" || updated.Enables[0].Transport != "stdio" {
		t.Fatalf("updated enablement = %+v, %v", updated.Enables, err)
	}
	edited := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "claude", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{claude.ID}, Enables: updated.Enables, ReplacesArtifactID: claude.ID})
	if edited.Enables[0].Command != "claude-code-acp" {
		t.Fatalf("EDIT lost the command: %+v", edited.Enables)
	}
}

func TestJobReferenceNamespacesBranchAndCarriesToForks(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "ref", RemoteURL: "https://example.com/ref.git", DefaultRef: "develop"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Kort", Phases: []domain.WorkflowPhase{{ID: "build", Name: "Bouw", Instructions: "Bouw", AllowChanges: true, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf}}}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.CreateJob(domain.CreateJobRequest{Title: "Slecht", Reference: "EF 12/34", Objective: "x", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID}); !errors.Is(err, ErrConflict) {
		t.Fatalf("invalid reference error = %v", err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Reserveringen", Reference: "#1234", Objective: "x", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil || created.Job.Reference != "#1234" || created.Job.Branch != "jobs/#1234/main" {
		t.Fatalf("job = %+v, error = %v", created.Job, err)
	}
	if _, err := st.CloseJob(created.Job.ID, "derek"); err != nil {
		t.Fatal(err)
	}
	fork, err := st.CreateJob(domain.CreateJobRequest{Title: "Vervolg", Objective: "y", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID, ForkedFromJobID: created.Job.ID})
	if err != nil || fork.Job.Reference != "#1234" || !strings.HasPrefix(fork.Job.Branch, "jobs/#1234/vervolg-") || !strings.HasSuffix(fork.Job.Branch, "/main") {
		t.Fatalf("fork = %+v, error = %v", fork.Job, err)
	}
}

// A Template step that names a Git or tool layer as its environment does
// not lose the agent: the Job's environment (where the model is chosen)
// stays the entry and the step's layer rides along as a WITH layer.
func TestPhaseEnvironmentWithoutAgentKeepsTheJobsAgent(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "claude", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "claude-agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "env", RemoteURL: "https://example.com/env.git", DefaultRef: "develop"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Ontwikkeling", Phases: []domain.WorkflowPhase{{
		ID: "develop", Name: "Ontwikkeling", Instructions: "Bouw", AllowChanges: true, EnvironmentSelector: "tool:git",
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	for _, agent := range []string{"tool:codex", "tool:claude"} {
		created, err := st.CreateJob(domain.CreateJobRequest{Title: "Job op " + agent, Objective: "x", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: agent, TemplateID: template.ID})
		if err != nil {
			t.Fatalf("job with %s: %v", agent, err)
		}
		if created.Session.EnvironmentSelector != agent || !slices.Contains(created.Session.WithSelectors, "tool:git") {
			t.Fatalf("session environment for %s = %q with %v", agent, created.Session.EnvironmentSelector, created.Session.WithSelectors)
		}
	}
}

// A Job's environment can change after creation: the next step starts
// with the new agent layer, a running step keeps what it was started
// with, and only the owner or assignee of an open Job may change it.
func TestJobEnvironmentChangesForTheNextStep(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "codex", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "codex-acp"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "claude", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "claude-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://github.com/derek/shop.git", DefaultRef: "develop"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Code", Phases: []domain.WorkflowPhase{
		{ID: "plan", Name: "Plan", Instructions: "Plan", Accept: domain.WorkflowTransition{Target: "NEXT"}, Reject: domain.WorkflowTransition{Target: "SELF"}},
		{ID: "dev", Name: "Dev", Instructions: "Bouw", AllowChanges: true, Accept: domain.WorkflowTransition{Target: "DONE"}, Reject: domain.WorkflowTransition{Target: "SELF"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Objective: "Werkend", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:codex", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if created.Session.EnvironmentSelector != "tool:codex" {
		t.Fatalf("first step environment = %q", created.Session.EnvironmentSelector)
	}
	if _, err := st.UpdateJobEnvironment(created.Job.ID, "john", domain.UpdateJobEnvironmentRequest{EnvironmentSelector: "tool:claude"}); err == nil {
		t.Fatal("someone else changed the environment")
	}
	if _, err := st.UpdateJobEnvironment(created.Job.ID, "derek", domain.UpdateJobEnvironmentRequest{EnvironmentSelector: "tool:nonsense"}); err == nil {
		t.Fatal("an unknown layer was accepted")
	}
	if _, err := st.UpdateJobEnvironment(created.Job.ID, "derek", domain.UpdateJobEnvironmentRequest{EnvironmentSelector: "tool:claude", MCPServerIDs: []string{"mcp_unknown"}}); err == nil {
		t.Fatal("an unknown MCP connection was accepted")
	}
	job, err := st.UpdateJobEnvironment(created.Job.ID, "derek", domain.UpdateJobEnvironmentRequest{EnvironmentSelector: "tool:claude"})
	if err != nil || job.EnvironmentSelector != "tool:claude" {
		t.Fatalf("update = %+v, %v", job, err)
	}
	// The running step keeps codex; the next step starts with claude.
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	for _, session := range st.Snapshot().Sessions {
		if session.ID == created.Session.ID && session.EnvironmentSelector != "tool:codex" {
			t.Fatalf("running step environment = %q", session.EnvironmentSelector)
		}
	}
	advance, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "plan klaar")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("accept = %+v, %v", advance, err)
	}
	if advance.NextSession.EnvironmentSelector != "tool:claude" {
		t.Fatalf("next step environment = %q", advance.NextSession.EnvironmentSelector)
	}
	if _, err := st.CloseJob(created.Job.ID, "derek"); err == nil {
		if _, err := st.UpdateJobEnvironment(created.Job.ID, "derek", domain.UpdateJobEnvironmentRequest{EnvironmentSelector: "tool:codex"}); err == nil {
			t.Fatal("a closed Job changed its environment")
		}
	}
}

// The kind of a deliverable is its measure: a put that is not what the
// Template asked for is refused, and every kind maps to a path in the
// capsule.
func TestDeliverableKindsAreCheckedOnPut(t *testing.T) {
	folder := &domain.DeliverableBundle{Ref: "bundle:a", Files: 3, Folder: true, Entry: "index.html", ContentType: "text/html; charset=utf-8"}
	emptyFolder := &domain.DeliverableBundle{Ref: "bundle:b", Files: 0, Folder: true}
	pdf := &domain.DeliverableBundle{Ref: "bundle:c", Files: 1, Entry: "bon.pdf", ContentType: "application/pdf"}
	png := &domain.DeliverableBundle{Ref: "bundle:d", Files: 1, Entry: "kassa.png", ContentType: "image/png"}
	other := &domain.DeliverableBundle{Ref: "bundle:e", Files: 1, Entry: "export.xlsx", ContentType: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"}
	cases := []struct {
		kind    string
		content string
		bundle  *domain.DeliverableBundle
		ok      bool
	}{
		{domain.DeliverableKindMarkdown, "# FO", nil, true},
		{domain.DeliverableKindMarkdown, "", nil, false},
		{domain.DeliverableKindMarkdown, "", folder, false},
		{domain.DeliverableKindFolder, "", folder, true},
		{domain.DeliverableKindFolder, "", emptyFolder, false},
		{domain.DeliverableKindFolder, "", pdf, false},
		{domain.DeliverableKindPDF, "", pdf, true},
		{domain.DeliverableKindPDF, "", png, false},
		{domain.DeliverableKindPDF, "", folder, false},
		{domain.DeliverableKindImage, "", png, true},
		{domain.DeliverableKindImage, "", pdf, false},
		{domain.DeliverableKindFile, "", other, true},
		{domain.DeliverableKindFile, "", pdf, true},
		{domain.DeliverableKindFile, "", folder, false},
	}
	for _, tc := range cases {
		err := checkDeliverableShape(domain.DeliverableDefinition{Name: "X", Kind: tc.kind}, tc.content, tc.bundle)
		if (err == nil) != tc.ok {
			t.Fatalf("kind %s with %+v: ok=%v, err=%v", tc.kind, tc.bundle, tc.ok, err)
		}
	}
	paths := map[string]domain.Deliverable{
		"/root/deliverables/functioneel-ontwerp.md": {Name: "Functioneel ontwerp", Kind: domain.DeliverableKindMarkdown},
		"/root/deliverables/website":                {Name: "Website", Kind: domain.DeliverableKindFolder, Bundle: folder},
		"/root/deliverables/bon.pdf":                {Name: "Bon", Kind: domain.DeliverableKindPDF, Bundle: pdf},
		"/root/deliverables/kassa-scherm.png":       {Name: "Kassa scherm!", Kind: domain.DeliverableKindImage, Bundle: png},
		"/root/deliverables/export.xlsx":            {Name: "Export", Kind: domain.DeliverableKindFile, Bundle: other},
	}
	for want, deliverable := range paths {
		if got := deliverable.CapsulePath(); got != want {
			t.Fatalf("%s lands at %s, want %s", deliverable.Name, got, want)
		}
	}
}

// A Job may start with a brainstorm: a chat in the Job's environment, with
// the goal still open. start_process sets the goal and queues the
// Template's first step; nothing else about the Job changes.
func TestBrainstormSetsTheGoalAndStartsTheTemplate(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://github.com/derek/shop.git", DefaultRef: "develop"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Code", Phases: []domain.WorkflowPhase{
		{ID: "dev", Name: "Ontwikkel", Instructions: "Bouw", AllowChanges: true, Accept: domain.WorkflowTransition{Target: "DONE"}, Reject: domain.WorkflowTransition{Target: "SELF"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID}); err == nil {
		t.Fatal("a Job without a goal and without a brainstorm was accepted")
	}
	if _, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Brainstorm: true, Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent"}); err == nil {
		t.Fatal("a brainstorm without a Template was accepted")
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Brainstorm: true, Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if created.Job.Objective != "" || created.Session.Role != "Brainstorm" || created.Session.Executor != domain.WorkflowExecutorAgent {
		t.Fatalf("brainstorm Job = %+v session = %+v", created.Job, created.Session)
	}
	_, _, run, phase, _, _, err := st.WorkflowForSession(created.Session.ID)
	if err != nil || run.PhaseID != domain.BrainstormPhaseID || run.Status != domain.PhaseRunQueued || phase.Name != "Brainstorm" || phase.AllowChanges {
		t.Fatalf("brainstorm run = %+v phase = %+v, %v", run, phase, err)
	}
	if _, _, err := st.StartProcess(created.Session.ID, "Een goal"); err == nil {
		t.Fatal("a queued brainstorm started the process")
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	if _, _, err := st.StartProcess(created.Session.ID, "  "); err == nil {
		t.Fatal("an empty goal started the process")
	}
	started, _, err := st.StartProcess(created.Session.ID, "# Darkmode\n\nEén werkende switch.")
	if err != nil || started.Job.Objective != "# Darkmode\n\nEén werkende switch." || started.Session.ID == created.Session.ID {
		t.Fatalf("start = %+v, %v", started, err)
	}
	if _, _, next, nextPhase, _, _, err := st.WorkflowForSession(started.Session.ID); err != nil || nextPhase.ID != "dev" || next.Status != domain.PhaseRunQueued || next.Attempt != 1 {
		t.Fatalf("next step = %+v %+v, %v", next, nextPhase, err)
	}
	snapshot := st.Snapshot()
	for _, candidate := range snapshot.PhaseRuns {
		if candidate.ID == run.ID && (candidate.Status != domain.PhaseRunAccepted || candidate.Summary != started.Job.Objective) {
			t.Fatalf("brainstorm run after start = %+v", candidate)
		}
	}
	if snapshot.Jobs[0].CurrentPhaseRunID == run.ID || snapshot.Jobs[0].WorkflowStatus != domain.WorkflowBusy {
		t.Fatalf("job after start = %+v", snapshot.Jobs[0])
	}
	if _, _, err := st.StartProcess(created.Session.ID, "Nog een goal"); err == nil {
		t.Fatal("a finished brainstorm started the process again")
	}
	// A Job shot straight at a goal has no brainstorm.
	direct, err := st.CreateJob(domain.CreateJobRequest{Title: "Direct", Objective: "Klaar", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil || direct.Session.Role != "Ontwikkel" {
		t.Fatalf("direct Job session = %+v, %v", direct.Session, err)
	}
}
