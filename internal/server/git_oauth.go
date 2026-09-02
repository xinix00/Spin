package server

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/store"
	"easyacp/internal/worker"
)

type GitOAuthProviderConfig struct {
	Name         string
	ClientID     string
	ClientSecret string
	AuthorizeURL string
	TokenURL     string
	UserURL      string
	Scope        string
	Host         string
}

type ServerOptions struct {
	PublicURL             string
	InternalURL           string
	HTTPClient            *http.Client
	GitOAuthProviders     map[string]GitOAuthProviderConfig
	DisableAuthentication bool
	WorkerToken           string
	AttachmentDir         string
	RunnerBroker          *worker.Broker
}

func ServerOptionsFromEnvironment() ServerOptions {
	providers := map[string]GitOAuthProviderConfig{}
	if id, secret := strings.TrimSpace(os.Getenv("SPIN_GITHUB_CLIENT_ID")), strings.TrimSpace(os.Getenv("SPIN_GITHUB_CLIENT_SECRET")); id != "" && secret != "" {
		providers["github"] = GitOAuthProviderConfig{
			Name: "GitHub", ClientID: id, ClientSecret: secret, Host: "github.com",
			AuthorizeURL: "https://github.com/login/oauth/authorize",
			TokenURL:     "https://github.com/login/oauth/access_token",
			UserURL:      "https://api.github.com/user", Scope: "repo read:user user:email",
		}
	}
	if id, secret := strings.TrimSpace(os.Getenv("SPIN_GITLAB_CLIENT_ID")), strings.TrimSpace(os.Getenv("SPIN_GITLAB_CLIENT_SECRET")); id != "" && secret != "" {
		providers["gitlab"] = GitOAuthProviderConfig{
			Name: "GitLab", ClientID: id, ClientSecret: secret, Host: "gitlab.com",
			AuthorizeURL: "https://gitlab.com/oauth/authorize",
			TokenURL:     "https://gitlab.com/oauth/token",
			UserURL:      "https://gitlab.com/api/v4/user", Scope: "read_user read_repository write_repository",
		}
	}
	return ServerOptions{
		PublicURL: strings.TrimRight(strings.TrimSpace(os.Getenv("SPIN_PUBLIC_URL")), "/"), GitOAuthProviders: providers,
		InternalURL:   strings.TrimRight(strings.TrimSpace(os.Getenv("SPIN_INTERNAL_URL")), "/"),
		WorkerToken:   strings.TrimSpace(os.Getenv("SPIN_WORKER_TOKEN")),
		AttachmentDir: strings.TrimSpace(os.Getenv("SPIN_ATTACHMENT_DIR")),
	}
}

type gitOAuthProviderInfo struct {
	ID          string `json:"id"`
	Name        string `json:"name"`
	Configured  bool   `json:"configured"`
	Source      string `json:"source,omitempty"`
	ClientID    string `json:"client_id,omitempty"`
	CallbackURL string `json:"callback_url"`
	SetupURL    string `json:"setup_url"`
}

type gitOAuthAttempt struct {
	Operator        string
	Provider        string
	CredentialScope domain.CredentialScope
	Verifier        string
	RedirectURI     string
	ExpiresAt       time.Time
}

type gitOAuthManager struct {
	publicURL string
	client    *http.Client
	providers map[string]GitOAuthProviderConfig
	store     *store.Store
	mu        sync.Mutex
	attempts  map[string]gitOAuthAttempt
}

func newGitOAuthManager(options ServerOptions, st *store.Store) *gitOAuthManager {
	client := options.HTTPClient
	if client == nil {
		client = &http.Client{Timeout: 15 * time.Second}
	}
	providers := map[string]GitOAuthProviderConfig{}
	for id, config := range options.GitOAuthProviders {
		id = strings.ToLower(strings.TrimSpace(id))
		if id != "" && config.ClientID != "" && config.ClientSecret != "" {
			providers[id] = config
		}
	}
	return &gitOAuthManager{publicURL: strings.TrimRight(options.PublicURL, "/"), client: client, providers: providers, store: st, attempts: map[string]gitOAuthAttempt{}}
}

func (m *gitOAuthManager) publicProviders(r *http.Request) []gitOAuthProviderInfo {
	order := []string{"github", "gitlab"}
	out := []gitOAuthProviderInfo{}
	for _, id := range order {
		provider, configured, source := m.provider(id)
		setupURL := "https://github.com/settings/applications/new"
		if id == "gitlab" {
			setupURL = "https://gitlab.com/-/user_settings/applications"
		}
		out = append(out, gitOAuthProviderInfo{ID: id, Name: provider.Name, Configured: configured, Source: source, ClientID: provider.ClientID, CallbackURL: m.callbackURL(r, id), SetupURL: setupURL})
	}
	return out
}

