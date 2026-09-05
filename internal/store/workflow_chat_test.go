package store

import (
	"errors"
	"testing"

	"easyacp/internal/domain"
)

// A chat is a way to ask the agent something, not a verdict on the phase. The
// standing decision stays open; the buttons close only while the agent works.
func TestChatKeepsThePendingDecisionOpenUntilTheAgentDecidesAgain(t *testing.T) {
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
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Review", Phases: []domain.WorkflowPhase{{
		ID: "review", Name: "Review", Instructions: "Beoordeel",
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone, AskUser: true},
		Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf, AskUser: true},
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
	first, err := st.CompleteWorkflowPhase(session, "reject", "Sleutel staat in de broncode")
	if err != nil || first.Question == nil {
		t.Fatalf("agent reject = %+v, error = %v", first, err)
	}
	gate := first.Question.ID

	// The operator asks the agent something instead of deciding.
	if resumed, err := st.ResumeWorkflowPhaseForChat(session, "derek"); err != nil || !resumed {
		t.Fatalf("chat resume: resumed=%t error=%v", resumed, err)
	}
	if question := questionByID(t, st, gate); question.Status != "open" || question.Answer != "" {
		t.Fatalf("chat closed the decision: %+v", question)
	}
	if run := phaseRunByID(t, st, first.PhaseRun.ID); run.Status != domain.PhaseRunRunning {
		t.Fatalf("run during chat = %+v", run)
	}
	if job := st.Snapshot().Jobs[0]; job.WorkflowStatus != domain.WorkflowBusy {
		t.Fatalf("job during chat = %+v", job)
	}
	// While the agent works, deciding would integrate a half-edited workspace.
	if _, err := st.AnswerWorkflowQuestion(gate, "derek", "accept", ""); !errors.Is(err, ErrConflict) {
		t.Fatalf("accept during the agent's turn error = %v", err)
	}

	// The agent only answers and ends its turn: the same decision is back.
	if settled, err := st.SettleWorkflowChatTurn(session); err != nil || !settled {
		t.Fatalf("settle after chat turn: settled=%t error=%v", settled, err)
	}
	if run := phaseRunByID(t, st, first.PhaseRun.ID); run.Status != domain.PhaseRunPending || run.PendingReason != "user" || run.PendingOutcome != "reject" {
		t.Fatalf("run after settle = %+v", run)
	}
	if job := st.Snapshot().Jobs[0]; job.WorkflowStatus != domain.WorkflowPending || job.PendingReason != "user" {
		t.Fatalf("job after settle = %+v", job)
	}
	if settled, err := st.SettleWorkflowChatTurn(session); err != nil || settled {
		t.Fatalf("settling twice: settled=%t error=%v", settled, err)
	}

	// A second chat in which the agent changes its mind replaces the decision.
	if _, err := st.ResumeWorkflowPhaseForChat(session, "derek"); err != nil {
		t.Fatal(err)
	}
	second, err := st.CompleteWorkflowPhase(session, "accept", "Sleutel komt nu uit configuratie")
	if err != nil || second.Question == nil || second.Question.ID == gate {
		t.Fatalf("agent accept during chat = %+v, error = %v", second, err)
	}
	if question := questionByID(t, st, gate); question.Status != "superseded" || question.Answer != "" {
		t.Fatalf("old decision after a new one = %+v", question)
	}
	if run := phaseRunByID(t, st, first.PhaseRun.ID); run.Status != domain.PhaseRunPending || run.PendingOutcome != "accept" {
		t.Fatalf("run after new decision = %+v", run)
	}
	if settled, err := st.SettleWorkflowChatTurn(session); err != nil || settled {
		t.Fatalf("settle after a new decision: settled=%t error=%v", settled, err)
	}
	// And the operator can act on the new decision.
	if _, err := st.AnswerWorkflowQuestion(second.Question.ID, "derek", "accept", ""); err != nil {
		t.Fatalf("accept the new decision: %v", err)
	}
	if open := openQuestions(st); len(open) != 0 {
		t.Fatalf("open questions after accept = %+v", open)
	}
}

// State written by an older build: the chat closed the gate as answered and
// left the run running. When the agent's next turn ends, the standing outcome
// comes back as a fresh decision instead of leaving the phase without buttons.
func TestChatTurnRestoresADecisionAnOlderBuildClosedAsChat(t *testing.T) {
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
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Review", Phases: []domain.WorkflowPhase{{
		ID: "review", Name: "Review", Instructions: "Beoordeel",
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone, AskUser: true},
		Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf, AskUser: true},
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
	accepted, err := st.CompleteWorkflowPhase(session, "accept", "Goal behaald")
	if err != nil || accepted.Question == nil {
		t.Fatalf("agent accept = %+v, error = %v", accepted, err)
	}

	// What the previous build wrote when the operator opened the chat.
	st.mu.Lock()
	legacy := st.state.WorkflowQuestions[accepted.Question.ID]
	legacy.Status, legacy.Answer, legacy.AnsweredBy = "answered", "chat", "derek"
	st.state.WorkflowQuestions[legacy.ID] = legacy
	run := st.state.PhaseRuns[accepted.PhaseRun.ID]
	run.Status, run.PendingReason, run.PendingOutcome = domain.PhaseRunRunning, "", ""
	st.state.PhaseRuns[run.ID] = run
	job := st.state.Jobs[run.JobID]
	job.WorkflowStatus, job.PendingReason = domain.WorkflowBusy, ""
	st.state.Jobs[job.ID] = job
	st.mu.Unlock()
	if open := openQuestions(st); len(open) != 0 {
		t.Fatalf("legacy state still has an open question: %+v", open)
	}

	// The server repairs this on start, before any agent could be working.
	restored, err := st.RepairStandingDecisions()
	if err != nil || restored != 1 {
		t.Fatalf("repair on start: restored=%d error=%v", restored, err)
	}
	if again, err := st.RepairStandingDecisions(); err != nil || again != 0 {
		t.Fatalf("repair is not idempotent: restored=%d error=%v", again, err)
	}
	// A turn ending on the same run changes nothing further.
	if settled, err := st.SettleWorkflowChatTurn(session); err != nil || settled {
		t.Fatalf("settle after repair: settled=%t error=%v", settled, err)
	}
	open := openQuestions(st)
	if len(open) != 1 || open[0].Kind != "approval" || open[0].Outcome != "accept" || open[0].ID == legacy.ID {
		t.Fatalf("restored decision = %+v", open)
	}
	if run := phaseRunByID(t, st, accepted.PhaseRun.ID); run.Status != domain.PhaseRunPending || run.PendingOutcome != "accept" {
		t.Fatalf("run after restore = %+v", run)
	}
	if job := st.Snapshot().Jobs[0]; job.WorkflowStatus != domain.WorkflowPending {
		t.Fatalf("job after restore = %+v", job)
	}
	// And the operator can act on it.
	if _, err := st.AnswerWorkflowQuestion(open[0].ID, "derek", "accept", ""); err != nil {
		t.Fatalf("accept restored decision: %v", err)
	}
}

func questionByID(t *testing.T, st *Store, id string) domain.WorkflowQuestion {
	t.Helper()
	for _, question := range st.Snapshot().WorkflowQuestions {
		if question.ID == id {
			return question
		}
	}
	t.Fatalf("question %s not found", id)
	return domain.WorkflowQuestion{}
}

func openQuestions(st *Store) []domain.WorkflowQuestion {
	open := []domain.WorkflowQuestion{}
	for _, question := range st.Snapshot().WorkflowQuestions {
		if question.Status == "open" {
			open = append(open, question)
		}
	}
	return open
}
