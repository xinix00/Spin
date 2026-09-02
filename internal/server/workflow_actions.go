package server

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"slices"
	"strconv"
	"strings"
	"time"

	"easyacp/internal/domain"
)

type githubPullRequest struct {
	Number  int    `json:"number"`
	HTMLURL string `json:"html_url"`
	Title   string `json:"title"`
}

func (s *Server) launchWorkflowAction(ctx context.Context, sessionID string) {
	job, _, _, phase, _, _, err := s.store.WorkflowForSession(sessionID)
	if err != nil {
		s.logger.Warn("load workflow action", "session", sessionID, "error", err)
		return
	}
	if phase.Executor != domain.WorkflowExecutorAction || phase.Action == nil {
		s.logger.Warn("invalid workflow action phase", "session", sessionID)
		return
	}
	if _, err := s.store.MarkWorkflowPhaseRunning(sessionID); err != nil {
		s.logger.Warn("mark workflow action running", "session", sessionID, "error", err)
		return
	}
	actionContext, cancel := context.WithTimeout(ctx, 45*time.Second)
	defer cancel()
	var result domain.WorkflowActionResult
	switch phase.Action.Type {
	case domain.WorkflowActionGitPullRequest:
		result, err = s.createGitHubPullRequest(actionContext, job)
	default:
		err = fmt.Errorf("unsupported workflow action %q", phase.Action.Type)
	}
	if err != nil {
		s.finishWorkflowAction(sessionID, "reject", err.Error())
		return
	}
	if _, err := s.store.SetWorkflowActionResult(sessionID, result); err != nil {
		s.finishWorkflowAction(sessionID, "reject", "actie slaagde maar het resultaat kon niet worden opgeslagen: "+err.Error())
		return
	}
	s.finishWorkflowAction(sessionID, "accept", result.Detail)
}

func (s *Server) finishWorkflowAction(sessionID, outcome, detail string) {
	advance, err := s.store.CompleteWorkflowPhase(sessionID, outcome, detail)
	if err != nil {
		s.logger.Warn("finish workflow action", "session", sessionID, "outcome", outcome, "error", err)
		return
	}
	if advance.NextSession != nil {
		go s.launchWorkflowSession(advance.NextSession.ID, advance.NextSession.Operator)
	} else if advance.Question == nil && advance.Job.WorkflowStatus == domain.WorkflowDone {
		s.retireWorkflowCompositions(advance.Job.ID, "")
	}
}

func (s *Server) createGitHubPullRequest(ctx context.Context, job domain.Job) (domain.WorkflowActionResult, error) {
	snapshot := s.store.Snapshot()
	repositoryIndex := slices.IndexFunc(snapshot.GitRepositories, func(repository domain.GitRepository) bool { return repository.ID == job.GitRepositoryID })
	if repositoryIndex < 0 {
		return domain.WorkflowActionResult{}, fmt.Errorf("Git repository is unavailable")
	}
	repository := snapshot.GitRepositories[repositoryIndex]
	workspace := &domain.GitWorkspace{
		RepositoryID: repository.ID, RepositoryName: job.GitRepositoryName, RemoteURL: job.GitRemoteURL,
		Provider: job.GitProvider, CredentialScope: job.GitCredentialScope,
	}
	if workspace.RepositoryName == "" {
		workspace.RepositoryName = repository.Name
	}
	if workspace.RemoteURL == "" {
		workspace.RemoteURL = repository.RemoteURL
	}
	if workspace.Provider == "" {
		workspace.Provider = repository.Provider
	}
	if workspace.CredentialScope == "" {
		workspace.CredentialScope = repository.CredentialScope
	}
	account, authenticated, err := s.gitAccountForWorkspace(ctx, workspace, job.Owner)
	if err != nil {
		return domain.WorkflowActionResult{}, fmt.Errorf("resolve GitHub connection for user %s: %w", job.Owner, err)
	}
	if !authenticated {
		return domain.WorkflowActionResult{}, fmt.Errorf("repository has no GitHub identity scope")
	}
	if account.Provider != "github" || !strings.EqualFold(account.Host, "github.com") {
		return domain.WorkflowActionResult{}, fmt.Errorf("git.pull_request.create currently requires a github.com account")
	}
	owner, name, err := githubRepository(workspace.RemoteURL)
	if err != nil {
		return domain.WorkflowActionResult{}, err
	}
	base := strings.TrimPrefix(strings.TrimPrefix(job.BaseRef, "refs/heads/"), "origin/")
	head := strings.TrimPrefix(job.Branch, "refs/heads/")
	if existing, ok, err := s.findGitHubPullRequest(ctx, account.AccessToken, owner, name, head, base); err != nil {
		return domain.WorkflowActionResult{}, err
	} else if ok {
		return pullRequestActionResult(existing, true), nil
	}
	payload := map[string]string{"title": job.Title, "body": job.Objective, "head": head, "base": base}
	var created githubPullRequest
	status, body, err := s.githubAPI(ctx, http.MethodPost, "/repos/"+url.PathEscape(owner)+"/"+url.PathEscape(name)+"/pulls", account.AccessToken, payload, &created)
	if err != nil {
		return domain.WorkflowActionResult{}, err
	}
	if status != http.StatusCreated {
		return domain.WorkflowActionResult{}, fmt.Errorf("GitHub PR creation returned %d: %s", status, strings.TrimSpace(string(body)))
	}
	return pullRequestActionResult(created, false), nil
}