func (m *gitOAuthManager) provider(id string) (GitOAuthProviderConfig, bool, string) {
	id = strings.ToLower(strings.TrimSpace(id))
	base, ok := builtinGitOAuthProvider(id)
	if !ok {
		return GitOAuthProviderConfig{}, false, ""
	}
	if configured, ok := m.providers[id]; ok {
		return configured, true, "environment"
	}
	if m.store != nil {
		if stored, err := m.store.GitOAuthConfiguration(id); err == nil {
			base.ClientID = stored.ClientID
			base.ClientSecret = stored.ClientSecret
			return base, base.ClientID != "" && base.ClientSecret != "", "app"
		}
	}
	return base, false, ""
}

func builtinGitOAuthProvider(id string) (GitOAuthProviderConfig, bool) {
	switch id {
	case "github":
		return GitOAuthProviderConfig{Name: "GitHub", Host: "github.com", AuthorizeURL: "https://github.com/login/oauth/authorize", TokenURL: "https://github.com/login/oauth/access_token", UserURL: "https://api.github.com/user", Scope: "repo read:user user:email"}, true
	case "gitlab":
		return GitOAuthProviderConfig{Name: "GitLab", Host: "gitlab.com", AuthorizeURL: "https://gitlab.com/oauth/authorize", TokenURL: "https://gitlab.com/oauth/token", UserURL: "https://gitlab.com/api/v4/user", Scope: "read_user read_repository write_repository"}, true
	default:
		return GitOAuthProviderConfig{}, false
	}
}

func (s *Server) startGitOAuth(w http.ResponseWriter, r *http.Request) {
	providerID := strings.ToLower(strings.TrimSpace(r.PathValue("provider")))
	provider, configured, _ := s.gitOAuth.provider(providerID)
	identity, authenticated := identityFromRequest(r)
	operator := identity.User.Username
	credentialScope := domain.CredentialScope(strings.ToLower(strings.TrimSpace(r.URL.Query().Get("credential_scope"))))
	if credentialScope == "" {
		credentialScope = domain.CredentialScopeUser
	}
	if credentialScope != domain.CredentialScopeUser && credentialScope != domain.CredentialScopeGlobal {
		writeError(w, fmt.Errorf("invalid Git credential scope: %w", store.ErrConflict))
		return
	}
	if credentialScope == domain.CredentialScopeGlobal && identity.User.Role != domain.UserAdmin {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "admin role required for a global Git account"})
		return
	}
	if !configured || !authenticated || operator == "" {
		writeError(w, fmt.Errorf("configured OAuth provider and operator are required: %w", store.ErrNotFound))
		return
	}
	state, err := randomOAuthValue(32)
	if err != nil {
		writeError(w, err)
		return
	}
	verifier, err := randomOAuthValue(48)
	if err != nil {
		writeError(w, err)
		return
	}
	redirectURI := s.gitOAuth.callbackURL(r, providerID)
	s.gitOAuth.mu.Lock()
	for key, attempt := range s.gitOAuth.attempts {
		if time.Now().After(attempt.ExpiresAt) {
			delete(s.gitOAuth.attempts, key)
		}
	}
	s.gitOAuth.attempts[state] = gitOAuthAttempt{Operator: operator, Provider: providerID, CredentialScope: credentialScope, Verifier: verifier, RedirectURI: redirectURI, ExpiresAt: time.Now().Add(10 * time.Minute)}
	s.gitOAuth.mu.Unlock()
	challenge := sha256.Sum256([]byte(verifier))
	query := url.Values{
		"client_id": {provider.ClientID}, "redirect_uri": {redirectURI}, "response_type": {"code"},
		"scope": {provider.Scope}, "state": {state}, "code_challenge": {base64.RawURLEncoding.EncodeToString(challenge[:])}, "code_challenge_method": {"S256"},
	}
	if providerID == "github" {
		query.Set("prompt", "select_account")
	}
	http.Redirect(w, r, provider.AuthorizeURL+"?"+query.Encode(), http.StatusFound)
}

