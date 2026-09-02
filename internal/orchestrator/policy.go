package orchestrator

import (
	"slices"

	"easyacp/internal/domain"
)

// Recommend evaluates the immutable Job/Session/Result graph. It returns
// bounded orchestration actions; the server or a human remains responsible for
// authorization and execution.
func Recommend(snapshot domain.Snapshot) []domain.Recommendation {
	var recommendations []domain.Recommendation
	for _, job := range snapshot.Jobs {
		recommendations = append(recommendations, recommendJob(snapshot, job)...)
	}
	slices.SortFunc(recommendations, func(a, b domain.Recommendation) int {
		return b.Priority - a.Priority
	})
	return recommendations
}

func recommendJob(snapshot domain.Snapshot, job domain.Job) []domain.Recommendation {
	if job.Status == domain.JobDone || job.Status == domain.JobCancelled {
		return nil
	}
	var active []domain.Session
	var completed []domain.Session
	for _, session := range snapshot.Sessions {
		if session.JobID != job.ID {
			continue
		}
		if session.Status == domain.SessionCompleted {
			completed = append(completed, session)
		} else if session.Status != domain.SessionCancelled {
			active = append(active, session)
		}
	}
	if len(active) > 0 {
		session := active[0]
		action := "continue_session"
		reason := "Er is al een uitvoerbare of actieve Session; behoud continuity en wacht op haar Result."
		if session.Status == domain.SessionFrozen {
			action = "restore_session"
			reason = "De Session is frozen; restore het warmste compatibele Checkpoint."
		}
		return []domain.Recommendation{{
			JobID: job.ID, Action: action, Reason: reason, SessionID: session.ID,
			CheckpointID: session.CurrentCheckpointID, Priority: 40,
		}}
	}

	var successful []domain.Result
	var unfinished []domain.Result
	for _, result := range snapshot.Results {
		if result.JobID != job.ID {
			continue
		}
		if result.Status == domain.ResultSuccess {
			successful = append(successful, result)
		} else {
			unfinished = append(unfinished, result)
		}
	}
	if len(successful) > 1 {
		ids := make([]string, 0, len(successful))
		for _, result := range successful {
			ids = append(ids, result.ID)
		}
		return []domain.Recommendation{{
			JobID: job.ID, Action: "start_critic", ResultIDs: ids, Priority: 90,
			Reason: "Er zijn meerdere succesvolle kandidaat-Results; laat ze vergelijken of synthetiseren vóór finale selectie.",
		}}
	}
	if len(successful) == 1 {
		result := successful[0]
		return []domain.Recommendation{{
			JobID: job.ID, Action: "select_result", ResultIDs: []string{result.ID}, Priority: 80,
			Reason: "Er is één succesvol Result met bewijs; leg het voor aan een menselijke reviewer.",
		}}
	}
	if len(unfinished) > 0 {
		result := unfinished[len(unfinished)-1]
		return []domain.Recommendation{{
			JobID: job.ID, Action: "fork_session", SessionID: result.SessionID,
			CheckpointID: result.CheckpointID, ResultIDs: []string{result.ID}, Priority: 70,
			Reason: "De laatste Session leverde geen volledig succesvol Result; maak een gerichte reparatiefork vanaf haar Result-Checkpoint.",
		}}
	}
	if len(completed) > 0 {
		return []domain.Recommendation{{
			JobID: job.ID, Action: "inspect_session", SessionID: completed[len(completed)-1].ID, Priority: 60,
			Reason: "Een completed Session mist een bruikbaar Result; inspecteer de contractinvariant.",
		}}
	}
	return []domain.Recommendation{{
		JobID: job.ID, Action: "start_session", Priority: 50,
		Reason: "De Job heeft nog geen actieve Session of Result; start een primaire Session vanaf de Job-root.",
	}}
}