func (s *Server) findGitHubPullRequest(ctx context.Context, token, owner, name, head, base string) (githubPullRequest, bool, error) {
	query := url.Values{"state": {"open"}, "head": {owner + ":" + head}, "base": {base}}
	path := "/repos/" + url.PathEscape(owner) + "/" + url.PathEscape(name) + "/pulls?" + query.Encode()
	var pulls []githubPullRequest
	status, body, err := s.githubAPI(ctx, http.MethodGet, path, token, nil, &pulls)
	if err != nil {
		return githubPullRequest{}, false, err
	}
	if status != http.StatusOK {
		return githubPullRequest{}, false, fmt.Errorf("GitHub PR lookup returned %d: %s", status, strings.TrimSpace(string(body)))
	}
	if len(pulls) == 0 {
		return githubPullRequest{}, false, nil
	}
	return pulls[0], true, nil
}

func (s *Server) githubAPI(ctx context.Context, method, path, token string, input, output any) (int, []byte, error) {
	var body io.Reader
	if input != nil {
		encoded, err := json.Marshal(input)
		if err != nil {
			return 0, nil, err
		}
		body = bytes.NewReader(encoded)
	}
	request, err := http.NewRequestWithContext(ctx, method, "https://api.github.com"+path, body)
	if err != nil {
		return 0, nil, err
	}
	request.Header.Set("Accept", "application/vnd.github+json")
	request.Header.Set("Authorization", "Bearer "+token)
	request.Header.Set("X-GitHub-Api-Version", "2022-11-28")
	request.Header.Set("User-Agent", "Spin-Workflow")
	if input != nil {
		request.Header.Set("Content-Type", "application/json")
	}
	response, err := s.httpClient.Do(request)
	if err != nil {
		return 0, nil, fmt.Errorf("GitHub API: %w", err)
	}
	defer response.Body.Close()
	responseBody, err := io.ReadAll(io.LimitReader(response.Body, 128<<10))
	if err != nil {
		return 0, nil, fmt.Errorf("read GitHub response: %w", err)
	}
	if response.StatusCode >= 200 && response.StatusCode < 300 && output != nil {
		if err := json.Unmarshal(responseBody, output); err != nil {
			return 0, nil, fmt.Errorf("decode GitHub response: %w", err)
		}
	}
	return response.StatusCode, responseBody, nil
}

func githubRepository(remoteURL string) (string, string, error) {
	parsed, err := url.Parse(strings.TrimSpace(remoteURL))
	if err != nil || parsed.Scheme != "https" || !strings.EqualFold(parsed.Host, "github.com") {
		return "", "", fmt.Errorf("pull requests require an HTTPS github.com repository")
	}
	parts := strings.Split(strings.Trim(strings.TrimSuffix(parsed.Path, ".git"), "/"), "/")
	if len(parts) != 2 || parts[0] == "" || parts[1] == "" {
		return "", "", fmt.Errorf("cannot determine GitHub owner/repository from %s", remoteURL)
	}
	return parts[0], parts[1], nil
}

func pullRequestActionResult(pull githubPullRequest, existing bool) domain.WorkflowActionResult {
	detail := "Pull request aangemaakt: " + pull.HTMLURL
	if existing {
		detail = "Bestaande pull request gebruikt: " + pull.HTMLURL
	}
	return domain.WorkflowActionResult{Type: domain.WorkflowActionGitPullRequest, ExternalID: strconv.Itoa(pull.Number), URL: pull.HTMLURL, Detail: detail, CreatedAt: time.Now().UTC()}
}
