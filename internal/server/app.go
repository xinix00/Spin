package server

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// A repository's app services run on the runner that holds a Session's
// capsule, next to it, so a person can test what the agent built. Starting
// is a job (a dependency image may need pulling); status and logs are read
// live from the runner, which is the source of truth, so nothing durable
// has to be cleaned up.

// appServiceRunner is what the remote engine offers for app services.
type appServiceRunner interface {
	StartAppServices(ctx context.Context, runtime domain.CapsuleRuntime, sessionID string, services []domain.AppService) ([]domain.AppServiceRuntime, error)
	StopAppServicesOn(ctx context.Context, clientID, sessionID string) error
	AppServiceStatusOn(ctx context.Context, clientID, sessionID string) ([]domain.AppServiceRuntime, error)
	AppServiceLogsOn(ctx context.Context, clientID, sessionID, service string, tail int) (string, error)
}

type appStart struct {
	mu        sync.Mutex
	status    string // running, done, error
	error     string
	startedAt time.Time
}

// appStatusResponse is what the browser shows for a Session's app.
type appStatusResponse struct {
	SessionID string                     `json:"session_id"`
	Services  []domain.AppService        `json:"services"`
	Running   []domain.AppServiceRuntime `json:"running"`
	Start     string                     `json:"start,omitempty"` // running, done, error
	Error     string                     `json:"error,omitempty"`
	StartedAt *time.Time                 `json:"started_at,omitempty"`
}

// appTarget resolves what a Session's app needs: its repository recipe and
// its live capsule runtime.
func (s *Server) appTarget(sessionID, operator string) (domain.Session, domain.Composition, domain.GitRepository, error) {
	session, composition, err := s.sessionComposition(sessionID, operator)
	if err != nil {
		return domain.Session{}, domain.Composition{}, domain.GitRepository{}, err
	}
	snapshot := s.store.Snapshot()
	index := slices.IndexFunc(snapshot.GitRepositories, func(repository domain.GitRepository) bool { return repository.ID == session.GitRepositoryID })
	if index < 0 {
		return domain.Session{}, domain.Composition{}, domain.GitRepository{}, fmt.Errorf("session has no Git repository: %w", store.ErrConflict)
	}
	return session, composition, snapshot.GitRepositories[index], nil
}

// startAppServices brings the repository's services up on the Session's
// workspace, in the background. A start already running is reused.
func (s *Server) startAppServices(sessionID, operator string) (*appStart, error) {
	session, composition, repository, err := s.appTarget(sessionID, operator)
	if err != nil {
		return nil, err
	}
	if len(repository.Services) == 0 {
		return nil, fmt.Errorf("repository %s has no app services; add them under Connections → Git: %w", repository.Name, store.ErrConflict)
	}
	if composition.Runtime == nil || composition.Runtime.Status != "ready" {
		return nil, fmt.Errorf("session workspace is not running: %w", store.ErrConflict)
	}
	runner, ok := s.engine.(appServiceRunner)
	if !ok {
		return nil, fmt.Errorf("capsule engine %s cannot run app services: %w", s.engine.Info().Driver, store.ErrConflict)
	}
	s.appMu.Lock()
	if existing := s.appStarts[sessionID]; existing != nil {
		existing.mu.Lock()
		running := existing.status == "running"
		existing.mu.Unlock()
		if running {
			s.appMu.Unlock()
			return existing, nil
		}
	}
	start := &appStart{status: "running", startedAt: time.Now().UTC()}
	s.appStarts[sessionID] = start
	s.appMu.Unlock()
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
		defer cancel()
		results, err := runner.StartAppServices(ctx, *composition.Runtime, session.ID, repository.Services)
		if err == nil {
			var failed []string
			for _, result := range results {
				if result.Error != "" {
					failed = append(failed, result.Service+": "+result.Error)
				}
			}
			if len(failed) > 0 {
				err = errors.New(strings.Join(failed, " · "))
			}
		}
		start.mu.Lock()
		if err != nil {
			start.status, start.error = "error", err.Error()
			s.logger.Warn("start app services", "session", session.ID, "error", err)
		} else {
			start.status = "done"
		}
		start.mu.Unlock()
	}()
	return start, nil
}

// awaitAppStart waits, bounded, for a start to finish.
func awaitAppStart(start *appStart, limit time.Duration) (string, string) {
	deadline := time.Now().Add(limit)
	for {
		start.mu.Lock()
		status, message := start.status, start.error
		start.mu.Unlock()
		if status != "running" || time.Now().After(deadline) {
			return status, message
		}
		time.Sleep(200 * time.Millisecond)
	}
}

func (s *Server) stopAppServices(ctx context.Context, sessionID, operator string) error {
	session, composition, _, err := s.appTarget(sessionID, operator)
	if err != nil {
		return err
	}
	runner, ok := s.engine.(appServiceRunner)
	if !ok || composition.Runtime == nil {
		return nil
	}
	return runner.StopAppServicesOn(ctx, composition.Runtime.ClientID, session.ID)
}

