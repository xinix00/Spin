package server

import (
	"context"
	"easyacp/internal/buildinfo"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"net/http"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/orchestrator"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
	"easyacp/internal/worker"
	"github.com/gorilla/websocket"
)

type Server struct {
	inflight        inflight
	store           *store.Store
	logger          *slog.Logger
	mux             *http.ServeMux
	engine          capsule.Engine
	httpClient      *http.Client
	gitOAuth        *gitOAuthManager
	authDisabled    bool
	workerToken     string
	runnerBroker    *worker.Broker
	internalURL     string
	attachments     AttachmentStorage
	snapshotArchive capsule.SnapshotArchive
	database        *persistence.SQLite
	replica         ReplicaStatus
	loginLimiter    loginLimiter
	csrfTokens      csrfTokenCache
	terminalMu      sync.Mutex
	terminals       map[string]map[*activeTerminal]struct{}
	acpMu           sync.Mutex
	acpSessions     map[string]*activeACP
	workflowMu      sync.Mutex
	workflowTokens  map[string]string
	jobLaunchMu     sync.Mutex
	jobLaunching    map[string]*backgroundJobLaunch
	launchFailures  map[string]launchFailure // why the last launch of a queued Session gave up
	launchSweep     time.Duration            // cadence at which queued phases are offered a launch again
	backupMu        sync.Mutex
	paused          atomic.Bool // writes wait while a backup streams
	workspaceSyncs  workspaceSyncs
	backupTicketMu  sync.Mutex
	backupTickets   map[string]backupTicket
	uploadMu        sync.Mutex
	uploads         map[string]*chunkedUpload
	sealMu          sync.Mutex
	seals           map[string]*sealJob
	sealWait        time.Duration // how long End & save waits before answering with progress
	startMu         sync.Mutex
	starts          map[string]*startJob
	startWait       time.Duration // how long a recording and a new version wait before answering with progress
	startCancelWait time.Duration // how long a cancel waits for a stopped start job
	appMu           sync.Mutex
	appStarts       map[string]*appStart // app service starts per Session
	restoreJobMu    sync.Mutex
	restoreJobs     map[string]*restoreJob
}

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

func New(st *store.Store, logger *slog.Logger) *Server {
	return NewWithEngine(st, logger, capsule.Journal{})
}

func NewWithEngine(st *store.Store, logger *slog.Logger, engine capsule.Engine) *Server {
	return NewWithOptions(st, logger, engine, ServerOptionsFromEnvironment())
}

func NewWithOptions(st *store.Store, logger *slog.Logger, engine capsule.Engine, options ServerOptions) *Server {
	if logger == nil {
		logger = slog.Default()
	}
	if engine == nil {
		engine = capsule.Journal{}
	}
	httpClient := options.HTTPClient
	if httpClient == nil {
		httpClient = &http.Client{Timeout: 20 * time.Second}
	}
	attachmentStorage := options.AttachmentStorage
	if attachmentStorage == nil {
		attachmentStorage = newFilesystemAttachmentStorage(options.AttachmentDir)
	}
	s := &Server{
		store: st, logger: logger, mux: http.NewServeMux(), engine: engine, httpClient: httpClient,
		authDisabled: options.DisableAuthentication, workerToken: strings.TrimSpace(options.WorkerToken), replica: options.Replica,
		runnerBroker: options.RunnerBroker,
		internalURL:  strings.TrimRight(strings.TrimSpace(options.InternalURL), "/"),
		attachments:  attachmentStorage, snapshotArchive: options.SnapshotArchive, database: options.Database,
		loginLimiter: loginLimiter{attempts: map[string]loginAttempt{}}, csrfTokens: csrfTokenCache{values: map[string]string{}},
		terminals: map[string]map[*activeTerminal]struct{}{}, acpSessions: map[string]*activeACP{}, workflowTokens: map[string]string{}, jobLaunching: map[string]*backgroundJobLaunch{}, launchFailures: map[string]launchFailure{}, launchSweep: launchSweepInterval, backupTickets: map[string]backupTicket{}, uploads: map[string]*chunkedUpload{}, seals: map[string]*sealJob{}, sealWait: sealAnswerWait, starts: map[string]*startJob{}, startWait: startAnswerWait, startCancelWait: startCancelWait, restoreJobs: map[string]*restoreJob{}, appStarts: map[string]*appStart{},
	}
	if restored, err := st.RepairStandingDecisions(); err != nil {
		logger.Warn("repair standing workflow decisions", "error", err)
	} else if restored > 0 {
		logger.Info("restored workflow decisions that a chat had closed", "count", restored)
	}
	s.gitOAuth = newGitOAuthManager(options, st)
	s.routes()
	if s.runnerBroker != nil {
		s.runnerBroker.OnRunnerConnected(s.resumeQueuedWorkflowPhases)
	}
	if reporter, ok := engine.(placementReporter); ok {
		reporter.OnPlacement(s.recordLaunchPlacement)
	}
	go s.resumeQueuedWorkflowActions()
	s.resumeStartingRecordings()
	s.pruneLater()
	s.cleanTemporaryFiles()
	s.pruneClients()
	go s.sweepQueuedWorkflowPhases()
	return s
}

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
	for _, composition := range snapshot.Compositions {
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
	go func() {
		defer func() {
			cancel()
			s.jobLaunchMu.Lock()
			if s.jobLaunching[sessionID] == launch {
				delete(s.jobLaunching, sessionID)
			}
			s.jobLaunchMu.Unlock()
			close(launch.done)
		}()
		body(launchContext)
	}()
	return true
}

