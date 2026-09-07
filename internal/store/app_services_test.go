package store

import (
	"strings"
	"testing"

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
	back, err := st.AssignJob(created.Job.ID, "john", "derek")
	if err != nil || back.Assignee != "derek" {
		t.Fatalf("assign back = %+v, %v", back, err)
	}
}

// The final accept may choose how the Job lands; the Job's frozen Template
// copy then carries that finalizer.
func TestSetJobFinalizeSwapsTheFinalizerOfTheJob(t *testing.T) {
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
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Code", Phases: []domain.WorkflowPhase{{ID: "dev", Name: "Dev", Instructions: "Bouw", AllowChanges: true, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone, AskUser: true}, Reject: domain.WorkflowTransition{Target: "SELF"}}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Objective: "Werkend", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	job, err := st.SetJobFinalize(created.Job.ID, "merge")
	if err != nil {
		t.Fatal(err)
	}
	last := job.TemplateSnapshot.Phases[len(job.TemplateSnapshot.Phases)-1]
	if job.Finalize != "merge" || last.Action == nil || last.Action.Type != domain.WorkflowActionGitMerge || last.Name != "Mergen" {
		t.Fatalf("finalizer after choice = %+v (job finalize %q)", last, job.Finalize)
	}
	if _, err := st.SetJobFinalize(created.Job.ID, "fax"); err == nil {
		t.Fatal("an unknown landing was accepted")
	}
	// The Template itself is untouched.
	for _, stored := range st.Snapshot().WorkflowTemplates {
		if stored.ID == template.ID && stored.Finalize == "merge" {
			t.Fatal("choosing for one Job changed the Template")
		}
	}
}

// A step whose accept ends the Job says how it lands, so an agent that
// accepts on its own does not need a person to choose.
func TestStepTargetCarriesTheLanding(t *testing.T) {
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
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Code", Phases: []domain.WorkflowPhase{{ID: "dev", Name: "Dev", Instructions: "Bouw", AllowChanges: true, Accept: domain.WorkflowTransition{Target: "DONE:merge"}, Reject: domain.WorkflowTransition{Target: "SELF"}}}})
	if err != nil {
		t.Fatal(err)
	}
	if dev := template.Phases[0]; dev.Accept.Target != domain.WorkflowPullRequestPhaseID || dev.Accept.Landing != "merge" {
		t.Fatalf("accept transition = %+v", dev.Accept)
	}
	if _, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Fout", Phases: []domain.WorkflowPhase{{ID: "dev", Name: "Dev", Instructions: "Bouw", Accept: domain.WorkflowTransition{Target: "DONE:fax"}, Reject: domain.WorkflowTransition{Target: "SELF"}}}}); err == nil {
		t.Fatal("an unknown landing was accepted")
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Objective: "Werkend", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	// The agent accepts by itself: the next phase is the merge, not a PR.
	advance, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "klaar")
	if err != nil {
		t.Fatal(err)
	}
	if advance.NextSession == nil {
		t.Fatalf("no finalizer session: %+v", advance)
	}
	_, _, _, next, _, _, err := st.WorkflowForSession(advance.NextSession.ID)
	if err != nil || next.Action == nil || next.Action.Type != domain.WorkflowActionGitMerge {
		t.Fatalf("finalizer phase = %+v, %v", next, err)
	}
}
