package server

import (
	"context"
	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
	"errors"
	"slices"
	"strings"
	"sync"
	"time"
)

type backgroundJobLaunch struct {
	cancel    context.CancelFunc
	done      chan struct{}
	startedAt time.Time
	clientID  string // set once the engine reports which runner took the work
	progress  launchProgress
}

// launchProgress is the last thing a launch reported: the base image on its
// way to the runner, the capsule coming up. It doubles as the liveness signal
// that keeps a slow launch from being cut off.
type launchProgress struct {
	Stage     string    `json:"stage,omitempty"`
	Message   string    `json:"message,omitempty"`
	Current   int64     `json:"current,omitempty"`
	Total     int64     `json:"total,omitempty"`
	UpdatedAt time.Time `json:"updated_at"`
}

// launchFailure is why the last launch of a queued Session gave up; the sweep
// tries again and the browser shows the reason meanwhile.
type launchFailure struct {
	Error string    `json:"error"`
	At    time.Time `json:"at"`
}

// sessionPreparation tells the browser that a Session is being started right
// now, on which runner once one has been chosen and how far it is, or why the
// last attempt failed while the next one is due. It is deliberately
// transient: it lives in the launch bookkeeping, never in durable state, so a
// failed attempt leaves nothing behind to clean up or to pin a Session with.
type sessionPreparation struct {
	SessionID string          `json:"session_id"`
	ClientID  string          `json:"client_id,omitempty"`
	StartedAt time.Time       `json:"started_at"`
	Progress  *launchProgress `json:"progress,omitempty"`
	Failure   *launchFailure  `json:"failure,omitempty"`
}

// launchSweepInterval is how often queued phases are offered a launch again.
// Runners connect, free capacity and fail transiently; a sweep covers all of
// that without a listener for each.
const launchSweepInterval = 30 * time.Second

// launchStallTimeout cuts off a launch that has reported nothing for this
// long. Shipping a gigabyte reports every chunk, so only a hung launch trips
// it; there is no cap on the total.
const launchStallTimeout = 3 * time.Minute

// sweepQueuedWorkflowPhases offers every queued phase a launch again at a
// fixed cadence. A launch that failed (no runner in time, a transfer that
// stalled, a transient error) leaves its phase queued; nothing else looks at
// it again, and a Job must not sit on "waiting for a runner" for that.
func (s *Server) sweepQueuedWorkflowPhases() {
	ticker := time.NewTicker(s.launchSweep)
	defer ticker.Stop()
	clients := time.NewTicker(time.Hour)
	defer clients.Stop()
	capsules := time.NewTicker(time.Minute)
	defer capsules.Stop()
	for {
		select {
		case <-ticker.C:
			s.launchQueuedWorkflowPhases("sweep")
		case <-clients.C:
			s.pruneClients()
		case <-capsules.C:
			s.sweepIdleCapsules()
		}
	}
}