func (s *Server) Handler() http.Handler {
	return s.logging(s.securityHeaders(s.authentication(s.mux)))
}

func (s *Server) securityHeaders(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("X-Content-Type-Options", "nosniff")
		w.Header().Set("X-Frame-Options", "DENY")
		w.Header().Set("Referrer-Policy", "no-referrer")
		w.Header().Set("Permissions-Policy", "camera=(), microphone=(), geolocation=()")
		w.Header().Set("Content-Security-Policy", "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; object-src 'none'; img-src 'self' data:; media-src 'self' data:; font-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; connect-src 'self'")
		if strings.HasPrefix(r.URL.Path, "/api/") {
			preventCaching(w)
		}
		next.ServeHTTP(w, r)
	})
}

func preventCaching(w http.ResponseWriter) {
	w.Header().Set("Cache-Control", "no-store, no-cache, must-revalidate, max-age=0")
	w.Header().Set("CDN-Cache-Control", "no-store")
	w.Header().Set("Cloudflare-CDN-Cache-Control", "no-store")
	w.Header().Set("Surrogate-Control", "no-store")
	w.Header().Set("Pragma", "no-cache")
	w.Header().Set("Expires", "0")
}

func (s *Server) routes() {
	s.mux.HandleFunc("GET /", s.dashboard)
	assetFiles, err := fs.Sub(dashboardAssets, "assets")
	if err != nil {
		panic(fmt.Sprintf("prepare embedded dashboard assets: %v", err))
	}
	assetPrefix := "/assets/v" + frontendAssetVersion + "/"
	assets := http.FileServer(http.FS(assetFiles))
	s.mux.Handle("GET /assets/", http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		assetPath := strings.TrimPrefix(r.URL.Path, "/assets/")
		requestedPrefix := "/assets/"
		if separator := strings.IndexByte(assetPath, '/'); separator > 1 && assetPath[0] == 'v' {
			version := assetPath[1:separator]
			if strings.IndexFunc(version, func(character rune) bool { return character < '0' || character > '9' }) == -1 {
				requestedPrefix += assetPath[:separator+1]
				assetPath = assetPath[separator+1:]
			}
		}
		if requestedPrefix == assetPrefix {
			w.Header().Set("Cache-Control", "public, max-age=31536000, immutable")
		} else {
			// Keep an already-open or edge-cached older dashboard functional during
			// rolling deploys, without making fallback responses immutable.
			preventCaching(w)
		}
		request := r.Clone(r.Context())
		request.URL.Path = "/" + assetPath
		assets.ServeHTTP(w, request)
	}))
	s.mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, http.StatusOK, map[string]any{"status": "ok", "version": buildinfo.Version, "commit": buildinfo.Commit, "storage": s.storageInfo(r.Context())})
	})
	if s.runnerBroker != nil {
		s.mux.HandleFunc("GET /api/runner/ws", s.runnerBroker.Handler)
	}
	s.mux.HandleFunc("GET /api/auth/status", s.authStatus)
	s.mux.HandleFunc("POST /api/auth/setup", s.setupOwner)
	s.mux.HandleFunc("POST /api/auth/login", s.login)
	s.mux.HandleFunc("POST /api/auth/logout", s.logout)
	s.mux.HandleFunc("POST /api/auth/users", s.createUser)
	s.mux.HandleFunc("POST /api/auth/users/{userID}/archive", s.archiveUser)
	s.mux.HandleFunc("POST /api/auth/users/{userID}/restore", s.restoreUser)
	s.mux.HandleFunc("POST /api/auth/users/{userID}/password", s.resetUserPassword)
	s.mux.HandleFunc("POST /api/backup", s.downloadBackup)
	s.mux.HandleFunc("POST /api/backup-ticket", s.createBackupTicket)
	s.mux.HandleFunc("GET /api/backup", s.downloadBackupWithTicket)
	s.mux.HandleFunc("POST /api/restore", s.restoreBackup)
	s.mux.HandleFunc("GET /api/snapshots/{digest}", s.snapshotChunkHandler)
	s.mux.HandleFunc("POST /api/uploads", s.createUpload)
	s.mux.HandleFunc("GET /api/uploads/{uploadID}", s.getUpload)
	s.mux.HandleFunc("PUT /api/uploads/{uploadID}", s.appendUpload)
	s.mux.HandleFunc("DELETE /api/uploads/{uploadID}", s.deleteUpload)
	s.mux.HandleFunc("POST /api/uploads/{uploadID}/complete", s.completeUpload)
	s.mux.HandleFunc("GET /api/restores/{restoreID}", s.getRestoreJob)
	s.mux.HandleFunc("GET /api/replica/points", s.listReplicaPoints)
	s.mux.HandleFunc("POST /api/replica/restore", s.restoreReplicaPoint)
	s.mux.HandleFunc("GET /api/state", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, http.StatusOK, s.stateFor(r))
	})
	s.mux.HandleFunc("GET /api/state/ws", s.stateStream)
	s.mux.HandleFunc("GET /api/storage", s.storageHandler)
	s.mux.HandleFunc("GET /api/artifacts", s.listArtifacts)
	s.mux.HandleFunc("DELETE /api/artifacts/{artifactID}", s.deleteArtifact)
	s.mux.HandleFunc("POST /api/artifacts/{artifactID}/acp/options", s.fetchAgentOptionsHandler)
	s.mux.HandleFunc("PUT /api/artifacts/{artifactID}/acp/settings", s.setAgentSettingsHandler)
	s.mux.HandleFunc("PUT /api/artifacts/{artifactID}/enablements/{name}", s.setEnablementCommandHandler)
	s.mux.HandleFunc("POST /api/artifacts/{artifactID}/edit", s.editArtifact)
	s.mux.HandleFunc("POST /api/recordings", s.createRecording)
	s.mux.HandleFunc("POST /api/recordings/{recordingID}/commands", s.appendRecordingCommand)
	s.mux.HandleFunc("GET /api/recordings/{recordingID}/terminal", s.recordingTerminal)
	s.mux.HandleFunc("GET /api/compositions/{compositionID}/terminal", s.compositionTerminal)
	s.mux.HandleFunc("POST /api/recordings/{recordingID}/parents", s.attachRecordingParent)
	s.mux.HandleFunc("POST /api/recordings/{recordingID}/end", s.endRecording)
	s.mux.HandleFunc("GET /api/recordings/{recordingID}/seal", s.getSeal)
	s.mux.HandleFunc("GET /api/recordings/{recordingID}/start", s.getStart)
	s.mux.HandleFunc("POST /api/recordings/{recordingID}/cancel", s.cancelRecording)
	s.mux.HandleFunc("POST /api/use", s.useArtifacts)
	s.mux.HandleFunc("POST /api/compositions/{compositionID}/acp/probe", s.probeACPHandler)
	s.mux.HandleFunc("POST /api/compositions/{compositionID}/stop", s.stopComposition)
	s.mux.HandleFunc("POST /api/jobs", s.createJob)
	s.mux.HandleFunc("POST /api/job-attachments", s.uploadStagedJobAttachment)
	s.mux.HandleFunc("GET /api/job-attachments/{attachmentID}", s.downloadJobAttachment)
	s.mux.HandleFunc("DELETE /api/job-attachments/{attachmentID}", s.deleteStagedJobAttachment)
	s.mux.HandleFunc("POST /api/jobs/{jobID}/attachments", s.uploadJobAttachment)
	s.mux.HandleFunc("GET /api/jobs/{jobID}/changes", s.jobChanges)
	s.mux.HandleFunc("GET /api/git/repositories/{repositoryID}/code/{mode}", s.exploreHandler)
	s.mux.HandleFunc("POST /api/jobs/{jobID}/code-reviews", s.createCodeReview)
	s.mux.HandleFunc("GET /api/code-reviews/{revisionID}", s.getCodeReview)
	s.mux.HandleFunc("POST /api/code-reviews/{revisionID}/comments", s.createCodeReviewComment)
	s.mux.HandleFunc("POST /api/jobs/{jobID}/close", s.closeJob)
	s.mux.HandleFunc("PUT /api/jobs/{jobID}/assignee", s.assignJob)
	s.mux.HandleFunc("PUT /api/jobs/{jobID}/environment", s.updateJobEnvironment)
	s.mux.HandleFunc("POST /api/jobs/{jobID}/template", s.adoptJobTemplate)
	s.mux.HandleFunc("DELETE /api/jobs/{jobID}", s.deleteJob)
	s.mux.HandleFunc("POST /api/deliverables/{deliverableID}/comments", s.createDeliverableComment)
	s.mux.HandleFunc("GET /api/deliverables/{deliverableID}/download", s.downloadDeliverable)
	s.mux.HandleFunc("GET /preview/{deliverableID}/{file...}", s.previewDeliverable)
	s.mux.HandleFunc("GET /api/blobs/{ref}", s.blobChunkHandler)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/retry", s.retryWorkflowSession)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/app/start", s.startAppHandler)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/app/stop", s.stopAppHandler)
	s.mux.HandleFunc("GET /api/sessions/{sessionID}/app", s.appStatusHandler)
	s.mux.HandleFunc("GET /api/sessions/{sessionID}/app/{service}/logs", s.appLogsHandler)
	s.mux.HandleFunc("POST /api/workflow-templates", s.createWorkflowTemplate)
	s.mux.HandleFunc("PUT /api/workflow-templates/{templateID}", s.updateWorkflowTemplate)
	s.mux.HandleFunc("DELETE /api/workflow-templates/{templateID}", s.deleteWorkflowTemplate)
	s.mux.HandleFunc("POST /api/workflow/questions/{questionID}/answer", s.answerWorkflowQuestion)
	s.mux.HandleFunc("POST /api/workflow/mcp/{sessionID}", s.workflowMCP)
	s.mux.HandleFunc("POST /api/jobs/{jobID}/sessions", s.createJobSession)
	s.mux.HandleFunc("POST /api/jobs/{jobID}/select-result", s.selectResult)
	s.mux.HandleFunc("POST /api/mcp-servers", s.createMCPServer)
	s.mux.HandleFunc("DELETE /api/mcp-servers/{mcpServerID}", s.deleteMCPServer)
	s.mux.HandleFunc("POST /api/git/repositories", s.createGitRepository)
	s.mux.HandleFunc("PUT /api/git/repositories/{repositoryID}", s.updateGitRepository)
	s.mux.HandleFunc("DELETE /api/git/repositories/{repositoryID}", s.deleteGitRepository)
	s.mux.HandleFunc("POST /api/git/accounts", s.createGitAccount)
	s.mux.HandleFunc("DELETE /api/git/accounts/{accountID}", s.deleteGitAccount)
	s.mux.HandleFunc("GET /api/git/oauth/{provider}/start", s.startGitOAuth)
	s.mux.HandleFunc("GET /api/git/oauth/{provider}/callback", s.finishGitOAuth)
	s.mux.HandleFunc("PUT /api/git/oauth/{provider}/configuration", s.saveGitOAuthConfiguration)
	s.mux.HandleFunc("DELETE /api/git/oauth/{provider}/configuration", s.deleteGitOAuthConfiguration)
	s.mux.HandleFunc("POST /api/clients/register", s.registerClient)
	s.mux.HandleFunc("POST /api/clients/{clientID}/drain", s.drainClient)
	s.mux.HandleFunc("DELETE /api/clients/{clientID}", s.removeClient)
	s.mux.HandleFunc("GET /api/artifacts/{artifactID}/contents", s.artifactContentsHandler)
	s.mux.HandleFunc("GET /api/artifacts/{artifactID}/tree", s.artifactTreeHandler)
	s.mux.HandleFunc("PUT /api/artifacts/{artifactID}/tracked", s.setTrackedPathsHandler)
	s.mux.HandleFunc("POST /api/compositions/{compositionID}/login", s.saveLoginHandler)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/capsule", s.restartSessionCapsule)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/capsule/stop", s.stopSessionCapsule)
	s.mux.HandleFunc("GET /api/sessions/{sessionID}/file", s.sessionFileHandler)
	s.mux.HandleFunc("DELETE /api/logins/{loginID}", s.deleteLoginHandler)
	s.mux.HandleFunc("GET /api/logins/{loginID}/files", s.loginFilesHandler)
	s.mux.HandleFunc("POST /api/artifacts/{artifactID}/tracked/exclude", s.excludeLoginPathHandler)
	s.mux.HandleFunc("GET /api/compositions/{compositionID}/changes", s.compositionChangesHandler)
	s.mux.HandleFunc("GET /api/runners/token", s.workerTokenHandler)
	s.mux.HandleFunc("POST /api/runners/token", s.workerTokenHandler)
	s.mux.HandleFunc("POST /api/clients/{clientID}/resume", s.resumeClient)
	s.mux.HandleFunc("POST /api/sessions/claim", s.claim)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/start", s.startSession)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/turns", s.startTurn)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/checkpoints", s.createCheckpoint)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/result", s.createResult)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/fork", s.forkSession)
	s.mux.HandleFunc("GET /api/sessions/{sessionID}/acp", s.sessionACP)
	s.mux.HandleFunc("GET /api/sessions/{sessionID}/changes", s.sessionChanges)
	s.mux.HandleFunc("POST /api/activations/{activationID}/heartbeat", s.heartbeat)
}

