package store

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"easyacp/internal/domain"
)

func TestLegacyAllowCommitMigratesToAllowChanges(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	legacy := `{"workflow_templates":{"tpl_legacy":{"id":"tpl_legacy","name":"Legacy","created_by":"derek","phases":[{"id":"design","name":"Design","instructions":"Design","deliverables":[{"name":"FO","required":true}],"accept":{"target":"develop"},"reject":{"target":"SELF"}},{"id":"develop","name":"Develop","instructions":"Build","allow_commit":true,"ask_user":true,"accept":{"target":"DONE"},"reject":{"target":"SELF"}}]}}}`
	if err := os.WriteFile(path, []byte(legacy), 0o600); err != nil {
		t.Fatal(err)
	}
	st, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	phases := st.Snapshot().WorkflowTemplates[0].Phases
	phase := phases[1]
	if !phase.AllowChanges || phase.AllowCommit {
		t.Fatalf("migrated phase = %+v", phase)
	}
	if phase.AskUser || !phase.Accept.AskUser || !phase.Reject.AskUser || len(phase.Inject) != 1 || phase.Inject[0] != "FO" {
		t.Fatalf("legacy gates/injection = %+v", phase)
	}
	persisted, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(persisted), `"allow_commit"`) || !strings.Contains(string(persisted), `"allow_changes": true`) {
		t.Fatalf("persisted migration = %s", persisted)
	}
}

func TestCompletedLegacyWorkflowIsBackfilledWithMandatoryPullRequest(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	legacy := `{
  "workflow_templates":{"tpl_old":{"id":"tpl_old","name":"Old flow","created_by":"derek","phases":[{"id":"review","name":"Review","instructions":"Review","accept":{"target":"DONE"},"reject":{"target":"SELF"}}]}},
  "jobs":{"job_old":{"id":"job_old","title":"Existing result","objective":"Publish it","owner":"derek","git_repository_id":"git_old","base_ref":"main","branch":"jobs/existing/main","template_id":"tpl_old","status":"done","workflow_status":"done","session_ids":["ses_old"],"phase_run_ids":["run_old"]}},
  "sessions":{"ses_old":{"id":"ses_old","job_id":"job_old","phase_run_id":"run_old","operator":"derek","status":"done"}},
  "phase_runs":{"run_old":{"id":"run_old","job_id":"job_old","template_id":"tpl_old","phase_id":"review","phase_name":"Review","session_id":"ses_old","status":"accepted"}}
}`
	if err := os.WriteFile(path, []byte(legacy), 0o600); err != nil {
		t.Fatal(err)
	}
	st, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	snapshot := st.Snapshot()
	if len(snapshot.Jobs) != 1 || snapshot.Jobs[0].WorkflowStatus != domain.WorkflowBusy || snapshot.Jobs[0].Status != domain.JobActive || snapshot.Jobs[0].CurrentPhaseRunID == "" {
		t.Fatalf("backfilled Job = %+v", snapshot.Jobs)
	}
	if len(snapshot.PhaseRuns) != 2 || len(snapshot.Sessions) != 2 {
		t.Fatalf("backfilled runs/sessions = %+v / %+v", snapshot.PhaseRuns, snapshot.Sessions)
	}
	var finalSession domain.Session
	for _, session := range snapshot.Sessions {
		if session.Executor == domain.WorkflowExecutorAction {
			finalSession = session
		}
	}
	if finalSession.ID == "" || finalSession.BaseRef != "jobs/existing/main" || finalSession.TargetBranch != "jobs/existing/main" {
		t.Fatalf("backfilled PR Session = %+v", finalSession)
	}
}

