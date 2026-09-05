package store

import (
	"errors"
	"testing"

	"easyacp/internal/domain"
)

// The agent asks a whole form at once; the operator answers it in one go and
// the same attempt carries on. Nothing about the phase outcome is decided.
func TestAskWorkflowQuestionsFormIsAnsweredAsAWholeAndResumesTheRun(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "spin", RemoteURL: "https://example.com/spin.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Build", Phases: []domain.WorkflowPhase{{
		ID: "build", Name: "Build", Instructions: "Bouw",
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Transcripties", Objective: "Notulen", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	session := created.Session.ID
	if _, err := st.MarkWorkflowPhaseRunning(session); err != nil {
		t.Fatal(err)
	}

	if _, err := st.AskWorkflowQuestions(session, nil); !errors.Is(err, ErrConflict) {
		t.Fatalf("empty form error = %v", err)
	}
	if _, err := st.AskWorkflowQuestions(session, []domain.WorkflowQuestionItem{{Question: "  "}}); !errors.Is(err, ErrConflict) {
		t.Fatalf("blank question error = %v", err)
	}
	asked, err := st.AskWorkflowQuestions(session, []domain.WorkflowQuestionItem{
		{Question: "Waar komt de AssemblyAI-sleutel vandaan?", Options: []string{"appsettings", "environment variable", " appsettings "}},
		{Question: "Hoe heet de nieuwe knop?"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(asked.Items) != 2 || asked.Items[0].ID != "q1" || asked.Items[1].ID != "q2" || len(asked.Items[0].Options) != 2 {
		t.Fatalf("asked items = %+v", asked.Items)
	}
	if run := phaseRunByID(t, st, asked.PhaseRunID); run.Status != domain.PhaseRunPending || run.PendingReason != "ask" {
		t.Fatalf("run after ask = %+v", run)
	}

	// Every question needs an answer before the form counts as answered.
	if _, err := st.AnswerWorkflowQuestions(asked.ID, "derek", []domain.WorkflowQuestionAnswer{{ItemID: "q1", Answer: "appsettings"}}); !errors.Is(err, ErrConflict) {
		t.Fatalf("partial answer error = %v", err)
	}
	answered, err := st.AnswerWorkflowQuestions(asked.ID, "derek", []domain.WorkflowQuestionAnswer{
		{ItemID: "q1", Answer: "appsettings"},
		{ItemID: "q2", Answer: "+ Nieuwe transcriptie"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if answered.Status != "answered" || answered.Answer != "answered" || answered.AnsweredBy != "derek" {
		t.Fatalf("answered question = %+v", answered)
	}
	if answered.Items[0].Answer != "appsettings" || answered.Items[0].Other {
		t.Fatalf("chosen option recorded as %+v", answered.Items[0])
	}
	if answered.Items[1].Answer != "+ Nieuwe transcriptie" || !answered.Items[1].Other {
		t.Fatalf("own words recorded as %+v", answered.Items[1])
	}
	if run := phaseRunByID(t, st, asked.PhaseRunID); run.Status != domain.PhaseRunRunning || run.PendingReason != "" || run.CompletedAt != nil {
		t.Fatalf("run after answers = %+v", run)
	}
	if job := st.Snapshot().Jobs[0]; job.WorkflowStatus != domain.WorkflowBusy || job.PendingReason != "" {
		t.Fatalf("job after answers = %+v", job)
	}
	if _, err := st.AnswerWorkflowQuestions(asked.ID, "derek", nil); !errors.Is(err, ErrNotFound) {
		t.Fatalf("answering twice error = %v", err)
	}
}

func phaseRunByID(t *testing.T, st *Store, id string) domain.PhaseRun {
	t.Helper()
	for _, run := range st.Snapshot().PhaseRuns {
		if run.ID == id {
			return run
		}
	}
	t.Fatalf("phase run %s not found", id)
	return domain.PhaseRun{}
}
