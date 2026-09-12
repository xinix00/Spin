package server

import (
	"strings"
	"testing"

	"easyacp/internal/domain"
)

// The prompt names every repository of a Job with several, its folder and
// what the agent may do in it.
func TestGitSectionNamesEveryRepository(t *testing.T) {
	job := domain.Job{Branch: "jobs/x/main", BaseRef: "main", Repositories: []domain.JobRepository{
		{Name: "Shop", Path: "shop", BaseRef: "main", Mode: domain.RepositoryModeChange},
		{Name: "Backoffice", Path: "backoffice", BaseRef: "develop", Mode: domain.RepositoryModeReference},
	}}
	section := workflowGitSection(job, domain.Session{GitRef: "jobs/x/sessions/ses_1"}, nil)
	for _, want := range []string{"/workspace/shop · Shop · AANPASSEN", "/workspace/backoffice · Backoffice · ALLEEN TER REFERENTIE op branch develop", "Job-branch: jobs/x/main"} {
		if !strings.Contains(section, want) {
			t.Fatalf("section lacks %q:\n%s", want, section)
		}
	}
	if strings.Contains(workflowGitSection(domain.Job{Branch: "jobs/y/main", BaseRef: "main", GitRepositoryID: "r1"}, domain.Session{GitRef: "s"}, nil), "meer repositories") {
		t.Fatal("a Job with one repository lists repositories")
	}
}
