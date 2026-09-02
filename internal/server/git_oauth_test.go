package server

import (
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

func TestGitHubOAuthConnectsRedactedUserAccount(t *testing.T) {
	var tokenForm url.Values
	httpClient := &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
		response := &http.Response{StatusCode: http.StatusOK, Header: make(http.Header)}
		response.Header.Set("Content-Type", "application/json")
		switch r.URL.Path {
		case "/token":
			if err := r.ParseForm(); err != nil {
				t.Fatal(err)
			}
			tokenForm = r.PostForm
			response.Body = io.NopCloser(strings.NewReader(`{"access_token":"oauth-secret","token_type":"bearer","scope":"repo"}`))
		case "/user":
			if r.Header.Get("Authorization") != "Bearer oauth-secret" {
				t.Fatalf("authorization = %q", r.Header.Get("Authorization"))
			}
			response.Body = io.NopCloser(strings.NewReader(`{"id":42,"login":"octo","name":"Octo Cat","email":"octo@example.com"}`))
		default:
			response.StatusCode = http.StatusNotFound
			response.Body = io.NopCloser(strings.NewReader(`{}`))
		}
		return response, nil
	})}

	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), capsule.Journal{}, ServerOptions{
		PublicURL: "http://spin.example", HTTPClient: httpClient,
		GitOAuthProviders: map[string]GitOAuthProviderConfig{
			"github": {Name: "GitHub", ClientID: "client-id", ClientSecret: "client-secret", AuthorizeURL: "https://provider.example/authorize", TokenURL: "https://provider.example/token", UserURL: "https://provider.example/user", Scope: "repo", Host: "github.com"},
		},
	})
	setup := httptest.NewRequest(http.MethodPost, "/api/auth/setup", strings.NewReader(`{"username":"derek","display_name":"Derek","password":"correct horse battery staple"}`))
	setup.Header.Set("Content-Type", "application/json")
	setupResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(setupResponse, setup)
	if setupResponse.Code != http.StatusCreated || len(setupResponse.Result().Cookies()) != 1 {
		t.Fatalf("setup status = %d; body = %s", setupResponse.Code, setupResponse.Body.String())
	}
	sessionCookie := setupResponse.Result().Cookies()[0]

	start := httptest.NewRequest(http.MethodGet, "/api/git/oauth/github/start", nil)
	start.AddCookie(sessionCookie)
	startResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(startResponse, start)
	if startResponse.Code != http.StatusFound {
		t.Fatalf("start status = %d; body = %s", startResponse.Code, startResponse.Body.String())
	}
	authorizeURL, err := url.Parse(startResponse.Header().Get("Location"))
	if err != nil {
		t.Fatal(err)
	}
	state := authorizeURL.Query().Get("state")
	if state == "" || authorizeURL.Query().Get("code_challenge") == "" || authorizeURL.Query().Get("code_challenge_method") != "S256" {
		t.Fatalf("authorize URL has no state/PKCE: %s", authorizeURL)
	}
	callback := httptest.NewRequest(http.MethodGet, "/api/git/oauth/github/callback?state="+url.QueryEscape(state)+"&code=one-time-code", nil)
	callbackResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(callbackResponse, callback)
	if callbackResponse.Code != http.StatusFound || !strings.Contains(callbackResponse.Header().Get("Location"), "git_oauth=connected") {
		t.Fatalf("callback status = %d; location = %s", callbackResponse.Code, callbackResponse.Header().Get("Location"))
	}
	if tokenForm.Get("code") != "one-time-code" || tokenForm.Get("code_verifier") == "" || tokenForm.Get("client_secret") != "client-secret" {
		t.Fatalf("token exchange form = %+v", tokenForm)
	}
	snapshot := st.Snapshot()
	if len(snapshot.GitAccounts) != 1 || snapshot.GitAccounts[0].Operator != "derek" || snapshot.GitAccounts[0].Login != "octo" || snapshot.GitAccounts[0].CredentialScope != domain.CredentialScopeUser || snapshot.GitAccounts[0].AccessToken != "" {
		t.Fatalf("public account = %+v", snapshot.GitAccounts)
	}
	private, err := st.GitAccount(snapshot.GitAccounts[0].ID, "derek")
	if err != nil || private.AccessToken != "oauth-secret" {
		t.Fatalf("private account = %+v; error = %v", private, err)
	}

	stateRequest := httptest.NewRequest(http.MethodGet, "/api/state", nil)
	stateRequest.AddCookie(sessionCookie)
	stateResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(stateResponse, stateRequest)
	if strings.Contains(stateResponse.Body.String(), "oauth-secret") || strings.Contains(stateResponse.Body.String(), "client-secret") {
		t.Fatal("state endpoint leaked an OAuth secret")
	}
	var stateBody struct {
		Providers []gitOAuthProviderInfo `json:"git_oauth_providers"`
	}
	if err := json.Unmarshal(stateResponse.Body.Bytes(), &stateBody); err != nil || len(stateBody.Providers) != 2 || stateBody.Providers[0].ID != "github" || !stateBody.Providers[0].Configured || stateBody.Providers[1].Configured {
		t.Fatalf("OAuth providers = %+v; error = %v", stateBody.Providers, err)
	}
}

type roundTripFunc func(*http.Request) (*http.Response, error)

func (fn roundTripFunc) RoundTrip(request *http.Request) (*http.Response, error) {
	return fn(request)
}