// sweepIdleCapsules closes the capsule of every Job step that is over: a
// step the Job moved past, a Job that is done or closed, a step waiting
// for a person's answer, a Session that no longer exists. Each of those
// is closed the moment it happens as well; the sweep is what catches the
// ones that slipped through (a server restart at the wrong moment, an
// older version), so runners do not fill up with capsules nobody uses
// and the logins they hold come free.
func (s *Server) sweepIdleCapsules() {
	snapshot := s.store.Snapshot()
	sessions := map[string]domain.Session{}
	for _, session := range snapshot.Sessions {
		sessions[session.ID] = session
	}
	jobs := map[string]domain.Job{}
	for _, job := range snapshot.Jobs {
		jobs[job.ID] = job
	}
	runs := map[string]domain.PhaseRun{}
	for _, run := range snapshot.PhaseRuns {
		runs[run.ID] = run
	}
	s.jobLaunchMu.Lock()
	launching := map[string]bool{}
	for sessionID := range s.jobLaunching {
		launching[sessionID] = true
	}
	s.jobLaunchMu.Unlock()
	for _, composition := range snapshot.Compositions {
		// A composition that never got its capsule (a start that died with
		// the server) still holds the logins it reserved: it goes once no
		// launch is under way for it any more.
		if composition.Runtime == nil && time.Since(composition.CreatedAt) > 10*time.Minute && !launching[composition.SessionID] {
			if err := s.store.DiscardComposition(composition.ID, composition.Operator); err == nil {
				s.logger.Info("composition without a capsule discarded", "composition", composition.ID, "session", composition.SessionID)
			}
			continue
		}
		if composition.SessionID == "" || composition.Runtime == nil || composition.Runtime.Status == "stopped" {
			continue
		}
		reason := ""
		session, ok := sessions[composition.SessionID]
		switch {
		case !ok:
			reason = "its Session no longer exists"
		case session.PhaseRunID == "":
			continue // a person's own Session: theirs to stop
		default:
			job, hasJob := jobs[session.JobID]
			run, hasRun := runs[session.PhaseRunID]
			switch {
			case !hasJob:
				reason = "its Job no longer exists"
			case job.Status == domain.JobDone || job.Status == domain.JobCancelled:
				reason = "its Job is " + string(job.Status)
			case job.CurrentPhaseRunID != session.PhaseRunID:
				reason = "the Job moved past its step"
			case hasRun && run.Status == domain.PhaseRunPending && run.PendingReason == "ask":
				reason = "its step waits for an answer"
			case hasRun && run.Status != domain.PhaseRunQueued && run.Status != domain.PhaseRunRunning && run.Status != domain.PhaseRunPending:
				reason = "its step is " + string(run.Status)
			}
		}
		if reason == "" {
			continue
		}
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		_, err := s.stopCapsule(ctx, composition.ID, composition.Operator)
		cancel()
		if err != nil {
			s.logger.Warn("close idle capsule", "composition", composition.ID, "reason", reason, "error", err)
			continue
		}
		s.logger.Info("idle capsule closed", "composition", composition.ID, "session", composition.SessionID, "reason", reason)
	}
}

// launchContext bounds a launch by silence rather than by a clock: progress
// reports reach the browser and reset the stall timer, so a long transfer
// survives and a hung one does not.
func (s *Server) launchContext(ctx context.Context, sessionID string) (context.Context, context.CancelFunc) {
	ctx, cancel := context.WithCancel(ctx)
	var mu sync.Mutex
	timer := time.AfterFunc(launchStallTimeout, cancel)
	ctx = capsule.WithProgress(ctx, func(stage, message string, current, total int64) {
		mu.Lock()
		timer.Reset(launchStallTimeout)
		mu.Unlock()
		s.recordLaunchProgress(sessionID, launchProgress{Stage: stage, Message: message, Current: current, Total: total, UpdatedAt: time.Now().UTC()})
	})
	return ctx, func() { timer.Stop(); cancel() }
}

func (s *Server) recordLaunchProgress(sessionID string, progress launchProgress) {
	s.jobLaunchMu.Lock()
	if launch := s.jobLaunching[sessionID]; launch != nil {
		launch.progress = progress
	}
	s.jobLaunchMu.Unlock()
}

// pruneLaunchFailures forgets the failures of Sessions that no longer wait
// for a launch: their phase moved on (or another attempt already runs), so
// the old reason would only mislead.
func (s *Server) pruneLaunchFailures() {
	s.jobLaunchMu.Lock()
	sessionIDs := make([]string, 0, len(s.launchFailures))
	for sessionID := range s.launchFailures {
		sessionIDs = append(sessionIDs, sessionID)
	}
	s.jobLaunchMu.Unlock()
	if len(sessionIDs) == 0 {
		return
	}
	snapshot := s.store.Snapshot()
	stale := make([]string, 0, len(sessionIDs))
	for _, sessionID := range sessionIDs {
		index := slices.IndexFunc(snapshot.Sessions, func(session domain.Session) bool { return session.ID == sessionID })
		if index < 0 || !s.jobSessionNeedsLaunch(sessionID, snapshot.Sessions[index].PhaseRunID != "") {
			stale = append(stale, sessionID)
		}
	}
	s.jobLaunchMu.Lock()
	for _, sessionID := range stale {
		delete(s.launchFailures, sessionID)
	}
	s.jobLaunchMu.Unlock()
}