// stateFor is the state one browser sees: the store as visible to the
// signed-in user plus what only lives in memory (launches under way).
func (s *Server) stateFor(r *http.Request) stateResponse {
	identity, _ := identityFromRequest(r)
	snapshot := s.store.Snapshot()
	if !s.authDisabled {
		snapshot = visibleSnapshot(snapshot, identity.User.Username)
	}
	recommendations := orchestrator.Recommend(snapshot)
	if recommendations == nil {
		recommendations = []domain.Recommendation{}
	}
	return stateResponse{Snapshot: snapshot, Recommendations: recommendations, Engine: s.engine.Info(), GitOAuthProviders: s.gitOAuth.publicProviders(r), CurrentUser: publicUser(identity.User), Preparing: s.sessionPreparations(), Storage: s.storageInfo(r.Context()), Version: s.store.Version()}
}

// stateStream pushes the state over a WebSocket: the whole of it on
// connect, again whenever the store changes (saves within a short window
// collapse into one message), and every few seconds while something only
// in memory is moving (a launch, a seal, a fetch). The browser never asks.
func (s *Server) stateStream(w http.ResponseWriter, r *http.Request) {
	connection, err := terminalUpgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	defer connection.Close()
	connection.SetReadLimit(1 << 20)
	ticks, stop := s.store.Watch()
	defer stop()
	closed := make(chan struct{})
	go func() {
		defer close(closed)
		for {
			if _, _, err := connection.ReadMessage(); err != nil {
				return
			}
		}
	}()
	send := func() bool {
		connection.SetWriteDeadline(time.Now().Add(20 * time.Second))
		return connection.WriteJSON(s.stateFor(r)) == nil
	}
	if !send() {
		return
	}
	transient := time.NewTicker(3 * time.Second)
	defer transient.Stop()
	keepalive := time.NewTicker(acpKeepaliveInterval)
	defer keepalive.Stop()
	var pending <-chan time.Time
	for {
		select {
		case <-closed:
			return
		case <-ticks:
			if pending == nil {
				pending = time.After(150 * time.Millisecond)
			}
		case <-pending:
			pending = nil
			if !send() {
				return
			}
		case <-transient.C:
			if s.hasTransientWork() && !send() {
				return
			}
		case <-keepalive.C:
			if err := connection.WriteControl(websocket.PingMessage, nil, time.Now().Add(5*time.Second)); err != nil {
				return
			}
		}
	}
}

