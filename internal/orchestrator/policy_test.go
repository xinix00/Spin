package orchestrator

import (
	"testing"

	"easyacp/internal/domain"
)

func TestRecommendMultipleSuccessfulResultsUsesCritic(t *testing.T) {
	snapshot := domain.Snapshot{
		Jobs: []domain.Job{{ID: "job-1", Status: domain.JobComparing}},
		Sessions: []domain.Session{
			{ID: "session-1", JobID: "job-1", Status: domain.SessionCompleted},
			{ID: "session-2", JobID: "job-1", Status: domain.SessionCompleted},
		},
		Results: []domain.Result{
			{ID: "result-1", JobID: "job-1", SessionID: "session-1", Status: domain.ResultSuccess},
			{ID: "result-2", JobID: "job-1", SessionID: "session-2", Status: domain.ResultSuccess},
		},
	}
	recommendations := Recommend(snapshot)
	if len(recommendations) != 1 || recommendations[0].Action != "start_critic" {
		t.Fatalf("unexpected recommendations: %+v", recommendations)
	}
}

func TestRecommendPartialResultUsesRepairFork(t *testing.T) {
	snapshot := domain.Snapshot{
		Jobs:     []domain.Job{{ID: "job-1", Status: domain.JobActive}},
		Sessions: []domain.Session{{ID: "session-1", JobID: "job-1", Status: domain.SessionCompleted}},
		Results: []domain.Result{{
			ID: "result-1", JobID: "job-1", SessionID: "session-1", CheckpointID: "checkpoint-1", Status: domain.ResultPartial,
		}},
	}
	recommendations := Recommend(snapshot)
	if len(recommendations) != 1 || recommendations[0].Action != "fork_session" || recommendations[0].CheckpointID != "checkpoint-1" {
		t.Fatalf("unexpected recommendations: %+v", recommendations)
	}
}