func (s *Server) finishGitOAuth(w http.ResponseWriter, r *http.Request) {
	providerID := strings.ToLower(strings.TrimSpace(r.PathValue("provider")))
	state := strings.TrimSpace(r.URL.Query().Get("state"))
	s.gitOAuth.mu.Lock()
	attempt, ok := s.gitOAuth.attempts[state]
	delete(s.gitOAuth.attempts, state)
	s.gitOAuth.mu.Unlock()
	if !ok || attempt.Provider != providerID || time.Now().After(attempt.ExpiresAt) {
		writeError(w, fmt.Errorf("OAuth state is invalid or expired: %w", store.ErrConflict))
		return
	}
	if providerError := strings.TrimSpace(r.URL.Query().Get("error")); providerError != "" {
		http.Redirect(w, r, "/?git_oauth="+url.QueryEscape(providerError)+"#git", http.StatusFound)
		return
	}
	token, err := s.gitOAuth.exchange(r.Context(), providerID, r.URL.Query().Get("code"), attempt, "authorization_code")
	if err != nil {
		s.logger.Warn("Git OAuth exchange failed", "provider", providerID, "error", err)
		http.Redirect(w, r, "/?git_oauth=failed#git", http.StatusFound)
		return
	}
	account, err := s.gitOAuth.fetchIdentity(r.Context(), providerID, attempt.Operator, attempt.CredentialScope, token)
	if err == nil {
		_, err = s.store.SaveGitAccount(account)
	}
	if err != nil {
		s.logger.Warn("Git OAuth identity failed", "provider", providerID, "error", err)
		http.Redirect(w, r, "/?git_oauth=failed#git", http.StatusFound)
		return
	}
	http.Redirect(w, r, "/?git_oauth=connected#git", http.StatusFound)
}

func (m *gitOAuthManager) callbackURL(r *http.Request, provider string) string {
	base := m.publicURL
	if base == "" {
		scheme := "http"
		if r.TLS != nil {
			scheme = "https"
		}
		base = scheme + "://" + r.Host
	}
	return base + "/api/git/oauth/" + url.PathEscape(provider) + "/callback"
}

type oauthToken struct {
	AccessToken  string `json:"access_token"`
	RefreshToken string `json:"refresh_token"`
	TokenType    string `json:"token_type"`
	Scope        string `json:"scope"`
	ExpiresIn    int64  `json:"expires_in"`
}

func (m *gitOAuthManager) exchange(ctx context.Context, providerID, code string, attempt gitOAuthAttempt, grant string) (oauthToken, error) {
	provider, configured, _ := m.provider(providerID)
	if !configured {
		return oauthToken{}, errors.New("OAuth provider is not configured")
	}
	values := url.Values{"client_id": {provider.ClientID}, "client_secret": {provider.ClientSecret}, "grant_type": {grant}}
	if grant == "authorization_code" {
		if strings.TrimSpace(code) == "" {
			return oauthToken{}, errors.New("OAuth callback has no code")
		}
		values.Set("code", code)
		values.Set("redirect_uri", attempt.RedirectURI)
		values.Set("code_verifier", attempt.Verifier)
	} else {
		values.Set("refresh_token", code)
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, provider.TokenURL, strings.NewReader(values.Encode()))
	if err != nil {
		return oauthToken{}, err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("Accept", "application/json")
	response, err := m.client.Do(req)
	if err != nil {
		return oauthToken{}, err
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		_, _ = io.Copy(io.Discard, io.LimitReader(response.Body, 1<<20))
		return oauthToken{}, fmt.Errorf("OAuth token endpoint returned HTTP %d", response.StatusCode)
	}
	var token oauthToken
	if err := json.NewDecoder(io.LimitReader(response.Body, 1<<20)).Decode(&token); err != nil {
		return oauthToken{}, err
	}
	if strings.TrimSpace(token.AccessToken) == "" {
		return oauthToken{}, errors.New("OAuth provider returned no access token")
	}
	return token, nil
}

func (m *gitOAuthManager) fetchIdentity(ctx context.Context, providerID, operator string, credentialScope domain.CredentialScope, token oauthToken) (domain.GitAccount, error) {
	provider, configured, _ := m.provider(providerID)
	if !configured {
		return domain.GitAccount{}, errors.New("OAuth provider is not configured")
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, provider.UserURL, nil)
	if err != nil {
		return domain.GitAccount{}, err
	}
	req.Header.Set("Authorization", "Bearer "+token.AccessToken)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "EasyACP-Spin")
	response, err := m.client.Do(req)
	if err != nil {
		return domain.GitAccount{}, err
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		_, _ = io.Copy(io.Discard, io.LimitReader(response.Body, 1<<20))
		return domain.GitAccount{}, fmt.Errorf("OAuth user endpoint returned HTTP %d", response.StatusCode)
	}
	var identity struct {
		ID          int64  `json:"id"`
		Login       string `json:"login"`
		Username    string `json:"username"`
		Name        string `json:"name"`
		Email       string `json:"email"`
		CommitEmail string `json:"commit_email"`
	}
	if err := json.NewDecoder(io.LimitReader(response.Body, 1<<20)).Decode(&identity); err != nil {
		return domain.GitAccount{}, err
	}
	login := identity.Login
	if login == "" {
		login = identity.Username
	}
	if login == "" || identity.ID == 0 {
		return domain.GitAccount{}, errors.New("OAuth provider returned no stable user identity")
	}
	email := identity.CommitEmail
	if email == "" {
		email = identity.Email
	}
	if email == "" && providerID == "github" {
		email = login + "@users.noreply.github.com"
	}
	account := domain.GitAccount{
		Operator: operator, Provider: providerID, Host: provider.Host, ProviderID: strconv.FormatInt(identity.ID, 10),
		Login: login, Name: identity.Name, Email: email, AccessToken: token.AccessToken,
		RefreshToken: token.RefreshToken, TokenType: token.TokenType, Scope: token.Scope, CredentialScope: credentialScope,
	}
	if token.ExpiresIn > 0 {
		expires := time.Now().UTC().Add(time.Duration(token.ExpiresIn) * time.Second)
		account.ExpiresAt = &expires
	}
	return account, nil
}

