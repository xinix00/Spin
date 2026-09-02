package store

import (
	"errors"
	"testing"
	"time"

	"easyacp/internal/domain"
)

func TestCodeReviewCommentsAreImmutableAndOnlyLatestAttemptIsAnnotatable(t *testing.T) {
	st, err := Open("")
	if err != nil {
		t.Fatal(err)
	}
	now := time.Now().UTC()
	st.state.Jobs["job_review"] = domain.Job{ID: "job_review", Owner: "derek", CreatedAt: now, UpdatedAt: now}
	st.state.PhaseRuns["run_1"] = domain.PhaseRun{ID: "run_1", JobID: "job_review", PhaseID: "build", Attempt: 1}

	first, err := st.SaveCodeReviewRevision(domain.CodeReviewRevision{
		JobID: "job_review", SourcePhaseRunID: "run_1", ContextPhaseRunID: "run_1",
		PhaseID: "build", Attempt: 1, Scope: "phase", ScopeKey: "job:job_review:phase:build",
		Digest: "first", CreatedBy: "derek", Files: []domain.CodeReviewFile{{Path: "main.go", Patch: "+old"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	duplicate, err := st.SaveCodeReviewRevision(domain.CodeReviewRevision{
		JobID: "job_review", SourcePhaseRunID: "run_1", ContextPhaseRunID: "run_1",
		PhaseID: "build", Attempt: 1, Scope: "phase", ScopeKey: "job:job_review:phase:build",
		Digest: "first", CreatedBy: "john", Files: []domain.CodeReviewFile{{Path: "main.go", Patch: "+old"}},
	})
	if err != nil || duplicate.ID != first.ID {
		t.Fatalf("duplicate revision = %+v, error = %v", duplicate, err)
	}
	if _, err := st.AddCodeReviewComment(first.ID, "john", domain.CreateCodeReviewCommentRequest{Path: "main.go", Side: "new", StartLine: 7, EndLine: 7, SelectedText: "old", Body: "Maak dit duidelijker."}); err != nil {
		t.Fatal(err)
	}
	st.state.PhaseRuns["run_2"] = domain.PhaseRun{ID: "run_2", JobID: "job_review", PhaseID: "build", Attempt: 2}
	second, err := st.SaveCodeReviewRevision(domain.CodeReviewRevision{
		JobID: "job_review", SourcePhaseRunID: "run_2", ContextPhaseRunID: "run_2",
		PhaseID: "build", Attempt: 2, Scope: "phase", ScopeKey: "job:job_review:phase:build",
		Digest: "second", CreatedBy: "derek", Files: []domain.CodeReviewFile{{Path: "main.go", Patch: "+new"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.AddCodeReviewComment(first.ID, "derek", domain.CreateCodeReviewCommentRequest{Path: "main.go", Side: "new", StartLine: 7, EndLine: 7, SelectedText: "old", Body: "Retroactief."}); !errors.Is(err, ErrConflict) {
		t.Fatalf("historical comment error = %v", err)
	}
	if _, err := st.AddCodeReviewComment(second.ID, "derek", domain.CreateCodeReviewCommentRequest{Path: "main.go", Side: "new", StartLine: 9, EndLine: 10, SelectedText: "new", Body: "Laatste feedback."}); err != nil {
		t.Fatal(err)
	}
	firstBundle, err := st.CodeReviewBundle(first.ID)
	if err != nil || firstBundle.Annotatable || len(firstBundle.Comments) != 1 || len(firstBundle.History) != 2 {
		t.Fatalf("first bundle = %+v, error = %v", firstBundle, err)
	}
	secondBundle, err := st.CodeReviewBundle(second.ID)
	if err != nil || !secondBundle.Annotatable || len(secondBundle.Comments) != 1 {
		t.Fatalf("second bundle = %+v, error = %v", secondBundle, err)
	}
	snapshot := st.Snapshot()
	if len(snapshot.CodeReviewRevisions) != 2 || len(snapshot.CodeReviewComments) != 2 || snapshot.CodeReviewRevisions[0].FileCount != 1 {
		t.Fatalf("review snapshot = %+v / %+v", snapshot.CodeReviewRevisions, snapshot.CodeReviewComments)
	}
}
