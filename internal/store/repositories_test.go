package store

import (
	"errors"
	"testing"

	"easyacp/internal/domain"
)

// A Job with several repositories checks each out in its own folder: the
// ones it changes on the Job branch, a reference one at its base, read
// only; the first changed one is the Job's main repository. A Job with one
// repository keeps it at the workspace root, and a Job that changes none
// of its repositories is refused.
func TestJobWithSeveralRepositoriesChecksOutEachInItsOwnFolder(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	shop, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "Shop", RemoteURL: "https://example.com/shop.git", DefaultRef: "main"})
	if err != nil {
		t.Fatal(err)
	}
	backoffice, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "Backoffice", RemoteURL: "https://example.com/backoffice.git", DefaultRef: "develop"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Flow", Phases: []domain.WorkflowPhase{
		{ID: "build", Name: "Build", Instructions: "Bouw", Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	// Reference first on purpose: the changed repository still leads.
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Beide", Objective: "Doe het in beide", Operator: "derek", EnvironmentSelector: "tool:agent", TemplateID: template.ID, Repositories: []domain.JobRepositoryRequest{
		{RepositoryID: backoffice.Repository.ID, Mode: domain.RepositoryModeReference},
		{RepositoryID: shop.Repository.ID, Mode: domain.RepositoryModeChange, BaseRef: "release"},
	}})
	if err != nil {
		t.Fatal(err)
	}
	job := created.Job
	if job.GitRepositoryID != shop.Repository.ID || job.BaseRef != "release" || len(job.Repositories) != 2 {
		t.Fatalf("job = repo %s base %s repositories %+v", job.GitRepositoryID, job.BaseRef, job.Repositories)
	}
	if job.Repositories[0].Path != "shop" || job.Repositories[0].Mode != domain.RepositoryModeChange || job.Repositories[1].Path != "backoffice" || job.Repositories[1].Mode != domain.RepositoryModeReference || job.Repositories[1].BaseRef != "develop" {
		t.Fatalf("repositories = %+v", job.Repositories)
	}
	composition, err := st.Use(domain.UseRequest{Operator: "derek", SessionID: created.Session.ID})
	if err != nil {
		t.Fatal(err)
	}
	if composition.Git == nil || composition.Git.Path != "shop" || composition.Git.TargetRef != job.Branch || composition.Git.BootstrapRef != "release" || composition.Git.Directory() != "/workspace/shop" {
		t.Fatalf("main workspace = %+v", composition.Git)
	}
	if len(composition.Workspaces) != 2 || composition.Workspaces[1].Mode != domain.RepositoryModeReference || composition.Workspaces[1].BaseRef != "develop" || composition.Workspaces[1].HeadRef != "" || composition.Workspaces[1].Directory() != "/workspace/backoffice" {
		t.Fatalf("workspaces = %+v", composition.Workspaces)
	}
	if changed := composition.ChangedWorkspaces(); len(changed) != 1 || changed[0].RepositoryID != shop.Repository.ID {
		t.Fatalf("changed workspaces = %+v", changed)
	}
	if _, err := st.DeleteGitRepository(backoffice.Repository.ID, "derek"); !errors.Is(err, ErrConflict) {
		t.Fatalf("a reference repository of a Job was removed: %v", err)
	}
	if _, err := st.CreateJob(domain.CreateJobRequest{Title: "Alleen lezen", Objective: "x", Operator: "derek", EnvironmentSelector: "tool:agent", TemplateID: template.ID, Repositories: []domain.JobRepositoryRequest{{RepositoryID: shop.Repository.ID, Mode: domain.RepositoryModeReference}}}); !errors.Is(err, ErrConflict) {
		t.Fatalf("a Job that changes nothing was accepted: %v", err)
	}
	single, err := st.CreateJob(domain.CreateJobRequest{Title: "Eén", Objective: "x", Operator: "derek", GitRepositoryID: shop.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if single.Job.Repositories[0].Path != "" || len(single.Job.JobRepositories()) != 1 {
		t.Fatalf("a Job with one repository = %+v", single.Job.Repositories)
	}
	if composition, err := st.Use(domain.UseRequest{Operator: "derek", SessionID: single.Session.ID}); err != nil || composition.Git == nil || composition.Git.Path != "" || len(composition.Workspaces) != 0 {
		t.Fatalf("one repository at the root: %+v %v", composition.Git, err)
	}
}