// hasTransientWork reports whether something changes without a store save:
// launches, seals, starts, option fetches and app starts in progress.
func (s *Server) hasTransientWork() bool {
	s.jobLaunchMu.Lock()
	launching := len(s.jobLaunching) > 0
	s.jobLaunchMu.Unlock()
	if launching {
		return true
	}
	s.sealMu.Lock()
	sealing := len(s.seals) > 0
	s.sealMu.Unlock()
	s.startMu.Lock()
	starting := len(s.starts) > 0
	s.startMu.Unlock()
	s.appMu.Lock()
	apps := len(s.appStarts) > 0
	s.appMu.Unlock()
	return sealing || starting || apps
}

type stateResponse struct {
	domain.Snapshot
	Recommendations   []domain.Recommendation  `json:"recommendations"`
	Engine            domain.CapsuleEngineInfo `json:"engine"`
	GitOAuthProviders []gitOAuthProviderInfo   `json:"git_oauth_providers"`
	CurrentUser       domain.PublicUser        `json:"current_user"`
	Preparing         []sessionPreparation     `json:"preparing"`
	Storage           storageInfo              `json:"storage"`
	Version           uint64                   `json:"version"`
}

func (s *Server) drainClient(w http.ResponseWriter, r *http.Request) {
	s.setClientDraining(w, r, true)
}