// stopAppServicesForComposition removes a Session's app before its capsule
// goes; the app shares the capsule's image and workspace.
func (s *Server) stopAppServicesForComposition(ctx context.Context, composition domain.Composition) {
	runner, ok := s.engine.(appServiceRunner)
	if !ok || composition.SessionID == "" || composition.Runtime == nil {
		return
	}
	if err := runner.StopAppServicesOn(ctx, composition.Runtime.ClientID, composition.SessionID); err != nil {
		s.logger.Warn("stop app services with capsule", "session", composition.SessionID, "error", err)
	}
}

func (s *Server) appStatus(ctx context.Context, sessionID, operator string) (appStatusResponse, error) {
	session, composition, repository, err := s.appTarget(sessionID, operator)
	if err != nil {
		return appStatusResponse{}, err
	}
	response := appStatusResponse{SessionID: session.ID, Services: repository.Services}
	s.appMu.Lock()
	start := s.appStarts[sessionID]
	s.appMu.Unlock()
	if start != nil {
		start.mu.Lock()
		response.Start, response.Error = start.status, start.error
		startedAt := start.startedAt
		response.StartedAt = &startedAt
		start.mu.Unlock()
	}
	runner, ok := s.engine.(appServiceRunner)
	if ok && composition.Runtime != nil && composition.Runtime.Status == "ready" {
		running, err := runner.AppServiceStatusOn(ctx, composition.Runtime.ClientID, session.ID)
		if err != nil && response.Error == "" {
			response.Error = err.Error()
		}
		response.Running = running
	}
	return response, nil
}

func (s *Server) startAppHandler(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	start, err := s.startAppServices(r.PathValue("sessionID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	// A quick start answers with the result; a slow one with "running" and
	// the browser follows the status.
	status, message := awaitAppStart(start, 5*time.Second)
	writeJSON(w, http.StatusAccepted, map[string]string{"status": status, "error": message})
}

func (s *Server) stopAppHandler(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	ctx, cancel := context.WithTimeout(r.Context(), 60*time.Second)
	defer cancel()
	if err := s.stopAppServices(ctx, r.PathValue("sessionID"), operator); err != nil {
		writeError(w, err)
		return
	}
	s.appMu.Lock()
	delete(s.appStarts, r.PathValue("sessionID"))
	s.appMu.Unlock()
	writeJSON(w, http.StatusOK, map[string]string{"status": "stopped"})
}

func (s *Server) appStatusHandler(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	ctx, cancel := context.WithTimeout(r.Context(), 20*time.Second)
	defer cancel()
	status, err := s.appStatus(ctx, r.PathValue("sessionID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, status)
}

func (s *Server) appLogsHandler(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	session, composition, _, err := s.appTarget(r.PathValue("sessionID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	runner, ok := s.engine.(appServiceRunner)
	if !ok || composition.Runtime == nil {
		writeError(w, fmt.Errorf("session has no runner: %w", store.ErrConflict))
		return
	}
	tail, _ := strconv.Atoi(r.URL.Query().Get("tail"))
	ctx, cancel := context.WithTimeout(r.Context(), 20*time.Second)
	defer cancel()
	output, err := runner.AppServiceLogsOn(ctx, composition.Runtime.ClientID, session.ID, r.PathValue("service"), tail)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"service": r.PathValue("service"), "output": output})
}

// appLinks renders the reachable addresses of running services for a
// decision text.
func appLinks(results []domain.AppServiceRuntime) string {
	var links []string
	for _, result := range results {
		for containerPort, hostPort := range result.Ports {
			links = append(links, fmt.Sprintf("%s: http://%s:%d (poort %s)", result.Service, result.Host, hostPort, containerPort))
		}
	}
	slices.Sort(links)
	return strings.Join(links, " · ")
}

// launchWorkflowExpose runs an expose phase: the workspace is up, the app
// gets started on it, and the phase then waits for a person's verdict.
func (s *Server) launchWorkflowExpose(session domain.Session, operator string) {
	requeue := func(what string, err error) {
		s.logger.Warn(what, "session", session.ID, "error", err)
		s.recordLaunchFailure(session.ID, fmt.Errorf("%s: %w", what, err))
		if _, requeueErr := s.store.RequeueWorkflowPhase(session.ID); requeueErr != nil {
			s.logger.Warn("requeue workflow phase", "session", session.ID, "error", requeueErr)
		}
	}
	s.recordLaunchProgress(session.ID, launchProgress{Stage: "app", Message: "Test-app starten op de workspace", UpdatedAt: time.Now().UTC()})
	start, err := s.startAppServices(session.ID, operator)
	if err != nil {
		requeue("start test app", err)
		return
	}
	status, message := awaitAppStart(start, 10*time.Minute)
	if status != "done" {
		requeue("start test app", errors.New(message))
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	live, err := s.appStatus(ctx, session.ID, operator)
	cancel()
	detail := "Test de app en kies ACCEPT of REJECT met een reden."
	if err == nil {
		if links := appLinks(live.Running); links != "" {
			detail = links + " · " + detail
		}
	}
	advance, err := s.store.CompleteWorkflowPhase(session.ID, "accept", detail)
	if err != nil {
		requeue("wait for the app verdict", err)
		return
	}
	if advance.NextSession != nil {
		s.startQueuedWorkflowLaunch(*advance.NextSession)
	}
}
