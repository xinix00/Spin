package server

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"slices"
	"strings"
	"sync"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/orchestrator"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
	"easyacp/internal/worker"
)

type Server struct {
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
	backupMu        sync.Mutex
	backupTicketMu  sync.Mutex
	backupTickets   map[string]backupTicket
}

type backgroundJobLaunch struct {
	cancel context.CancelFunc
	done   chan struct{}
}

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
		authDisabled: options.DisableAuthentication, workerToken: strings.TrimSpace(options.WorkerToken),
		runnerBroker: options.RunnerBroker,
		internalURL:  strings.TrimRight(strings.TrimSpace(options.InternalURL), "/"),
		attachments:  attachmentStorage, snapshotArchive: options.SnapshotArchive, database: options.Database,
		loginLimiter: loginLimiter{attempts: map[string]loginAttempt{}}, csrfTokens: csrfTokenCache{values: map[string]string{}},
		terminals: map[string]map[*activeTerminal]struct{}{}, acpSessions: map[string]*activeACP{}, workflowTokens: map[string]string{}, jobLaunching: map[string]*backgroundJobLaunch{}, backupTickets: map[string]backupTicket{},
	}
	s.gitOAuth = newGitOAuthManager(options, st)
	s.routes()
	go s.resumeQueuedWorkflowActions()
	return s
}

func (s *Server) resumeQueuedWorkflowActions() {
	snapshot := s.store.Snapshot()
	for _, session := range snapshot.Sessions {
		if session.Executor != domain.WorkflowExecutorAction || session.PhaseRunID == "" {
			continue
		}
		jobIndex := slices.IndexFunc(snapshot.Jobs, func(job domain.Job) bool {
			return job.ID == session.JobID && job.CurrentPhaseRunID == session.PhaseRunID
		})
		runIndex := slices.IndexFunc(snapshot.PhaseRuns, func(run domain.PhaseRun) bool {
			return run.ID == session.PhaseRunID && run.Status == domain.PhaseRunQueued
		})
		if jobIndex >= 0 && runIndex >= 0 {
			s.launchWorkflowSession(session.ID, session.Operator)
		}
	}
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
			w.Header().Set("Cache-Control", "no-store")
		}
		next.ServeHTTP(w, r)
	})
}

func (s *Server) routes() {
	s.mux.HandleFunc("GET /", s.dashboard)
	assets := http.FileServer(http.FS(dashboardAssets))
	s.mux.Handle("GET /assets/", http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Cache-Control", "public, max-age=31536000, immutable")
		assets.ServeHTTP(w, r)
	}))
	s.mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
	})
	if s.runnerBroker != nil {
		s.mux.HandleFunc("GET /api/runner/ws", s.runnerBroker.Handler)
	}
	s.mux.HandleFunc("GET /api/auth/status", s.authStatus)
	s.mux.HandleFunc("POST /api/auth/setup", s.setupOwner)
	s.mux.HandleFunc("POST /api/auth/login", s.login)
	s.mux.HandleFunc("POST /api/auth/logout", s.logout)
	s.mux.HandleFunc("POST /api/auth/users", s.createUser)
	s.mux.HandleFunc("POST /api/backup", s.downloadBackup)
	s.mux.HandleFunc("POST /api/backup-ticket", s.createBackupTicket)
	s.mux.HandleFunc("GET /api/backup", s.downloadBackupWithTicket)
	s.mux.HandleFunc("POST /api/restore", s.restoreBackup)
	s.mux.HandleFunc("GET /api/state", func(w http.ResponseWriter, r *http.Request) {
		identity, _ := identityFromRequest(r)
		snapshot := s.store.Snapshot()
		if !s.authDisabled {
			snapshot = visibleSnapshot(snapshot, identity.User.Username)
		}
		recommendations := orchestrator.Recommend(snapshot)
		if recommendations == nil {
			recommendations = []domain.Recommendation{}
		}
		writeJSON(w, http.StatusOK, stateResponse{Snapshot: snapshot, Recommendations: recommendations, Engine: s.engine.Info(), GitOAuthProviders: s.gitOAuth.publicProviders(r), CurrentUser: publicUser(identity.User)})
	})
	s.mux.HandleFunc("GET /api/artifacts", s.listArtifacts)
	s.mux.HandleFunc("DELETE /api/artifacts/{artifactID}", s.deleteArtifact)
	s.mux.HandleFunc("POST /api/recordings", s.createRecording)
	s.mux.HandleFunc("POST /api/recordings/{recordingID}/commands", s.appendRecordingCommand)
	s.mux.HandleFunc("GET /api/recordings/{recordingID}/terminal", s.recordingTerminal)
	s.mux.HandleFunc("POST /api/recordings/{recordingID}/parents", s.attachRecordingParent)
	s.mux.HandleFunc("POST /api/recordings/{recordingID}/end", s.endRecording)
	s.mux.HandleFunc("POST /api/recordings/{recordingID}/cancel", s.cancelRecording)
	s.mux.HandleFunc("POST /api/use", s.useArtifacts)
	s.mux.HandleFunc("POST /api/compositions/{compositionID}/acp/probe", s.probeACPHandler)
	s.mux.HandleFunc("POST /api/compositions/{compositionID}/stop", s.stopComposition)
	s.mux.HandleFunc("POST /api/commands", s.executeCommand)
	s.mux.HandleFunc("POST /api/jobs", s.createJob)
	s.mux.HandleFunc("POST /api/job-attachments", s.uploadStagedJobAttachment)
	s.mux.HandleFunc("GET /api/job-attachments/{attachmentID}", s.downloadJobAttachment)
	s.mux.HandleFunc("DELETE /api/job-attachments/{attachmentID}", s.deleteStagedJobAttachment)
	s.mux.HandleFunc("POST /api/jobs/{jobID}/attachments", s.uploadJobAttachment)
	s.mux.HandleFunc("GET /api/jobs/{jobID}/changes", s.jobChanges)
	s.mux.HandleFunc("POST /api/jobs/{jobID}/close", s.closeJob)
	s.mux.HandleFunc("DELETE /api/jobs/{jobID}", s.deleteJob)
	s.mux.HandleFunc("POST /api/deliverables/{deliverableID}/comments", s.createDeliverableComment)
	s.mux.HandleFunc("POST /api/sessions/{sessionID}/retry", s.retryWorkflowSession)
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

