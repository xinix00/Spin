package server

import (
	"strings"
	"testing"
	"time"

	"easyacp/replica"
)

// The verdict names what is wrong in words, and is the worst of its findings.
func TestHealthJudgesTheReplicaInWords(t *testing.T) {
	started := processStarted
	processStarted = time.Now().Add(-time.Hour)
	defer func() { processStarted = started }()
	s := &Server{}
	now := time.Now()
	cases := []struct {
		name        string
		replication *replica.Status
		level       string
		says        string
	}{
		{"no replica", nil, healthWarning, "Geen replica"},
		{"healthy", &replica.Status{Complete: true, LastSyncAt: now}, healthOK, ""},
		{"stopped syncing", &replica.Status{Complete: true, LastSyncAt: now.Add(-10 * time.Minute)}, healthError, "niet meer gesynchroniseerd"},
		{"never synced", &replica.Status{Complete: true}, healthError, "nog geen enkele keer"},
		{"failing", &replica.Status{Complete: true, LastSyncAt: now, LastError: "compact: cannot merge a gap in commit sequence"}, healthError, "cannot merge a gap"},
		{"no complete generation", &replica.Status{LastSyncAt: now}, healthError, "geen complete generatie"},
		{"copying", &replica.Status{LastSyncAt: now, Copy: &replica.CopyProgress{Stage: "upload", Done: 1, Total: 4}}, healthInfo, "25%"},
		{"backlog", &replica.Status{Complete: true, LastSyncAt: now, PendingPages: 50000}, healthWarning, "50000 pagina's"},
	}
	for _, c := range cases {
		report := s.health(storageInfo{Replication: c.replication})
		if report.Level != c.level {
			t.Errorf("%s: level %s, want %s (%+v)", c.name, report.Level, c.level, report.Findings)
			continue
		}
		var said []string
		for _, finding := range report.Findings {
			said = append(said, finding.Message)
		}
		if c.says != "" && !strings.Contains(strings.Join(said, " | "), c.says) {
			t.Errorf("%s: findings %q do not say %q", c.name, said, c.says)
		}
		if c.says == "" && len(said) != 0 {
			t.Errorf("%s: a healthy server has findings %q", c.name, said)
		}
	}
	processStarted = time.Now()
	if report := s.health(storageInfo{Replication: &replica.Status{Complete: true, LastSyncAt: now}}); report.Level != healthInfo || !strings.Contains(report.Findings[0].Message, "herstart") {
		t.Fatalf("a young process is not said: %+v", report)
	}
}

// On HopOS the clock reads 1970 when the process starts. The start is taken
// once the clock is set, and a first copy that makes progress is said as a
// copy, with its reason, not as a replica that never synced.
func TestHealthTakesTheStartOnceTheClockIsSet(t *testing.T) {
	started := processStarted
	defer func() { processStarted = started }()
	processStarted = time.Unix(0, 0)
	s := &Server{}
	copying := &replica.Status{Copy: &replica.CopyProgress{Stage: "upload", Done: 39, Total: 100, Reason: "generation g is 169h old, the limit is 168h0m0s"}}
	report := s.health(storageInfo{Replication: copying})
	var said []string
	for _, finding := range report.Findings {
		said = append(said, finding.Message)
	}
	text := strings.Join(said, " | ")
	if strings.Contains(text, "497") || strings.Contains(text, "nog geen enkele keer") {
		t.Fatalf("findings %q", said)
	}
	if !strings.Contains(text, "Reden: generation g is 169h old") || !strings.Contains(text, "39%") {
		t.Fatalf("the copy is not said with its reason: %q", said)
	}
	if processStarted.Year() < 2020 {
		t.Fatal("the start stayed at 1970")
	}
}
