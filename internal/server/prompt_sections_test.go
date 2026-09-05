package server

import (
	"io"
	"log/slog"
	"strings"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// One attempt can be rejected several times, because a chat resumes that same
// attempt and the reviewer decides again. Every one of those rejections is
// feedback the next attempt needs, and none of them is a decision anyone made.
func TestPromptSeparatesReviewFeedbackFromRecordedDecisions(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true})
	for _, line := range []string{
		"RECORD tool:git --scope=global --enable=git", "install git", "END RECORD",
		"RECORD tool:agent --scope=global --from=tool:git --enable=acp --command=agent-acp", "install agent", "END RECORD",
	} {
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "spin", RemoteURL: "https://example.com/spin.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Review loop", Phases: []domain.WorkflowPhase{
		{ID: "build", Name: "Build", Instructions: "Bouw de transcriptie",
			Accept: domain.WorkflowTransition{Target: "review"},
			Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf}},
		{ID: "review", Name: "Review", Instructions: "Beoordeel de wijziging",
			Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone},
			Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf, AskUser: true}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Transcripties", Objective: "Notulen omzetten", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	session := created.Session.ID
	if _, err := st.MarkWorkflowPhaseRunning(session); err != nil {
		t.Fatal(err)
	}

	// A form asked during Build: every answered question is a recorded decision.
	form, err := st.AskWorkflowQuestions(session, []domain.WorkflowQuestionItem{
		{Question: "Waar komt de sleutel vandaan?", Options: []string{"appsettings", "environment"}},
		{Question: "Hoe heet de knop?"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.AnswerWorkflowQuestions(form.ID, "derek", []domain.WorkflowQuestionAnswer{{ItemID: "q1", Answer: "environment"}, {ItemID: "q2", Answer: "+ Nieuwe transcriptie"}}); err != nil {
		t.Fatal(err)
	}
	// The agent asks the operator something during Build. That is the only real
	// decision anyone made, and answering it carries the Job into Review.
	question, err := st.AskWorkflowQuestion(session, "Mag de sleutel uit de broncode?")
	if err != nil {
		t.Fatal(err)
	}
	toReview, err := st.AnswerWorkflowQuestion(question.ID, "derek", "accept", "Ja, via configuratie")
	if err != nil || toReview.NextSession == nil {
		t.Fatalf("answer advance = %+v, error = %v", toReview, err)
	}
	session = toReview.NextSession.ID
	if _, err := st.MarkWorkflowPhaseRunning(session); err != nil {
		t.Fatal(err)
	}

	// Three rejections on this one attempt: after each gate the operator chats,
	// which resumes the same run and lets the reviewer decide again.
	rejections := []string{"Hardgecodeerde sleutel", "Status niet gecontroleerd", "Upload is synchroon"}
	for index, reason := range rejections {
		if _, err := st.CompleteWorkflowPhase(session, "reject", reason); err != nil {
			t.Fatalf("reject %d: %v", index+1, err)
		}
		if index == len(rejections)-1 {
			break
		}
		if resumed, err := st.ResumeWorkflowPhaseForChat(session, "derek"); err != nil || !resumed {
			t.Fatalf("chat resume %d: resumed=%t error=%v", index+1, resumed, err)
		}
	}
	gate := ""
	for _, candidate := range st.Snapshot().WorkflowQuestions {
		if candidate.Status == "open" {
			gate = candidate.ID
		}
	}
	if gate == "" {
		t.Fatal("no open gate after the last rejection")
	}
	advance, err := st.AnswerWorkflowQuestion(gate, "derek", "reject", "Pak eerst de sleutel aan")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("user reject advance = %+v, error = %v", advance, err)
	}

	prompt, err := srv.workflowPrompt(advance.NextSession.ID)
	if err != nil {
		t.Fatal(err)
	}
	feedback := section(t, prompt, "GEGEVEN FEEDBACK")
	decisions := section(t, prompt, "VASTGELEGDE BESLUITEN")

	for _, reason := range rejections {
		if strings.Count(feedback, reason) != 1 {
			t.Fatalf("feedback carries %q %d times:\n%s", reason, strings.Count(feedback, reason), feedback)
		}
		if strings.Contains(decisions, reason) {
			t.Fatalf("reviewer prose leaked into the decisions:\n%s", decisions)
		}
	}
	if !strings.Contains(feedback, "afwijzing door de gebruiker: Pak eerst de sleutel aan") {
		t.Fatalf("feedback misses the operator's own rejection:\n%s", feedback)
	}
	if strings.Contains(decisions, "AI rejected") {
		t.Fatalf("a generated gate was recorded as a decision:\n%s", decisions)
	}
	if !strings.Contains(decisions, "Mag de sleutel uit de broncode?") || !strings.Contains(decisions, "Ja, via configuratie") {
		t.Fatalf("the asked question is missing from the decisions:\n%s", decisions)
	}
	for _, expected := range []string{"- Waar komt de sleutel vandaan? → environment", "- Hoe heet de knop? → + Nieuwe transcriptie"} {
		if !strings.Contains(decisions, expected) {
			t.Fatalf("decisions miss %q:\n%s", expected, decisions)
		}
	}
	// Nothing the reviewer said may appear twice anywhere in the prompt.
	for _, reason := range rejections {
		if count := strings.Count(prompt, reason); count != 1 {
			t.Fatalf("%q appears %d times in the whole prompt:\n%s", reason, count, prompt)
		}
	}
}

func section(t *testing.T, prompt, header string) string {
	t.Helper()
	start := strings.Index(prompt, header)
	if start < 0 {
		t.Fatalf("prompt has no %s section:\n%s", header, prompt)
	}
	rest := prompt[start+len(header):]
	for _, next := range []string{"\nGEGEVEN FEEDBACK", "\nVASTGELEGDE BESLUITEN", "\nCODECOMMENTS", "\nWERKWIJZE", "\nOP TE LEVEREN"} {
		if end := strings.Index(rest, next); end >= 0 {
			rest = rest[:end]
		}
	}
	return rest
}