func (s *Server) resumeClient(w http.ResponseWriter, r *http.Request) {
	s.setClientDraining(w, r, false)
}

func (s *Server) setClientDraining(w http.ResponseWriter, r *http.Request, draining bool) {
	identity, ok := identityFromRequest(r)
	if !s.authDisabled && (!ok || identity.User.Role != domain.UserAdmin) {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "admin role required"})
		return
	}
	if s.runnerBroker == nil {
		writeError(w, fmt.Errorf("runner broker is not configured: %w", store.ErrConflict))
		return
	}
	client, err := s.runnerBroker.SetDraining(r.PathValue("clientID"), draining)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, client)
}

// removeClient forgets an offline runner nothing hangs on.
func (s *Server) removeClient(w http.ResponseWriter, r *http.Request) {
	identity, ok := identityFromRequest(r)
	if !s.authDisabled && (!ok || identity.User.Role != domain.UserAdmin) {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "admin role required"})
		return
	}
	clientID := r.PathValue("clientID")
	if s.runnerBroker != nil {
		if err := s.runnerBroker.Forget(clientID); err != nil {
			writeError(w, err)
			return
		}
	}
	if err := s.store.RemoveClient(clientID); err != nil {
		writeError(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// pruneClients forgets runners offline for a day that nothing hangs on:
// every redeploy of a stateless runner used to leave a card behind.
func (s *Server) pruneClients() {
	removed, err := s.store.PruneClients(24 * time.Hour)
	if err != nil {
		s.logger.Warn("prune offline runners", "error", err)
		return
	}
	for _, id := range removed {
		if s.runnerBroker != nil {
			_ = s.runnerBroker.Forget(id)
		}
		s.logger.Info("forgot offline runner", "client", id)
	}
}

func (s *Server) dashboard(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("X-Spin-UI-Version", frontendAssetVersion)
	preventCaching(w)
	_, _ = w.Write(dashboardDocument)
}

func (s *Server) listArtifacts(w http.ResponseWriter, r *http.Request) {
	kind := r.URL.Query().Get("kind")
	snapshot := s.store.Snapshot()
	if !s.authDisabled {
		snapshot = visibleSnapshot(snapshot, s.requestOperator(r, ""))
	}
	artifacts := snapshot.Artifacts
	if kind != "" {
		filtered := artifacts[:0]
		for _, artifact := range artifacts {
			if string(artifact.Kind) == kind {
				filtered = append(filtered, artifact)
			}
		}
		artifacts = filtered
	}
	writeJSON(w, http.StatusOK, artifacts)
}

func visibleSnapshot(snapshot domain.Snapshot, operator string) domain.Snapshot {
	operator = normalizeOperator(operator)
	snapshot.Artifacts = filterSlice(snapshot.Artifacts, func(artifact domain.Artifact) bool {
		return artifact.Scope != domain.ScopeUser || artifact.Subject == operator
	})
	snapshot.Recordings = filterSlice(snapshot.Recordings, func(recording domain.Recording) bool {
		return recording.Actor == operator
	})
	snapshot.MCPServers = filterSlice(snapshot.MCPServers, func(server domain.MCPServer) bool {
		return server.Operator == operator
	})
	snapshot.GitAccounts = filterSlice(snapshot.GitAccounts, func(account domain.GitAccount) bool {
		return account.CredentialScope == domain.CredentialScopeGlobal || account.Operator == operator
	})
	return snapshot
}

func filterSlice[T any](items []T, keep func(T) bool) []T {
	filtered := make([]T, 0, len(items))
	for _, item := range items {
		if keep(item) {
			filtered = append(filtered, item)
		}
	}
	return filtered
}

func (s *Server) deleteArtifact(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	identity, ok := identityFromRequest(r)
	admin := s.authDisabled || (ok && identity.User.Role == domain.UserAdmin)
	artifact, err := s.store.Artifact(r.PathValue("artifactID"))
	if err != nil {
		writeError(w, err)
		return
	}
	// Whatever still runs on the layer or on a layer above it stops first:
	// capsules of Sessions and open recordings of anyone.
	tree := s.store.ArtifactTree(artifact.ID)
	inTree := map[string]bool{}
	for _, member := range tree {
		inTree[member.ID] = true
	}
	snapshot := s.store.Snapshot()
	for _, composition := range snapshot.Compositions {
		if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
			continue
		}
		uses := false
		for _, layerID := range capsule.CompositionLayers(composition) {
			if inTree[layerID] {
				uses = true
				break
			}
		}
		if uses {
			if _, err := s.stopCapsule(r.Context(), composition.ID, composition.Operator); err != nil {
				writeError(w, fmt.Errorf("stop composition %s before removing the layer: %w", composition.ID, err))
				return
			}
		}
	}
	for _, recording := range snapshot.Recordings {
		if recording.Status != domain.RecordingOpen {
			continue
		}
		for _, parentID := range recording.ParentArtifactIDs {
			if inTree[parentID] {
				if _, err := s.cancelCapsuleRecording(r.Context(), recording.ID, domain.CancelRecordingRequest{Actor: recording.Actor}); err != nil {
					writeError(w, fmt.Errorf("cancel recording %s before removing the layer: %w", recording.ID, err))
					return
				}
				break
			}
		}
	}
	if _, err := s.store.PrepareArtifactDeletion(artifact.ID, operator, admin); err != nil {
		writeError(w, err)
		return
	}
	// The top layers go first, then their parents: a runner rebuilds a
	// delta from its parent, never the other way round.
	for _, member := range tree {
		if remover, ok := s.engine.(capsule.SnapshotRemover); ok {
			if err := remover.RemoveSnapshot(r.Context(), member.Snapshot); err != nil {
				writeError(w, fmt.Errorf("remove snapshot of %s:%s: %w", member.Kind, member.Name, err))
				return
			}
		}
		if s.snapshotArchive != nil && member.SnapshotPrunedAt == nil && member.Snapshot.Digest != "" {
			if err := s.snapshotArchive.RemoveArchivedSnapshot(r.Context(), member.Snapshot); err != nil {
				s.logger.Warn("remove archived snapshot", "artifact", member.ID, "error", err)
			}
		}
	}
	deleted, err := s.store.DeleteArtifactTree(artifact.ID, operator, admin)
	if err != nil {
		writeError(w, err)
		return
	}
	if files := s.manifestFiles(); files != nil {
		for _, member := range deleted {
			_ = files.Remove("artifact:" + member.ID)
		}
	}
	s.logger.Info("layers removed", "root", string(artifact.Kind)+":"+artifact.Name, "layers", len(deleted), "by", operator)
	writeJSON(w, http.StatusOK, deleted[len(deleted)-1])
}

func (s *Server) createRecording(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateRecordingRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Actor = s.requestOperator(r, req.Actor)
	recording, start, err := s.createCapsuleRecording(req)
	if err != nil {
		writeError(w, err)
		return
	}
	if start != nil {
		w.Header().Set("Location", "/api/recordings/"+recording.ID+"/start")
		writeJSON(w, http.StatusAccepted, start)
		return
	}
	writeJSON(w, http.StatusCreated, recording)
}

// editArtifact starts a new version of a layer; like createRecording it
// answers with the start job when the capsule takes a moment.
func (s *Server) editArtifact(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Operator string `json:"operator"`
	}
	if r.ContentLength != 0 && !decodeJSON(w, r, &req) {
		return
	}
	actor := s.requestOperator(r, req.Operator)
	recording, start, err := s.editCapsuleArtifact(actor, r.PathValue("artifactID"))
	if err != nil {
		writeError(w, err)
		return
	}
	if start != nil {
		w.Header().Set("Location", "/api/recordings/"+recording.ID+"/start")
		writeJSON(w, http.StatusAccepted, start)
		return
	}
	writeJSON(w, http.StatusCreated, recording)
}

