package server

import (
	"fmt"
	"math"
	"runtime/metrics"
	"time"
)

// Health is Spin judging itself, so nobody has to read a status line to find
// out that backups stopped: the replica that has not shipped for minutes, the
// generation that never completed, the heap that is about to run out. Each
// finding says what is wrong in words; the verdict is the worst of them.

const (
	healthOK      = "ok"
	healthInfo    = "info"
	healthWarning = "warning"
	healthError   = "error"
)

type healthFinding struct {
	Level   string `json:"level"`
	Message string `json:"message"`
}

type healthReport struct {
	Level    string          `json:"level"`
	Findings []healthFinding `json:"findings"`
}

// processStarted is when this process began: a young process after a crash
// is worth saying.
var processStarted = time.Now()

// replicaSyncOverdue is how long without a successful sync the replica is
// considered to have stopped; it syncs every 15 seconds.
const replicaSyncOverdue = 5 * time.Minute

func (s *Server) health(storage storageInfo) healthReport {
	report := healthReport{Level: healthOK, Findings: []healthFinding{}}
	add := func(level, format string, args ...any) {
		report.Findings = append(report.Findings, healthFinding{Level: level, Message: fmt.Sprintf(format, args...)})
		if healthRank(level) > healthRank(report.Level) {
			report.Level = level
		}
	}
	now := time.Now()
	uptime := now.Sub(processStarted)
	if replication := storage.Replication; replication == nil {
		add(healthWarning, "Geen replica: deze Spin staat alleen op zijn eigen volume.")
	} else {
		switch {
		case !replication.LastSyncAt.IsZero() && now.Sub(replication.LastSyncAt) > replicaSyncOverdue:
			add(healthError, "De replica heeft sinds %s niet meer gesynchroniseerd: wijzigingen komen niet in de bucket.", replication.LastSyncAt.Local().Format("15:04"))
		case replication.LastSyncAt.IsZero() && uptime > replicaSyncOverdue:
			add(healthError, "De replica heeft sinds de start (%s geleden) nog geen enkele keer gesynchroniseerd.", roundDuration(uptime))
		}
		if replication.LastError != "" {
			add(healthError, "Laatste replica-sync mislukt: %s", replication.LastError)
		}
		if copy := replication.Copy; copy != nil {
			percent := 0.0
			if copy.Total > 0 {
				percent = float64(copy.Done) / float64(copy.Total) * 100
			}
			add(healthInfo, "Een nieuwe generatie wordt gekopieerd (%s, %.0f%%); wijzigingen wachten zolang.", map[string]string{"read": "lezen", "upload": "naar de bucket"}[copy.Stage], percent)
		} else if !replication.Complete {
			add(healthError, "Er is geen complete generatie in de bucket: een restore kan nu niet.")
		}
		if replication.PendingPages > 10000 {
			add(healthWarning, "%d pagina's wachten op verzending naar de bucket.", replication.PendingPages)
		}
	}
	if used, limit, ok := memoryUse(); ok {
		share := float64(used) / float64(limit)
		switch {
		case share >= 0.9:
			add(healthError, "Geheugen op %.0f%% van de limiet (%d van %d MiB): de server loopt kans te stoppen.", share*100, used>>20, limit>>20)
		case share >= 0.75:
			add(healthWarning, "Geheugen op %.0f%% van de limiet (%d van %d MiB).", share*100, used>>20, limit>>20)
		}
	}
	if uptime < 10*time.Minute {
		add(healthInfo, "De server draait pas %s: is hij net herstart?", roundDuration(uptime))
	}
	return report
}

// memoryUse is what the Go runtime holds against its memory limit, read
// without stopping the world (runtime.ReadMemStats would, on every state).
func memoryUse() (used, limit uint64, ok bool) {
	samples := []metrics.Sample{{Name: "/memory/classes/total:bytes"}, {Name: "/memory/classes/heap/released:bytes"}, {Name: "/gc/gomemlimit:bytes"}}
	metrics.Read(samples)
	for _, sample := range samples {
		if sample.Value.Kind() != metrics.KindUint64 {
			return 0, 0, false
		}
	}
	total, released, limit := samples[0].Value.Uint64(), samples[1].Value.Uint64(), samples[2].Value.Uint64()
	if limit == 0 || limit >= math.MaxInt64 {
		return 0, 0, false
	}
	return total - released, limit, true
}

func healthRank(level string) int {
	return map[string]int{healthOK: 0, healthInfo: 1, healthWarning: 2, healthError: 3}[level]
}

func roundDuration(d time.Duration) string {
	switch {
	case d < time.Minute:
		return fmt.Sprintf("%d s", int(d.Seconds()))
	case d < time.Hour:
		return fmt.Sprintf("%d min", int(d.Minutes()))
	default:
		return fmt.Sprintf("%d u %d min", int(d.Hours()), int(d.Minutes())%60)
	}
}
