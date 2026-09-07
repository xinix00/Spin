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