// recordLaunchFailure keeps why a launch gave up until the next attempt
// starts; a nil error clears it.
func (s *Server) recordLaunchFailure(sessionID string, err error) {
	s.jobLaunchMu.Lock()
	if err == nil {
		delete(s.launchFailures, sessionID)
	} else {
		s.launchFailures[sessionID] = launchFailure{Error: err.Error(), At: time.Now().UTC()}
	}
	s.jobLaunchMu.Unlock()
}

// workflowRunNeedsLaunch says whether a phase run wants a launch: it is
// queued, or it is running on paper while its agent Session has no live
// capsule (an answer came while every login was taken, a capsule that
// was lost); such a run is picked up again like a queued one.
func workflowRunNeedsLaunch(snapshot domain.Snapshot, session domain.Session, run domain.PhaseRun) bool {
	switch run.Status {
	case domain.PhaseRunQueued:
		return true
	case domain.PhaseRunRunning:
		if session.Executor == domain.WorkflowExecutorAction || session.Executor == domain.WorkflowExecutorExpose {
			return false
		}
		index := slices.IndexFunc(snapshot.Compositions, func(composition domain.Composition) bool { return composition.ID == session.PreparedCompositionID })
		return index < 0 || snapshot.Compositions[index].Runtime == nil || snapshot.Compositions[index].Runtime.Status == "stopped"
	}
	return false
}

// queuedWorkflowSessions returns the Sessions whose Job is parked on their own
// phase run while that run still wants a launch: work Spin already decided
// to start but never did, or lost its capsule.
func (s *Server) queuedWorkflowSessions() []domain.Session {
	snapshot := s.store.Snapshot()
	var queued []domain.Session
	for _, session := range snapshot.Sessions {
		if session.PhaseRunID == "" {
			continue
		}
		jobIndex := slices.IndexFunc(snapshot.Jobs, func(job domain.Job) bool {
			return job.ID == session.JobID && job.CurrentPhaseRunID == session.PhaseRunID
		})
		runIndex := slices.IndexFunc(snapshot.PhaseRuns, func(run domain.PhaseRun) bool { return run.ID == session.PhaseRunID })
		if jobIndex >= 0 && runIndex >= 0 && workflowRunNeedsLaunch(snapshot, session, snapshot.PhaseRuns[runIndex]) {
			queued = append(queued, session)
		}
	}
	return queued
}

// resumeQueuedWorkflowActions restarts queued action phases at boot. They need
// no runner, so a restart is the only thing that can have interrupted them.
func (s *Server) resumeQueuedWorkflowActions() {
	for _, session := range s.queuedWorkflowSessions() {
		if session.Executor == domain.WorkflowExecutorAction {
			s.startQueuedWorkflowLaunch(session)
		}
	}
}

// resumeQueuedWorkflowPhases restarts every queued phase once a runner has
// connected. Waiting for a runner is bounded, and a launch that ran out of
// patience leaves its phase queued with nothing left to look at it again, so
// the Job kept reporting "waiting for a runner" long after one came back.
func (s *Server) resumeQueuedWorkflowPhases() {
	s.launchQueuedWorkflowPhases("runner connected")
}

func (s *Server) launchQueuedWorkflowPhases(reason string) {
	for _, session := range s.queuedWorkflowSessions() {
		if s.startQueuedWorkflowLaunch(session) {
			s.logger.Info(reason+"; launching queued workflow phase",
				"session", session.ID, "job", session.JobID, "phase_run", session.PhaseRunID)
		}
	}
}

func (s *Server) startQueuedWorkflowLaunch(session domain.Session) bool {
	sessionID, operator := session.ID, session.Operator
	return s.beginTrackedLaunch(sessionID,
		func() bool { return s.jobSessionNeedsLaunch(sessionID, true) },
		func(ctx context.Context) { s.launchWorkflowSessionContext(ctx, sessionID, operator) })
}