type stateResponse struct {
	domain.Snapshot
	Recommendations   []domain.Recommendation  `json:"recommendations"`
	Engine            domain.CapsuleEngineInfo `json:"engine"`
	GitOAuthProviders []gitOAuthProviderInfo   `json:"git_oauth_providers"`
	CurrentUser       domain.PublicUser        `json:"current_user"`
}

func (s *Server) dashboard(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	_, _ = w.Write(dashboardHTML)
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
	artifact, err := s.store.PrepareArtifactDeletion(r.PathValue("artifactID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	if remover, ok := s.engine.(capsule.SnapshotRemover); ok {
		if err := remover.RemoveSnapshot(r.Context(), artifact.Snapshot); err != nil {
			writeError(w, fmt.Errorf("remove artifact snapshot: %w", err))
			return
		}
	}
	deleted, err := s.store.DeleteArtifact(artifact.ID, operator)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, deleted)
}

func (s *Server) createRecording(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateRecordingRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Actor = s.requestOperator(r, req.Actor)
	recording, err := s.createCapsuleRecording(r.Context(), req)
	if err != nil {
		writeError(w, err)
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
	artifact, err := s.endCapsuleRecording(r.Context(), r.PathValue("recordingID"), req)
	if err != nil {
		writeError(w, err)
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
	deleted, err := s.store.DeleteJob(job.ID, operator)
	if err != nil {
		writeError(w, err)
		return
	}
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
	s.jobLaunchMu.Lock()
	if s.jobLaunching[sessionID] != nil {
		s.jobLaunchMu.Unlock()
		return
	}
	if !s.jobSessionNeedsLaunch(sessionID, created.Job.TemplateID != "") {
		s.jobLaunchMu.Unlock()
		return
	}
	launchContext, cancel := context.WithCancel(context.Background())
	launch := &backgroundJobLaunch{cancel: cancel, done: make(chan struct{})}
	s.jobLaunching[sessionID] = launch
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
		if created.Job.TemplateID != "" {
			s.launchWorkflowSessionContext(launchContext, sessionID, operator)
			return
		}
		materializeContext, materializeCancel := context.WithTimeout(launchContext, 2*time.Minute)
		defer materializeCancel()
		if _, err := s.useCapsule(materializeContext, domain.UseRequest{Selector: "session:" + sessionID, Operator: operator}); err != nil && !errors.Is(err, context.Canceled) {
			s.logger.Warn("start queued Job Session", "session", sessionID, "error", err)
		}
	}()
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
	launch := &backgroundJobLaunch{cancel: cancel, done: make(chan struct{})}
	s.jobLaunching[sessionID] = launch
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
			cleanupContext, cleanupCancel := context.WithTimeout(launchContext, 30*time.Second)
			_, stopErr := s.stopCapsule(cleanupContext, previousCompositionID, operator)
			cleanupCancel()
			if stopErr != nil && !errors.Is(stopErr, store.ErrNotFound) && !errors.Is(stopErr, context.Canceled) {
				s.logger.Warn("clean workflow Session for retry", "session", sessionID, "composition", previousCompositionID, "error", stopErr)
				return
			}
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
		return runIndex >= 0 && snapshot.PhaseRuns[runIndex].Status == domain.PhaseRunQueued
	}
	if session.PreparedCompositionID == "" {
		return true
	}
	compositionIndex := slices.IndexFunc(snapshot.Compositions, func(composition domain.Composition) bool { return composition.ID == session.PreparedCompositionID })
	return compositionIndex < 0 || snapshot.Compositions[compositionIndex].Runtime == nil || snapshot.Compositions[compositionIndex].Runtime.Status == "stopped"
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
		s.logger.Debug("http request", "method", r.Method, "path", r.URL.Path)
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