func (s *Server) saveGitOAuthConfiguration(w http.ResponseWriter, r *http.Request) {
	identity, ok := identityFromRequest(r)
	if !ok || identity.User.Role != domain.UserAdmin {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "admin role required"})
		return
	}
	var req domain.SaveGitOAuthConfigurationRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Provider = r.PathValue("provider")
	configuration, err := s.store.SaveGitOAuthConfiguration(identity.User.ID, req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, configuration)
}

func (s *Server) deleteGitOAuthConfiguration(w http.ResponseWriter, r *http.Request) {
	identity, ok := identityFromRequest(r)
	if !ok || identity.User.Role != domain.UserAdmin {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "admin role required"})
		return
	}
	configuration, err := s.store.DeleteGitOAuthConfiguration(identity.User.ID, r.PathValue("provider"))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, configuration)
}

func (s *Server) gitAccountForCheckout(ctx context.Context, accountID, operator string) (domain.GitAccount, error) {
	account, err := s.store.GitAccount(accountID, operator)
	if err != nil {
		return domain.GitAccount{}, err
	}
	if account.ExpiresAt == nil || account.ExpiresAt.After(time.Now().Add(time.Minute)) {
		return account, nil
	}
	if account.RefreshToken == "" {
		return domain.GitAccount{}, errors.New("Git OAuth token expired; reconnect the account")
	}
	token, err := s.gitOAuth.exchange(ctx, account.Provider, account.RefreshToken, gitOAuthAttempt{}, "refresh_token")
	if err != nil {
		return domain.GitAccount{}, fmt.Errorf("refresh Git OAuth token: %w", err)
	}
	account.AccessToken = token.AccessToken
	if token.RefreshToken != "" {
		account.RefreshToken = token.RefreshToken
	}
	account.TokenType = token.TokenType
	account.Scope = token.Scope
	if token.ExpiresIn > 0 {
		expires := time.Now().UTC().Add(time.Duration(token.ExpiresIn) * time.Second)
		account.ExpiresAt = &expires
	} else {
		account.ExpiresAt = nil
	}
	if _, err := s.store.SaveGitAccount(account); err != nil {
		return domain.GitAccount{}, err
	}
	return s.store.GitAccount(account.ID, operator)
}

func (s *Server) gitAccountForWorkspace(ctx context.Context, workspace *domain.GitWorkspace, operator string) (domain.GitAccount, bool, error) {
	if workspace == nil {
		return domain.GitAccount{}, false, nil
	}
	if workspace.CredentialScope == domain.CredentialScopeUser || workspace.CredentialScope == domain.CredentialScopeGlobal {
		account, err := s.store.ResolveGitWorkspaceAccount(*workspace, operator)
		if err != nil {
			return domain.GitAccount{}, true, err
		}
		account, err = s.gitAccountForCheckout(ctx, account.ID, operator)
		return account, true, err
	}
	// Compositions persisted before credential scopes stored the resolved ID.
	if workspace.AccountID != "" {
		account, err := s.gitAccountForCheckout(ctx, workspace.AccountID, operator)
		return account, true, err
	}
	return domain.GitAccount{}, false, nil
}

func randomOAuthValue(bytesCount int) (string, error) {
	buffer := make([]byte, bytesCount)
	if _, err := rand.Read(buffer); err != nil {
		return "", err
	}
	return base64.RawURLEncoding.EncodeToString(buffer), nil
}