// placementReporter is implemented by an engine that picks its runner before it
// does the slow part of materializing.
type placementReporter interface {
	OnPlacement(func(sessionID, clientID string))
}

// recordLaunchPlacement notes which runner took an in-flight launch, so the
// browser can name it while the workspace is still being built.
func (s *Server) recordLaunchPlacement(sessionID, clientID string) {
	s.jobLaunchMu.Lock()
	if launch := s.jobLaunching[sessionID]; launch != nil {
		launch.clientID = clientID
	}
	s.jobLaunchMu.Unlock()
}

func (s *Server) sessionPreparations() []sessionPreparation {
	s.pruneLaunchFailures()
	s.jobLaunchMu.Lock()
	preparations := make([]sessionPreparation, 0, len(s.jobLaunching))
	for sessionID, launch := range s.jobLaunching {
		preparation := sessionPreparation{SessionID: sessionID, ClientID: launch.clientID, StartedAt: launch.startedAt}
		if launch.progress.Stage != "" {
			progress := launch.progress
			preparation.Progress = &progress
		}
		preparations = append(preparations, preparation)
	}
	for sessionID, failure := range s.launchFailures {
		if s.jobLaunching[sessionID] != nil {
			continue
		}
		failure := failure
		preparations = append(preparations, sessionPreparation{SessionID: sessionID, StartedAt: failure.At, Failure: &failure})
	}
	s.jobLaunchMu.Unlock()
	slices.SortFunc(preparations, func(a, b sessionPreparation) int { return strings.Compare(a.SessionID, b.SessionID) })
	return preparations
}

// beginTrackedLaunch runs body as the tracked background launch for a Session
// and reports whether it started one. Guard runs while the bookkeeping lock is
// held, so a concurrent launch cannot slip between its verdict and the
// registration that makes this launch visible.
func (s *Server) beginTrackedLaunch(sessionID string, guard func() bool, body func(context.Context)) bool {
	s.jobLaunchMu.Lock()
	if s.jobLaunching[sessionID] != nil {
		s.jobLaunchMu.Unlock()
		return false
	}
	if guard != nil && !guard() {
		s.jobLaunchMu.Unlock()
		return false
	}
	launchContext, cancel := context.WithCancel(context.Background())
	launch := &backgroundJobLaunch{cancel: cancel, done: make(chan struct{}), startedAt: time.Now().UTC()}
	s.jobLaunching[sessionID] = launch
	delete(s.launchFailures, sessionID)
	s.jobLaunchMu.Unlock()
	go s.runTrackedLaunch(sessionID, launch, func() { body(launchContext) })
	return true
}

// runTrackedLaunch releases bookkeeping for both a first launch and a retry.
// A superseded launch must finish without removing the retry that replaced it.
func (s *Server) runTrackedLaunch(sessionID string, launch *backgroundJobLaunch, body func()) {
	defer func() {
		launch.cancel()
		s.jobLaunchMu.Lock()
		if s.jobLaunching[sessionID] == launch {
			delete(s.jobLaunching, sessionID)
		}
		s.jobLaunchMu.Unlock()
		close(launch.done)
	}()
	body()
}

func (s *Server) scheduleJobLaunch(created domain.CreateJobResponse, requestedOperator string) {
	sessionID := created.Session.ID
	operator := normalizeOperator(requestedOperator)
	if operator == "" {
		operator = normalizeOperator(created.Session.Operator)
	}
	if sessionID == "" || operator == "" {
		return
	}
	workflow := created.Job.TemplateID != ""
	s.beginTrackedLaunch(sessionID,
		func() bool { return s.jobSessionNeedsLaunch(sessionID, workflow) },
		func(ctx context.Context) {
			if workflow {
				s.launchWorkflowSessionContext(ctx, sessionID, operator)
				return
			}
			materializeContext, materializeCancel := s.launchContext(ctx, sessionID)
			defer materializeCancel()
			if _, err := s.useCapsule(materializeContext, domain.UseRequest{Selector: "session:" + sessionID, Operator: operator}); err != nil && !errors.Is(err, context.Canceled) {
				s.logger.Warn("start queued Job Session", "session", sessionID, "error", err)
				s.recordLaunchFailure(sessionID, err)
			}
		})
}

