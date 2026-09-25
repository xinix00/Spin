package store

import (
	"path/filepath"
	"testing"
	"time"

	"easyacp/internal/domain"
)

// What a restarted server needs to go on with running agents is in the
// state: the agent of a capsule and the token its tools call with.
func TestRunningAgentsSurviveAReopen(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	st, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	st.mu.Lock()
	st.state.Sessions["ses_1"] = domain.Session{ID: "ses_1", PreparedCompositionID: "cmp_1"}
	st.state.Compositions["cmp_1"] = domain.Composition{ID: "cmp_1", Operator: "derek", SessionID: "ses_1", Runtime: &domain.CapsuleRuntime{ClientID: "cli_1", ContainerID: "c1", Status: "ready"}}
	st.mu.Unlock()
	agent := domain.AgentProcess{SessionID: "ses_1", Operator: "derek", StreamID: "str_1", AgentSessionID: "agent-1", PromptID: "7"}
	if err := st.SetCompositionAgent("cmp_1", "str_1", &agent); err != nil {
		t.Fatal(err)
	}
	if err := st.SetWorkflowToken("ses_1", "hash-1"); err != nil {
		t.Fatal(err)
	}

	reopened, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	composition, err := reopened.Composition("cmp_1")
	if err != nil || composition.Agent == nil || composition.Agent.StreamID != "str_1" || composition.Agent.PromptID != "7" {
		t.Fatalf("agent after a reopen = %+v, %v", composition.Agent, err)
	}
	if got := reopened.WorkflowToken("ses_1"); got != "hash-1" {
		t.Fatalf("workflow token after a reopen = %q", got)
	}
	// Forgetting another stream's agent leaves this one; a stop forgets it.
	if err := reopened.SetCompositionAgent("cmp_1", "str_other", nil); err != nil {
		t.Fatal(err)
	}
	if composition, _ := reopened.Composition("cmp_1"); composition.Agent == nil {
		t.Fatal("forgetting another stream took the agent away")
	}
	runtime := *composition.Runtime
	runtime.Status = "stopped"
	if _, err := reopened.SetCompositionRuntime("cmp_1", "derek", runtime); err != nil {
		t.Fatal(err)
	}
	if composition, _ := reopened.Composition("cmp_1"); composition.Agent != nil {
		t.Fatal("a stopped capsule still has an agent")
	}
}

// An offline runner may still run its capsules, even after a day. Neither
// its identity nor the reservation of its logins can expire on a timer.
func TestPendingStopsKeepTheirRunnerAfterADay(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	now := time.Now().UTC()
	st.mu.Lock()
	st.state.Clients["cli_away"] = domain.Client{ID: "cli_away", Status: "offline", LastSeenAt: now.Add(-25 * time.Hour)}
	st.state.Clients["cli_brief"] = domain.Client{ID: "cli_brief", Status: "offline", LastSeenAt: now.Add(-time.Minute)}
	st.state.Compositions["cmp_away"] = domain.Composition{ID: "cmp_away", Operator: "derek", Runtime: &domain.CapsuleRuntime{ClientID: "cli_away", Status: "ready", StopPending: true}}
	st.state.Compositions["cmp_brief"] = domain.Composition{ID: "cmp_brief", Operator: "derek", Runtime: &domain.CapsuleRuntime{ClientID: "cli_brief", Status: "ready", StopPending: true}}
	st.state.Compositions["cmp_running"] = domain.Composition{ID: "cmp_running", Operator: "derek", Runtime: &domain.CapsuleRuntime{ClientID: "cli_away", Status: "ready"}}
	st.mu.Unlock()
	removed, err := st.PruneClients(24 * time.Hour)
	if err != nil || len(removed) != 0 {
		t.Fatalf("removed runners with live capsules: %v, %v", removed, err)
	}
	if running := st.RunningCompositions(); len(running) != 3 {
		t.Fatalf("running after pruning = %d, want 3", len(running))
	}
	if st.ClientWorkloads("cli_away") != 2 || st.ClientWorkloads("cli_brief") != 1 {
		t.Fatalf("workloads away=%d brief=%d", st.ClientWorkloads("cli_away"), st.ClientWorkloads("cli_brief"))
	}
}