func TestWorkflowTemplateMovesJobThroughDeliverablesQuestionsAndRejectLimit(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "workflow", RemoteURL: "https://example.com/workflow.git", DefaultRef: "main"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{
		Operator: "derek", Name: "Ontwikkeling", Phases: []domain.WorkflowPhase{
			{ID: "design", Name: "Ontwerp", Instructions: "Maak het ontwerp", Deliverables: []domain.DeliverableDefinition{{Name: "FO", Required: true}}, Accept: domain.WorkflowTransition{Target: "develop"}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf, Max: 2, Exhausted: domain.WorkflowTargetAskUser}},
			{ID: "develop", Name: "Ontwikkelen", Instructions: "Bouw het", Inject: []string{"FO"}, AllowChanges: true, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf, Max: 2, Exhausted: domain.WorkflowTargetAskUser}},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	createRequest := domain.CreateJobRequest{Title: "Darkmode", Objective: "Een switch", IdempotencyKey: "browser-request-1", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID}
	created, err := st.CreateJob(createRequest)
	if err != nil {
		t.Fatal(err)
	}
	replayed, err := st.CreateJob(createRequest)
	if err != nil || !replayed.Replayed || replayed.Job.ID != created.Job.ID || replayed.Session.ID != created.Session.ID || len(st.Snapshot().Jobs) != 1 {
		t.Fatalf("idempotent replay = %+v, error = %v", replayed, err)
	}
	if created.Job.WorkflowStatus != domain.WorkflowBusy || created.Session.PhaseRunID == "" {
		t.Fatalf("created workflow = %+v / %+v", created.Job, created.Session)
	}
	client, err := st.RegisterClient(domain.RegisterClientRequest{Name: "external", Capabilities: domain.ClientCapabilities{Tools: []string{"agent"}}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.Claim(domain.ClaimRequest{ClientID: client.ID, Tools: []string{"agent"}}); !errors.Is(err, ErrNoWork) {
		t.Fatalf("external worker claimed ACP-managed workflow Session: %v", err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "klaar"); !errors.Is(err, ErrConflict) {
		t.Fatalf("accept without FO error = %v", err)
	}
	deliverable, err := st.AddWorkflowDeliverable(created.Session.ID, "FO", "# Functioneel ontwerp")
	if err != nil || deliverable.Revision != 1 {
		t.Fatalf("deliverable = %+v, error = %v", deliverable, err)
	}
	historicalComment, err := st.AddDeliverableComment(deliverable.ID, "john", domain.CreateDeliverableCommentRequest{
		SelectedText: "Functioneel ontwerp", StartOffset: 2, EndOffset: 23, Body: "Maak de gebruikersflow concreter.",
	})
	if err != nil || historicalComment.Author != "john" {
		t.Fatalf("historical comment = %+v, error = %v", historicalComment, err)
	}
	deliverable, err = st.AddWorkflowDeliverable(created.Session.ID, "FO", "# Functioneel ontwerp v2")
	if err != nil || deliverable.Revision != 2 {
		t.Fatalf("deliverable revision = %+v, error = %v", deliverable, err)
	}
	if _, err := st.AddDeliverableComment(historicalComment.DeliverableID, "derek", domain.CreateDeliverableCommentRequest{SelectedText: "Functioneel ontwerp", StartOffset: 2, EndOffset: 23, Body: "Retroactief comment"}); !errors.Is(err, ErrConflict) {
		t.Fatalf("historical revision comment error = %v", err)
	}
	currentComment, err := st.AddDeliverableComment(deliverable.ID, "derek", domain.CreateDeliverableCommentRequest{
		SelectedText: "ontwerp v2", StartOffset: 13, EndOffset: 23, Prefix: "Functioneel ", Body: "Vermeld ook de foutstatus.",
	})
	if err != nil || currentComment.Author != "derek" {
		t.Fatalf("current comment = %+v, error = %v", currentComment, err)
	}
	advance, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "ontwerp staat")
	if err != nil || advance.NextSession == nil || advance.PhaseRun.PhaseID != "develop" || advance.NextSession.BaseRef != created.Job.Branch {
		t.Fatalf("advance to develop = %+v, error = %v", advance, err)
	}
	develop := *advance.NextSession
	if _, err := st.MarkWorkflowPhaseRunning(develop.ID); err != nil {
		t.Fatal(err)
	}
	_, err = st.AskWorkflowQuestion(develop.ID, "Is deze API publiek?")
	if err != nil {
		t.Fatal(err)
	}
	resumed, err := st.ResumeWorkflowPhaseForChat(develop.ID, "derek")
	if err != nil || !resumed {
		t.Fatalf("resume through CHAT = %v, error = %v", resumed, err)
	}
	firstReject, err := st.CompleteWorkflowPhase(develop.ID, "reject", "tests falen")
	if err != nil || firstReject.NextSession == nil {
		t.Fatalf("first reject = %+v, error = %v", firstReject, err)
	}
	second := *firstReject.NextSession
	if _, err := st.MarkWorkflowPhaseRunning(second.ID); err != nil {
		t.Fatal(err)
	}
	secondReject, err := st.CompleteWorkflowPhase(second.ID, "reject", "nog niet goed")
	if err != nil || secondReject.Question == nil || secondReject.Job.WorkflowStatus != domain.WorkflowPending || secondReject.Job.PendingReason != "user" {
		t.Fatalf("reject limit = %+v, error = %v", secondReject, err)
	}
	retry, err := st.AnswerWorkflowQuestion(secondReject.Question.ID, "derek", "reject", "werk dit verder uit")
	if err != nil || retry.NextSession == nil || retry.PhaseRun.Attempt != 3 {
		t.Fatalf("manual retry = %+v, error = %v", retry, err)
	}
	snapshot := st.Snapshot()
	if len(snapshot.WorkflowTemplates) != 1 || len(snapshot.Deliverables) != 2 || len(snapshot.DeliverableComments) != 2 || len(snapshot.WorkflowQuestions) != 2 || len(snapshot.PhaseRuns) != 4 {
		t.Fatalf("workflow snapshot = %+v", snapshot)
	}
	if _, err := st.DeleteJob(created.Job.ID, "derek"); err != nil {
		t.Fatal(err)
	}
	snapshot = st.Snapshot()
	if len(snapshot.Jobs) != 0 || len(snapshot.Sessions) != 0 || len(snapshot.PhaseRuns) != 0 || len(snapshot.Deliverables) != 0 || len(snapshot.DeliverableComments) != 0 || len(snapshot.WorkflowQuestions) != 0 {
		t.Fatalf("deleted workflow Job left state behind: %+v", snapshot)
	}
	recreated, err := st.CreateJob(createRequest)
	if err != nil || recreated.Replayed || recreated.Job.ID == created.Job.ID {
		t.Fatalf("deleted Job retained its idempotency key: %+v, error = %v", recreated, err)
	}
}

func TestWorkflowAskUserGatesAIOutcomeWithExplicitRoutes(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "gated", RemoteURL: "https://example.com/gated.git", DefaultRef: "main"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{
		Operator: "derek", Name: "Gated", Phases: []domain.WorkflowPhase{
			{ID: "build", Name: "Build", Instructions: "Bouw het", Accept: domain.WorkflowTransition{Target: "review", AskUser: true}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf}},
			{ID: "review", Name: "Review", Instructions: "Review het", Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: "build"}},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Gate", Objective: "Test", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	rejected, err := st.CompleteWorkflowPhase(created.Session.ID, "reject", "nog niet klaar")
	if err != nil || rejected.Question != nil || rejected.NextSession == nil {
		t.Fatalf("ungated AI reject = %+v, error = %v", rejected, err)
	}
	if rejected.NextSession.BaseRef != created.Session.BaseRef {
		t.Fatalf("AI reject base ref = %q, want rejected Session base %q", rejected.NextSession.BaseRef, created.Session.BaseRef)
	}
	if _, err := st.MarkWorkflowPhaseRunning(rejected.NextSession.ID); err != nil {
		t.Fatal(err)
	}
	pending, err := st.CompleteWorkflowPhase(rejected.NextSession.ID, "accept", "code is klaar")
	if err != nil || pending.Question == nil {
		t.Fatalf("AI accept gate = %+v, error = %v", pending, err)
	}
	if pending.Question.Outcome != "accept" || pending.Question.AcceptTarget != "review" || pending.Question.RejectTarget != "build" {
		t.Fatalf("decision routes = %+v", pending.Question)
	}
	if _, err := st.AnswerWorkflowQuestion(pending.Question.ID, "derek", "reject", ""); !errors.Is(err, ErrConflict) {
		t.Fatalf("reject without own reason error = %v", err)
	}
	humanRejected, err := st.AnswerWorkflowQuestion(pending.Question.ID, "derek", "reject", "werk de feedback uit")
	if err != nil || humanRejected.NextSession == nil || humanRejected.PhaseRun.PhaseID != "build" {
		t.Fatalf("human reject = %+v, error = %v", humanRejected, err)
	}
	if humanRejected.Question == nil || humanRejected.Question.AgentDetail != "code is klaar" || humanRejected.Question.AnsweredBy != "derek" || humanRejected.Question.Reason != "werk de feedback uit" {
		t.Fatalf("decision audit = %+v", humanRejected.Question)
	}
	if humanRejected.NextSession.BaseRef != rejected.NextSession.BaseRef {
		t.Fatalf("human reject base ref = %q, want rejected Session base %q", humanRejected.NextSession.BaseRef, rejected.NextSession.BaseRef)
	}

	// Simulate a Session queued by the older transition code. Retry must repair
	// its unpublished Job-branch basis before materialization is attempted again.
	st.mu.Lock()
	broken := st.state.Sessions[humanRejected.NextSession.ID]
	broken.BaseRef = created.Job.Branch
	st.state.Sessions[broken.ID] = broken
	st.mu.Unlock()
	retried, _, err := st.RetryWorkflowSession(broken.ID, "derek")
	if err != nil || retried.Session.BaseRef != rejected.NextSession.BaseRef {
		t.Fatalf("repaired retry = %+v, error = %v", retried, err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(retried.Session.ID); err != nil {
		t.Fatal(err)
	}
	reviewGate, err := st.CompleteWorkflowPhase(retried.Session.ID, "accept", "code is nu klaar")
	if err != nil || reviewGate.Question == nil {
		t.Fatalf("second AI accept gate = %+v, error = %v", reviewGate, err)
	}
	accepted, err := st.AnswerWorkflowQuestion(reviewGate.Question.ID, "derek", "accept", "")
	if err != nil || accepted.NextSession == nil || accepted.PhaseRun.PhaseID != "review" {
		t.Fatalf("human accept = %+v, error = %v", accepted, err)
	}
}

func TestWorkflowKeepsAgentOutcomeHistoryWhenChatResumesSamePhaseRun(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "history", RemoteURL: "https://example.com/history.git", DefaultRef: "main"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{
		Operator: "derek", Name: "History", Phases: []domain.WorkflowPhase{{
			ID: "develop", Name: "Ontwikkel", Instructions: "Bouw het",
			Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone},
			Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetAskUser},
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "History", Objective: "Test", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	rejected, err := st.CompleteWorkflowPhase(created.Session.ID, "reject", "werk dit verder uit")
	if err != nil || rejected.Question == nil {
		t.Fatalf("AI reject = %+v, error = %v", rejected, err)
	}
	if rejected.Question.AgentOutcomeID == "" {
		t.Fatalf("AI reject question has no outcome link: %+v", rejected.Question)
	}
	if resumed, err := st.ResumeWorkflowPhaseForChat(created.Session.ID, "derek"); err != nil || !resumed {
		t.Fatalf("resume through CHAT = %v, error = %v", resumed, err)
	}
	accepted, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "implementatie is nu klaar")
	if err != nil || accepted.NextSession == nil || accepted.NextSession.Executor != domain.WorkflowExecutorAction || accepted.Job.WorkflowStatus != domain.WorkflowBusy {
		t.Fatalf("AI accept after CHAT = %+v, error = %v", accepted, err)
	}
	snapshot := st.Snapshot()
	run := snapshot.PhaseRuns[0]
	if len(run.AgentOutcomes) != 2 || run.AgentOutcomes[0].Outcome != "reject" || run.AgentOutcomes[1].Outcome != "accept" {
		t.Fatalf("agent outcome history = %+v", run.AgentOutcomes)
	}
	if run.AgentOutcomes[0].ID != rejected.Question.AgentOutcomeID || run.AgentOutcomes[1].Detail != "implementatie is nu klaar" {
		t.Fatalf("agent outcome audit = %+v / question %+v", run.AgentOutcomes, rejected.Question)
	}
	// The chat decided nothing; the agent's new accept replaced the standing
	// reject decision, which nobody ever answered.
	if len(snapshot.WorkflowQuestions) != 1 || snapshot.WorkflowQuestions[0].Status != "superseded" || snapshot.WorkflowQuestions[0].Answer != "" || snapshot.WorkflowQuestions[0].AnsweredBy != "" {
		t.Fatalf("CHAT audit = %+v", snapshot.WorkflowQuestions)
	}
}

func TestUpdateWorkflowTemplateCreatesNewRevisionWithoutMutatingActiveJob(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "editable", RemoteURL: "https://example.com/editable.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Development", Phases: []domain.WorkflowPhase{{
		ID: "develop", Name: "Ontwikkel", Instructions: "Bouw het", Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Edit active template", Objective: "Allow changes", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	updated, err := st.UpdateWorkflowTemplate(template.ID, domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Development", Description: "Updated in place", Phases: []domain.WorkflowPhase{{
		ID: "develop", Name: "Ontwikkel", Instructions: "Bouw het nu echt", AllowChanges: true, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil || updated.ID != template.ID || updated.Revision != 2 || !updated.Phases[0].AllowChanges {
		t.Fatalf("updated template = %+v, error = %v", updated, err)
	}
	_, activeTemplate, _, phase, _, _, err := st.WorkflowForSession(created.Session.ID)
	if err != nil || activeTemplate.Revision != 1 || activeTemplate.Description != "" || phase.AllowChanges || phase.Instructions != "Bouw het" {
		t.Fatalf("active workflow snapshot changed: template=%+v phase=%+v error=%v", activeTemplate, phase, err)
	}
	replaced, err := st.UpdateWorkflowTemplate(template.ID, domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Development", Phases: []domain.WorkflowPhase{{
		ID: "replacement", Name: "Ontwikkel", Instructions: "Break active phase", AllowChanges: true, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil || replaced.Revision != 3 || replaced.Phases[0].ID != "replacement" {
		t.Fatalf("new template revision = %+v, error = %v", replaced, err)
	}
	_, activeTemplate, _, phase, _, _, err = st.WorkflowForSession(created.Session.ID)
	if err != nil || activeTemplate.Revision != 1 || phase.ID != "develop" {
		t.Fatalf("active workflow lost frozen phase: template=%+v phase=%+v error=%v", activeTemplate, phase, err)
	}
}

func TestWorkflowPhaseOwnsItsEnvironmentRecipe(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "developer", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "developer-acp"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "reviewer", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "reviewer-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "phase-env", RemoteURL: "https://example.com/phase-env.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Phase environments", Phases: []domain.WorkflowPhase{
		{ID: "develop", Name: "Ontwikkel", Instructions: "Bouw", EnvironmentSelector: "tool:developer", Accept: domain.WorkflowTransition{Target: "review"}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf}},
		{ID: "review", Name: "Review", Instructions: "Review", EnvironmentSelector: "tool:reviewer", Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: "develop"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Per phase", Objective: "Use recipes", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:developer", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if created.Session.EnvironmentSelector != "tool:developer" || created.Session.Executor != domain.WorkflowExecutorAgent {
		t.Fatalf("develop Session recipe = %+v", created.Session)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	advance, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "klaar")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("advance = %+v, error = %v", advance, err)
	}
	if advance.NextSession.EnvironmentSelector != "tool:reviewer" || advance.NextSession.Tool != "reviewer" {
		t.Fatalf("review Session recipe = %+v", advance.NextSession)
	}
}

func TestWorkflowPhaseCanAddOwnWithLayersToJobDefaultEnvironment(t *testing.T) {
	phase := domain.WorkflowPhase{WithSelectors: []string{"tool:reviewer"}}
	selector, withSelectors := workflowPhaseEnvironment(phase, "tool:codex", []string{"tool:dotnet"})
	if selector != "tool:codex" || len(withSelectors) != 2 || withSelectors[0] != "tool:dotnet" || withSelectors[1] != "tool:reviewer" {
		t.Fatalf("phase recipe = USE %s WITH %v", selector, withSelectors)
	}

	selector, withSelectors = workflowPhaseEnvironment(domain.WorkflowPhase{}, "tool:codex", []string{"tool:dotnet"})
	if selector != "tool:codex" || len(withSelectors) != 1 || withSelectors[0] != "tool:dotnet" {
		t.Fatalf("fallback recipe = USE %s WITH %v", selector, withSelectors)
	}
}

func TestMandatoryPullRequestFailureCanOnlyReturnToPullRequestPhase(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "mandatory-pr", RemoteURL: "https://github.com/derek/mandatory-pr.git", DefaultRef: "main"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Always PR", Phases: []domain.WorkflowPhase{{
		ID: "review", Name: "Review", Instructions: "Review",
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Always PR", Objective: "Never finish without one", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	advance, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "review akkoord")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("review advance = %+v, error = %v", advance, err)
	}
	firstAction := advance.NextSession
	if _, err := st.MarkWorkflowPhaseRunning(firstAction.ID); err != nil {
		t.Fatal(err)
	}
	advance, err = st.CompleteWorkflowPhase(firstAction.ID, "reject", "provider unavailable")
	if err != nil || advance.NextSession == nil {
		t.Fatalf("first PR retry = %+v, error = %v", advance, err)
	}
	secondAction := advance.NextSession
	if _, err := st.MarkWorkflowPhaseRunning(secondAction.ID); err != nil {
		t.Fatal(err)
	}
	advance, err = st.CompleteWorkflowPhase(secondAction.ID, "reject", "provider still unavailable")
	if err != nil || advance.Question == nil || advance.Question.Kind != "action" || advance.Job.WorkflowStatus != domain.WorkflowPending {
		t.Fatalf("PR failure gate = %+v, error = %v", advance, err)
	}
	advance, err = st.AnswerWorkflowQuestion(advance.Question.ID, "derek", "accept", "")
	if err != nil || advance.NextSession == nil || advance.NextSession.Executor != domain.WorkflowExecutorAction || advance.Job.WorkflowStatus != domain.WorkflowBusy {
		t.Fatalf("PR retry answer = %+v, error = %v", advance, err)
	}
}

func TestWorkflowInjectionMustComeFromEarlierPhaseAndExistBeforeTransition(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Invalid injection", Phases: []domain.WorkflowPhase{{
		ID: "build", Name: "Build", Instructions: "Build", Inject: []string{"FO"}, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}}); !errors.Is(err, ErrConflict) {
		t.Fatalf("unknown injected deliverable error = %v", err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "dependency", RemoteURL: "https://example.com/dependency.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Dependency", Phases: []domain.WorkflowPhase{
		{ID: "design", Name: "Design", Instructions: "Design", Deliverables: []domain.DeliverableDefinition{{Name: "FO"}}, Accept: domain.WorkflowTransition{Target: "build"}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf}},
		{ID: "build", Name: "Build", Instructions: "Build", Inject: []string{"FO"}, Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Dependency", Objective: "Use FO", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "done"); !errors.Is(err, ErrConflict) {
		t.Fatalf("missing injected deliverable error = %v", err)
	}
	run := st.Snapshot().PhaseRuns[0]
	if run.Status != domain.PhaseRunRunning {
		t.Fatalf("failed transition mutated phase to %s", run.Status)
	}
}

func TestRetryWorkflowSessionRequeuesSameAttemptAndClosesOpenQuestion(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "retry", RemoteURL: "https://example.com/retry.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Retry", Phases: []domain.WorkflowPhase{{
		ID: "plan", Name: "Plan", Instructions: "Maak een plan", Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Retry me", Objective: "Blijf dezelfde Session", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	question, err := st.AskWorkflowQuestion(created.Session.ID, "Welke richting?")
	if err != nil {
		t.Fatal(err)
	}
	st.mu.Lock()
	prepared := st.state.Sessions[created.Session.ID]
	prepared.PreparedCompositionID = "cmp_previous"
	st.state.Sessions[prepared.ID] = prepared
	st.mu.Unlock()

	retried, compositionID, err := st.RetryWorkflowSession(created.Session.ID, "derek")
	if err != nil {
		t.Fatal(err)
	}
	if compositionID != "cmp_previous" || retried.Job.ID != created.Job.ID || retried.Session.ID != created.Session.ID || retried.Session.PreparedCompositionID != "" {
		t.Fatalf("retry created replacement state: response=%+v composition=%q", retried, compositionID)
	}
	snapshot := st.Snapshot()
	if len(snapshot.PhaseRuns) != 1 || snapshot.PhaseRuns[0].ID != created.Session.PhaseRunID || snapshot.PhaseRuns[0].Attempt != 1 || snapshot.PhaseRuns[0].Status != domain.PhaseRunQueued {
		t.Fatalf("phase retry = %+v", snapshot.PhaseRuns)
	}
	if len(snapshot.WorkflowQuestions) != 1 || snapshot.WorkflowQuestions[0].ID != question.ID || snapshot.WorkflowQuestions[0].Status != "answered" || snapshot.WorkflowQuestions[0].Answer != "retry" {
		t.Fatalf("question after retry = %+v", snapshot.WorkflowQuestions)
	}
	if _, _, err := st.RetryWorkflowSession(created.Session.ID, "john"); !errors.Is(err, ErrConflict) {
		t.Fatalf("other user retry error = %v", err)
	}
}

func TestCloseJobPreservesWorkflowHistoryAndClosesActiveDecision(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	st, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "close", RemoteURL: "https://example.com/close.git", DefaultRef: "main"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Approval", Phases: []domain.WorkflowPhase{{
		ID: "review", Name: "Review", Instructions: "Review",
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone, AskUser: true},
		Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Close me", Objective: "Preserve me", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	advance, err := st.CompleteWorkflowPhase(created.Session.ID, "accept", "ready for approval")
	if err != nil || advance.Question == nil {
		t.Fatalf("approval gate = %+v, error = %v", advance, err)
	}
	if _, err := st.CloseJob(created.Job.ID, "john"); !errors.Is(err, ErrConflict) {
		t.Fatalf("other user closed Job: %v", err)
	}
	closed, err := st.CloseJob(created.Job.ID, "derek")
	if err != nil {
		t.Fatal(err)
	}
	if closed.Status != domain.JobCancelled || closed.WorkflowStatus != domain.WorkflowDone || closed.CurrentPhaseRunID != "" {
		t.Fatalf("closed Job = %+v", closed)
	}
	snapshot := st.Snapshot()
	if len(snapshot.Jobs) != 1 || len(snapshot.Sessions) != 1 || len(snapshot.PhaseRuns) != 1 || len(snapshot.WorkflowQuestions) != 1 {
		t.Fatalf("close removed history: jobs=%d sessions=%d runs=%d questions=%d", len(snapshot.Jobs), len(snapshot.Sessions), len(snapshot.PhaseRuns), len(snapshot.WorkflowQuestions))
	}
	if snapshot.Sessions[0].Status != domain.SessionCancelled || snapshot.PhaseRuns[0].Status != domain.PhaseRunRejected || snapshot.WorkflowQuestions[0].Status != "answered" || snapshot.WorkflowQuestions[0].Answer != "closed" {
		t.Fatalf("closed workflow state = session %+v, run %+v, question %+v", snapshot.Sessions[0], snapshot.PhaseRuns[0], snapshot.WorkflowQuestions[0])
	}
	reopened, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := reopened.Snapshot(); len(got.PhaseRuns) != 1 || got.Jobs[0].Status != domain.JobCancelled {
		t.Fatalf("closed Job was reactivated on open: %+v / %+v", got.Jobs, got.PhaseRuns)
	}
}

func TestClosedJobForkStartsFromRemoteResultAndPreservesContextReference(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	git := recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "git", Scope: domain.ScopeGlobal, Enables: []domain.Enablement{{Name: "git"}}})
	recordArtifact(t, st, domain.CreateRecordingRequest{Actor: "derek", Kind: domain.ArtifactTool, Name: "agent", Scope: domain.ScopeGlobal, ParentArtifactIDs: []string{git.ID}, Enables: []domain.Enablement{{Name: "acp", Command: "agent-acp"}}})
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "source", RemoteURL: "https://example.com/source.git", DefaultRef: "main"})
	if err != nil {
		t.Fatal(err)
	}
	otherRepository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "other", RemoteURL: "https://example.com/other.git", DefaultRef: "develop"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Fork context", Phases: []domain.WorkflowPhase{{
		ID: "plan", Name: "Plan", Instructions: "Documenteer", Deliverables: []domain.DeliverableDefinition{{Name: "FO", Description: "Functioneel ontwerp"}},
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	source, err := st.CreateJob(domain.CreateJobRequest{Title: "Eerste implementatie", Objective: "Lever de eerste versie", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(source.Session.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := st.AddWorkflowDeliverable(source.Session.ID, "FO", "# Bestaand ontwerp\n\nGebruik dit in vervolgwerk."); err != nil {
		t.Fatal(err)
	}
	closed, err := st.CloseJob(source.Job.ID, "derek")
	if err != nil {
		t.Fatal(err)
	}
	fork, err := st.CreateJob(domain.CreateJobRequest{
		Title: "Feedback verwerken", Objective: "Los de nagekomen feedback op", Operator: "john",
		ForkedFromJobID: closed.ID, GitRepositoryID: otherRepository.Repository.ID, BaseRef: "wrong-base",
		EnvironmentSelector: "tool:agent", TemplateID: template.ID,
	})
	if err != nil {
		t.Fatal(err)
	}
	if fork.Job.ForkedFromJobID != closed.ID || fork.Job.GitRepositoryID != repository.Repository.ID || fork.Job.BaseRef != closed.Branch || fork.Job.Owner != "john" {
		t.Fatalf("fork did not inherit immutable source coordinates: source=%+v fork=%+v", closed, fork.Job)
	}
	if fork.Job.Branch == closed.Branch {
		t.Fatalf("fork reused source branch %q", closed.Branch)
	}
	if _, err := st.DeleteJob(closed.ID, "derek"); !errors.Is(err, ErrConflict) {
		t.Fatalf("context source could be deleted while fork refers to it: %v", err)
	}
	if _, err := st.CreateJob(domain.CreateJobRequest{Title: "Invalid", Objective: "Cannot fork active", Operator: "john", ForkedFromJobID: fork.Job.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID}); !errors.Is(err, ErrConflict) {
		t.Fatalf("active Job was forkable: %v", err)
	}
}
