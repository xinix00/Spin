package store

import (
	"errors"
	"path/filepath"
	"strings"
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
}

// A capsule that is gone from its runner holds no login: once the runner says
// what really runs there, the login goes to the next capsule that asks.
func TestACapsuleGoneFromItsRunnerFreesItsLogin(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	key := "global/credential:claude"
	login, err := st.CreateLogin("", key, map[string][]byte{"/root/.claude/.credentials.json": []byte(`{"token":"t"}`)}, "")
	if err != nil {
		t.Fatal(err)
	}
	for _, id := range []string{"cmp_gone", "cmp_next"} {
		if err := st.PutCompositionForTest(domain.Composition{ID: id, Operator: "derek", Runtime: &domain.CapsuleRuntime{ClientID: "cli_laptop", Status: "ready"}}); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := st.HandOutLogin("cmp_gone", key, true); err != nil {
		t.Fatal(err)
	}
	if _, err := st.HandOutLogin("cmp_next", key, true); !errors.Is(err, ErrLoginsBusy) {
		t.Fatalf("the only login went out twice: %v", err)
	}
	stopped, err := st.ReconcileClientCapsules("cli_laptop", []string{"cmp_next"}, nil)
	if err != nil || stopped != 1 {
		t.Fatalf("reconcile stopped %d, %v", stopped, err)
	}
	if got, err := st.HandOutLogin("cmp_next", key, true); err != nil || got.ID != login.ID {
		t.Fatalf("after the gone capsule stopped the login is %+v, %v", got, err)
	}
	// Another runner's capsules are not this report's business.
	if stopped, _ := st.ReconcileClientCapsules("cli_other", nil, nil); stopped != 0 {
		t.Fatalf("a report of another runner stopped %d capsules", stopped)
	}
}

// A composition that never got a capsule was being built by a process that is
// gone. It is discarded at the next start, and the login it held goes to the
// Job's next attempt instead of being held by its own earlier one.
func TestAnUnbuiltCompositionLetsGoOfItsLoginAtStart(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	key := "global/credential:claude"
	if _, err := st.CreateLogin("", key, map[string][]byte{"/root/.claude/.credentials.json": []byte(`{"token":"t"}`)}, ""); err != nil {
		t.Fatal(err)
	}
	for _, composition := range []domain.Composition{
		{ID: "cmp_attempt_1", Operator: "derek"},
		{ID: "cmp_attempt_2", Operator: "derek"},
		{ID: "cmp_built", Operator: "derek", Runtime: &domain.CapsuleRuntime{ClientID: "cli_laptop", Status: "ready"}},
	} {
		if err := st.PutCompositionForTest(composition); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := st.HandOutLogin("cmp_attempt_1", key, true); err != nil {
		t.Fatal(err)
	}
	if _, err := st.HandOutLogin("cmp_attempt_2", key, true); !errors.Is(err, ErrLoginsBusy) {
		t.Fatalf("the next attempt got a login held by the one before: %v", err)
	}
	discarded, err := st.DiscardUnbuiltCompositions()
	if err != nil || discarded != 2 {
		t.Fatalf("discarded %d, %v", discarded, err)
	}
	if _, err := st.Composition("cmp_built"); err != nil {
		t.Fatal("a composition with a capsule was discarded")
	}
	if err := st.PutCompositionForTest(domain.Composition{ID: "cmp_attempt_3", Operator: "derek"}); err != nil {
		t.Fatal(err)
	}
	if _, err := st.HandOutLogin("cmp_attempt_3", key, true); err != nil {
		t.Fatalf("after the start the login is still held: %v", err)
	}
}

// A runner's orphans are the capsules it runs that the state does not have
// running there: unknown, stopped, finished, or placed elsewhere. What runs
// there, and what exists without a runtime (being built), is no orphan.
func TestOrphanCapsulesAreWhatTheStateDoesNotHaveThere(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	for _, composition := range []domain.Composition{
		{ID: "cmp_live", Runtime: &domain.CapsuleRuntime{ClientID: "cli_a", Status: "ready"}},
		{ID: "cmp_stopped", Runtime: &domain.CapsuleRuntime{ClientID: "cli_a", Status: "stopped"}},
		{ID: "cmp_elsewhere", Runtime: &domain.CapsuleRuntime{ClientID: "cli_b", Status: "ready"}},
		{ID: "cmp_building"},
	} {
		if err := st.PutCompositionForTest(composition); err != nil {
			t.Fatal(err)
		}
	}
	compositions, recordings := st.OrphanCapsules("cli_a", []string{"cmp_live", "cmp_stopped", "cmp_elsewhere", "cmp_building", "cmp_unknown"}, []string{"rec_unknown"})
	if strings.Join(compositions, ",") != "cmp_stopped,cmp_elsewhere,cmp_unknown" || strings.Join(recordings, ",") != "rec_unknown" {
		t.Fatalf("orphans %v %v", compositions, recordings)
	}
}