// scheduleWorkflowRetry replaces any in-flight launch for this Session. It is
// independent of the request context so a temporary browser disconnect does
// not cancel runtime cleanup or the relaunch.
func (s *Server) scheduleWorkflowRetry(created domain.CreateJobResponse, requestedOperator, previousCompositionID string) {
	sessionID := created.Session.ID
	operator := normalizeOperator(requestedOperator)
	if operator == "" {
		operator = normalizeOperator(created.Session.Operator)
	}
	if sessionID == "" || operator == "" {
		return
	}

	s.jobLaunchMu.Lock()
	previous := s.jobLaunching[sessionID]
	if previous != nil {
		previous.cancel()
	}
	launchContext, cancel := context.WithCancel(context.Background())
	launch := &backgroundJobLaunch{cancel: cancel, done: make(chan struct{}), startedAt: time.Now().UTC()}
	s.jobLaunching[sessionID] = launch
	delete(s.launchFailures, sessionID)
	s.jobLaunchMu.Unlock()

	go s.runTrackedLaunch(sessionID, launch, func() {
		if previous != nil {
			select {
			case <-previous.done:
			case <-launchContext.Done():
				return
			}
		}
		if launchContext.Err() != nil {
			return
		}
		if previousCompositionID != "" {
			s.stopACPComposition(previousCompositionID)
			// Cleanup retains affinity to the old runner but is deliberately not a
			// prerequisite for Retry. The newly connected runner becomes
			// authoritative; an old runner that returns later only receives cleanup.
			go func() {
				cleanupContext, cleanupCancel := context.WithTimeout(context.Background(), 30*time.Second)
				defer cleanupCancel()
				_, stopErr := s.stopCapsule(cleanupContext, previousCompositionID, operator)
				if stopErr != nil && !errors.Is(stopErr, store.ErrNotFound) && !errors.Is(stopErr, context.Canceled) {
					s.logger.Warn("clean previous workflow Session after retry", "session", sessionID, "composition", previousCompositionID, "error", stopErr)
				}
			}()
		}
		if launchContext.Err() == nil {
			s.launchWorkflowSessionContext(launchContext, sessionID, operator)
		}
	})
}

func (s *Server) cancelJobLaunch(ctx context.Context, sessionID string) error {
	s.jobLaunchMu.Lock()
	launch := s.jobLaunching[sessionID]
	if launch != nil {
		launch.cancel()
	}
	s.jobLaunchMu.Unlock()
	if launch == nil {
		return nil
	}
	select {
	case <-launch.done:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func (s *Server) jobSessionNeedsLaunch(sessionID string, workflow bool) bool {
	snapshot := s.store.Snapshot()
	sessionIndex := slices.IndexFunc(snapshot.Sessions, func(session domain.Session) bool { return session.ID == sessionID })
	if sessionIndex < 0 {
		return false
	}
	session := snapshot.Sessions[sessionIndex]
	if workflow {
		runIndex := slices.IndexFunc(snapshot.PhaseRuns, func(run domain.PhaseRun) bool { return run.ID == session.PhaseRunID })
		return runIndex >= 0 && workflowRunNeedsLaunch(snapshot, session, snapshot.PhaseRuns[runIndex])
	}
	if session.PreparedCompositionID == "" {
		return true
	}
	compositionIndex := slices.IndexFunc(snapshot.Compositions, func(composition domain.Composition) bool { return composition.ID == session.PreparedCompositionID })
	return compositionIndex < 0 || snapshot.Compositions[compositionIndex].Runtime == nil || snapshot.Compositions[compositionIndex].Runtime.Status == "stopped"
}
