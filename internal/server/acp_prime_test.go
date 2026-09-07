package server

import (
	"io"
	"log/slog"
	"strings"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// An agent session is not durable. The first message into a fresh one for a
// workflow phase carries the phase prompt in front of it; the launch, which
// sends that prompt itself, and every later message do not repeat it.
func TestFreshAgentSessionGetsThePhasePromptBeforeAnAnswer(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &acpTestEngine{}, ServerOptions{DisableAuthentication: true})
	for _, line := range []string{
		"RECORD tool:git --scope=global --enable=git", "install git", "END RECORD",
		"RECORD tool:agent --scope=global --from=tool:git --enable=acp --command=agent-acp", "install agent", "END RECORD",
	} {
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://github.com/derek/shop.git", DefaultRef: "main", CredentialScope: domain.CredentialScopePublic})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Code", Phases: []domain.WorkflowPhase{{
		ID: "develop", Name: "Ontwikkelen", Instructions: "Bouw het zonder zelf te committen", AllowChanges: true,
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Feature", Objective: "Werkend", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}

	fresh := &activeACP{sessionID: created.Session.ID}
	primed, err := srv.primedPrompt(fresh, "De gebruiker heeft je vragen beantwoord: ja.")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(primed, "Bouw het zonder zelf te committen") || !strings.HasSuffix(primed, "De gebruiker heeft je vragen beantwoord: ja.") || !strings.Contains(primed, "opnieuw gestart") {
		t.Fatalf("resumed prompt = %q", primed)
	}
	// The agent is told which branches exist and what each one means.
	job := st.Snapshot().Jobs[0]
	if !strings.Contains(primed, "Job-branch: "+job.Branch) || !strings.Contains(primed, "Jouw branch: "+created.Session.GitRef) || !strings.Contains(primed, "Basisbranch: main") {
		t.Fatalf("prompt lacks the Git section: %q", primed)
	}
	if again, _ := srv.primedPrompt(fresh, "en nog iets"); again != "en nog iets" {
		t.Fatalf("second message was primed again: %q", again)
	}

	launched := &activeACP{sessionID: created.Session.ID}
	launched.markPrimed()
	if text, _ := srv.primedPrompt(launched, "volledige fase-prompt"); text != "volledige fase-prompt" {
		t.Fatalf("launch prompt was wrapped: %q", text)
	}
	plain := &activeACP{sessionID: ""}
	if text, _ := srv.primedPrompt(plain, "hoi"); text != "hoi" {
		t.Fatalf("non-workflow prompt was wrapped: %q", text)
	}
}