func (s *Server) appendRecordingCommand(w http.ResponseWriter, r *http.Request) {
	var req domain.ExecuteRecordingCommandRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Actor = s.requestOperator(r, req.Actor)
	recording, _, err := s.executeRecordingCommand(r.Context(), r.PathValue("recordingID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, recording)
}

func (s *Server) attachRecordingParent(w http.ResponseWriter, r *http.Request) {
	var req domain.AttachRecordingParentRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Actor = s.requestOperator(r, req.Actor)
	recording, err := s.attachCapsuleParent(r.Context(), r.PathValue("recordingID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, recording)
}

func (s *Server) endRecording(w http.ResponseWriter, r *http.Request) {
	var req domain.EndRecordingRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Actor = s.requestOperator(r, req.Actor)
	artifact, seal, err := s.endCapsuleRecording(r.PathValue("recordingID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	if seal != nil {
		w.Header().Set("Location", "/api/recordings/"+seal.RecordingID+"/seal")
		writeJSON(w, http.StatusAccepted, seal)
		return
	}
	writeJSON(w, http.StatusCreated, artifact)
}

func (s *Server) cancelRecording(w http.ResponseWriter, r *http.Request) {
	var req domain.CancelRecordingRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Actor = s.requestOperator(r, req.Actor)
	recording, err := s.cancelCapsuleRecording(r.Context(), r.PathValue("recordingID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, recording)
}

func (s *Server) useArtifacts(w http.ResponseWriter, r *http.Request) {
	var req domain.UseRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	composition, err := s.useCapsule(r.Context(), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, composition)
}

func (s *Server) stopComposition(w http.ResponseWriter, r *http.Request) {
	var req domain.StopCompositionRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	composition, err := s.stopCapsule(r.Context(), r.PathValue("compositionID"), req.Operator)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, composition)
}

func (s *Server) createJob(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateJobRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	created, err := s.store.CreateJob(req)
	if err != nil {
		writeError(w, err)
		return
	}
	// A Job is never metadata-only: its remote Job branch must exist before
	// any Session can become active. Run is retained only for old clients.
	writeJSON(w, http.StatusAccepted, created)
	s.scheduleJobLaunch(created, req.Operator)
}

func (s *Server) deleteJob(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	job, _, err := s.store.PrepareJobDeletion(r.PathValue("jobID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	cleanupContext, cancel := context.WithTimeout(r.Context(), 15*time.Second)
	defer cancel()
	if err := s.stopJobRuntimes(cleanupContext, job, operator); err != nil {
		writeError(w, err)
		return
	}
	bundles := s.jobBundleRefs(job.ID)
	deleted, err := s.store.DeleteJob(job.ID, operator)
	if err != nil {
		writeError(w, err)
		return
	}
	s.removeUnusedBundles(bundles)
	for _, attachmentID := range deleted.AttachmentIDs {
		if s.attachments != nil {
			if removeErr := s.attachments.Remove(attachmentID); removeErr != nil {
				s.logger.Warn("remove Job attachment", "job", deleted.ID, "attachment", attachmentID, "error", removeErr)
			}
		}
	}
	s.workflowMu.Lock()
	for _, sessionID := range deleted.SessionIDs {
		delete(s.workflowTokens, sessionID)
	}
	s.workflowMu.Unlock()
	writeJSON(w, http.StatusOK, deleted)
}

func (s *Server) assignJob(w http.ResponseWriter, r *http.Request) {
	var req domain.AssignJobRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	job, err := s.store.AssignJob(r.PathValue("jobID"), s.requestOperator(r, ""), req.Assignee)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, job)
}

func (s *Server) updateJobEnvironment(w http.ResponseWriter, r *http.Request) {
	var req domain.UpdateJobEnvironmentRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	job, err := s.store.UpdateJobEnvironment(r.PathValue("jobID"), s.requestOperator(r, ""), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, job)
}

func (s *Server) closeJob(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	job, _, err := s.store.PrepareJobDeletion(r.PathValue("jobID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	cleanupContext, cancel := context.WithTimeout(r.Context(), 15*time.Second)
	defer cancel()
	if err := s.stopJobRuntimes(cleanupContext, job, operator); err != nil {
		writeError(w, err)
		return
	}
	closed, err := s.store.CloseJob(job.ID, operator)
	if err != nil {
		writeError(w, err)
		return
	}
	s.workflowMu.Lock()
	for _, sessionID := range closed.SessionIDs {
		delete(s.workflowTokens, sessionID)
	}
	s.workflowMu.Unlock()
	writeJSON(w, http.StatusOK, closed)
}

func (s *Server) stopJobRuntimes(ctx context.Context, job domain.Job, operator string) error {
	for _, sessionID := range job.SessionIDs {
		if err := s.cancelJobLaunch(ctx, sessionID); err != nil {
			return fmt.Errorf("cancel Job Session start %s: %w", sessionID, err)
		}
	}
	_, compositions, err := s.store.PrepareJobDeletion(job.ID, operator)
	if err != nil {
		return err
	}
	for _, composition := range compositions {
		s.stopACPComposition(composition.ID)
		if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
			continue
		}
		if _, err := s.stopCapsule(ctx, composition.ID, composition.Operator); err != nil {
			return fmt.Errorf("stop Job composition %s: %w", composition.ID, err)
		}
	}
	return nil
}

// adoptJobTemplate moves a Job to the newest revision of its Template,
// continuing at the chosen step.
func (s *Server) adoptJobTemplate(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	var request struct {
		PhaseID string `json:"phase_id"`
	}
	if !decodeJSON(w, r, &request) {
		return
	}
	created, previousCompositionID, _, err := s.store.AdoptWorkflowTemplate(r.PathValue("jobID"), operator, request.PhaseID)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusAccepted, created)
	s.scheduleWorkflowRetry(created, operator, previousCompositionID)
}

func (s *Server) retryWorkflowSession(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	created, previousCompositionID, err := s.store.RetryWorkflowSession(r.PathValue("sessionID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusAccepted, created)
	s.scheduleWorkflowRetry(created, operator, previousCompositionID)
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

	go func() {
		defer func() {
			cancel()
			s.jobLaunchMu.Lock()
			if s.jobLaunching[sessionID] == launch {
				delete(s.jobLaunching, sessionID)
			}
			s.jobLaunchMu.Unlock()
			close(launch.done)
		}()
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
	}()
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

// stopSessionCapsule closes the capsule of a Session by hand: a chat that
// brought a capsule back holds a login until someone closes it.
func (s *Server) stopSessionCapsule(w http.ResponseWriter, r *http.Request) {
	session, composition, err := s.sessionComposition(r.PathValue("sessionID"), s.requestOperator(r, ""))
	if err != nil {
		writeError(w, err)
		return
	}
	if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		writeJSON(w, http.StatusOK, composition)
		return
	}
	stopped, err := s.stopCapsule(r.Context(), composition.ID, session.Operator)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, stopped)
}

// restartSessionCapsule brings the capsule of a Session back when it was
// closed: a step the Job moved past keeps nothing running, and a chat on
// it starts the capsule again. The start is the same followable launch as
// a Job's, so a second request while it runs joins it.
func (s *Server) restartSessionCapsule(w http.ResponseWriter, r *http.Request) {
	session, composition, err := s.sessionComposition(r.PathValue("sessionID"), s.requestOperator(r, ""))
	if err != nil && !errors.Is(err, store.ErrConflict) {
		writeError(w, err)
		return
	}
	if err == nil && composition.Runtime != nil && composition.Runtime.Status != "stopped" {
		writeJSON(w, http.StatusOK, composition)
		return
	}
	if session.ID == "" {
		writeError(w, err)
		return
	}
	sessionID, operator := session.ID, normalizeOperator(session.Operator)
	// A step that is still running (it waited for an answer that came)
	// gets its agent back with the step's prompt; a step that is done, or
	// a Session outside a Job, gets only its capsule: the chat says the rest.
	running := false
	if session.PhaseRunID != "" {
		if _, _, run, _, _, _, err := s.store.WorkflowForSession(sessionID); err == nil && run.Status == domain.PhaseRunRunning {
			running = true
		}
	}
	s.beginTrackedLaunch(sessionID,
		func() bool { return s.jobSessionNeedsLaunch(sessionID, false) },
		func(ctx context.Context) {
			if running {
				s.launchWorkflowSessionContext(ctx, sessionID, operator)
				return
			}
			materializeContext, cancel := s.launchContext(ctx, sessionID)
			defer cancel()
			_, err := s.useCapsule(materializeContext, domain.UseRequest{Selector: "session:" + sessionID, Operator: operator})
			if err != nil && !errors.Is(err, context.Canceled) {
				s.logger.Warn("start Session capsule again", "session", sessionID, "error", err)
			}
			s.recordLaunchFailure(sessionID, err)
		})
	writeJSON(w, http.StatusAccepted, map[string]string{"status": "starting", "session_id": sessionID})
}

func (s *Server) createJobSession(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateJobSessionRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	session, err := s.store.CreateJobSession(r.PathValue("jobID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	created := domain.CreateJobSessionResponse{Session: session}
	if req.Run {
		composition, runErr := s.useCapsule(r.Context(), domain.UseRequest{
			Selector: "session:" + session.ID,
			Operator: req.Operator,
		})
		if runErr != nil {
			created.RunError = runErr.Error()
		} else {
			created.Composition = &composition
			created.Session.PreparedCompositionID = composition.ID
		}
	}
	writeJSON(w, http.StatusCreated, created)
}

func (s *Server) createMCPServer(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateMCPServerRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	server, err := s.store.CreateMCPServer(req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, server)
}

func (s *Server) deleteMCPServer(w http.ResponseWriter, r *http.Request) {
	server, err := s.store.DeleteMCPServer(r.PathValue("mcpServerID"), s.requestOperator(r, r.URL.Query().Get("operator")))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, server)
}

func (s *Server) createGitRepository(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateGitRepositoryRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	created, err := s.store.CreateGitRepository(req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, created)
}

func (s *Server) updateGitRepository(w http.ResponseWriter, r *http.Request) {
	var req domain.UpdateGitRepositoryRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	repository, err := s.store.UpdateGitRepository(r.PathValue("repositoryID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, repository)
}

func (s *Server) deleteGitRepository(w http.ResponseWriter, r *http.Request) {
	repository, err := s.store.DeleteGitRepository(r.PathValue("repositoryID"), s.requestOperator(r, r.URL.Query().Get("operator")))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, repository)
}

func (s *Server) createGitAccount(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateGitAccountRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	if req.CredentialScope == domain.CredentialScopeGlobal {
		identity, ok := identityFromRequest(r)
		if !ok || identity.User.Role != domain.UserAdmin {
			writeJSON(w, http.StatusForbidden, map[string]string{"error": "admin role required for a global Git account"})
			return
		}
	}
	account, err := s.store.CreateGitAccount(req)
	if err == nil {
		go s.launchQueuedWorkflowPhases("git account added")
	}
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, account)
}

func (s *Server) deleteGitAccount(w http.ResponseWriter, r *http.Request) {
	account, err := s.store.DeleteGitAccount(r.PathValue("accountID"), s.requestOperator(r, r.URL.Query().Get("operator")))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, account)
}

func (s *Server) selectResult(w http.ResponseWriter, r *http.Request) {
	var req domain.SelectResultRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	job, err := s.store.SelectResult(r.PathValue("jobID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, job)
}

func (s *Server) registerClient(w http.ResponseWriter, r *http.Request) {
	var req domain.RegisterClientRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	client, err := s.store.RegisterClient(req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, client)
}

func (s *Server) claim(w http.ResponseWriter, r *http.Request) {
	var req domain.ClaimRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	assignment, err := s.store.Claim(req)
	if errors.Is(err, store.ErrNoWork) {
		w.WriteHeader(http.StatusNoContent)
		return
	}
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, assignment)
}

func (s *Server) startSession(w http.ResponseWriter, r *http.Request) {
	var req domain.ActivationRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	session, err := s.store.StartSession(r.PathValue("sessionID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, session)
}

func (s *Server) heartbeat(w http.ResponseWriter, r *http.Request) {
	var req domain.ActivationRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	activation, err := s.store.Heartbeat(r.PathValue("activationID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, activation)
}

func (s *Server) startTurn(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateTurnRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	turn, err := s.store.StartTurn(r.PathValue("sessionID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, turn)
}

func (s *Server) createCheckpoint(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateCheckpointRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	checkpoint, err := s.store.AddCheckpoint(r.PathValue("sessionID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, checkpoint)
}

func (s *Server) createResult(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateResultRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	result, err := s.store.CompleteSession(r.PathValue("sessionID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, result)
}

func (s *Server) forkSession(w http.ResponseWriter, r *http.Request) {
	var req domain.ForkSessionRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	session, err := s.store.ForkSession(r.PathValue("sessionID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, session)
}

func (s *Server) logging(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if quietRequestPath(r.URL.Path) {
			s.logger.Debug("http request", "method", r.Method, "path", r.URL.Path)
		} else {
			s.logger.Info("http request", "method", r.Method, "path", r.URL.Path)
		}
		defer s.inflight.end(s.inflight.begin(r, s.logger))
		next.ServeHTTP(w, r)
	})
}

func decodeJSON(w http.ResponseWriter, r *http.Request, dst any) bool {
	defer r.Body.Close()
	dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, 1<<20))
	dec.DisallowUnknownFields()
	if err := dec.Decode(dst); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": fmt.Sprintf("invalid JSON: %v", err)})
		return false
	}
	return true
}

func writeError(w http.ResponseWriter, err error) {
	status := http.StatusInternalServerError
	switch {
	case errors.Is(err, store.ErrNotFound):
		status = http.StatusNotFound
	case errors.Is(err, store.ErrConflict), errors.Is(err, store.ErrStaleActivation):
		status = http.StatusConflict
	}
	writeJSON(w, status, map[string]string{"error": err.Error()})
}

func writeJSON(w http.ResponseWriter, status int, value any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(value)
}
