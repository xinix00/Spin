package server

import (
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

func TestAuthenticationBindsOperatorFiltersUserStateAndProtectsMutations(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), capsule.Journal{}, ServerOptions{})
	handler := srv.Handler()

	unauthenticated := httptest.NewRecorder()
	handler.ServeHTTP(unauthenticated, httptest.NewRequest(http.MethodGet, "/api/state", nil))
	if unauthenticated.Code != http.StatusUnauthorized {
		t.Fatalf("unauthenticated state status = %d; body = %s", unauthenticated.Code, unauthenticated.Body.String())
	}
	publicRestoreStatus := httptest.NewRecorder()
	handler.ServeHTTP(publicRestoreStatus, httptest.NewRequest(http.MethodGet, "/api/restores/unguessable-status-id", nil))
	if publicRestoreStatus.Code != http.StatusNotFound {
		t.Fatalf("public restore status = %d; body = %s", publicRestoreStatus.Code, publicRestoreStatus.Body.String())
	}

	derekCookie, derekCSRF := setupTestOwner(t, handler, "derek", "correct horse battery staple")
	withoutCSRF := newAuthenticatedRequest(http.MethodPost, "/api/mcp-servers", `{"operator":"john","name":"private","transport":"http","url":"https://mcp.example.test"}`, derekCookie, "")
	withoutCSRFResponse := httptest.NewRecorder()
	handler.ServeHTTP(withoutCSRFResponse, withoutCSRF)
	if withoutCSRFResponse.Code != http.StatusForbidden {
		t.Fatalf("mutation without CSRF status = %d; body = %s", withoutCSRFResponse.Code, withoutCSRFResponse.Body.String())
	}

	crossOrigin := newAuthenticatedRequest(http.MethodPost, "/api/mcp-servers", `{"name":"private","transport":"http","url":"https://mcp.example.test"}`, derekCookie, derekCSRF)
	crossOrigin.Header.Set("Origin", "https://evil.example")
	crossOriginResponse := httptest.NewRecorder()
	handler.ServeHTTP(crossOriginResponse, crossOrigin)
	if crossOriginResponse.Code != http.StatusForbidden {
		t.Fatalf("cross-origin mutation status = %d; body = %s", crossOriginResponse.Code, crossOriginResponse.Body.String())
	}

	createMCP := newAuthenticatedRequest(http.MethodPost, "/api/mcp-servers", `{"operator":"john","name":"private","transport":"http","url":"https://mcp.example.test","headers":[{"name":"Authorization","value":"Bearer private"}]}`, derekCookie, derekCSRF)
	createMCPResponse := httptest.NewRecorder()
	handler.ServeHTTP(createMCPResponse, createMCP)
	if createMCPResponse.Code != http.StatusCreated {
		t.Fatalf("create MCP status = %d; body = %s", createMCPResponse.Code, createMCPResponse.Body.String())
	}
	var mcp domain.MCPServer
	if err := json.Unmarshal(createMCPResponse.Body.Bytes(), &mcp); err != nil || mcp.Operator != "derek" {
		t.Fatalf("server-bound MCP operator = %q; error = %v", mcp.Operator, err)
	}

	configureOAuth := newAuthenticatedRequest(http.MethodPut, "/api/git/oauth/github/configuration", `{"client_id":"app-client-id","client_secret":"app-client-secret"}`, derekCookie, derekCSRF)
	configureOAuthResponse := httptest.NewRecorder()
	handler.ServeHTTP(configureOAuthResponse, configureOAuth)
	if configureOAuthResponse.Code != http.StatusOK || strings.Contains(configureOAuthResponse.Body.String(), "app-client-secret") {
		t.Fatalf("admin OAuth configuration status = %d; body = %s", configureOAuthResponse.Code, configureOAuthResponse.Body.String())
	}
	derekState := httptest.NewRecorder()
	handler.ServeHTTP(derekState, newAuthenticatedRequest(http.MethodGet, "/api/state", "", derekCookie, ""))
	var configuredState stateResponse
	if err := json.Unmarshal(derekState.Body.Bytes(), &configuredState); err != nil || len(configuredState.GitOAuthProviders) != 2 || !configuredState.GitOAuthProviders[0].Configured || configuredState.GitOAuthProviders[0].Source != "app" || configuredState.GitOAuthProviders[0].ClientID != "app-client-id" || strings.Contains(derekState.Body.String(), "app-client-secret") {
		t.Fatalf("configured OAuth state = %+v; body = %s; error = %v", configuredState.GitOAuthProviders, derekState.Body.String(), err)
	}

	createRecording := newAuthenticatedRequest(http.MethodPost, "/api/recordings", `{"actor":"john","kind":"tool","name":"spoof-check","scope":"global"}`, derekCookie, derekCSRF)
	createRecordingResponse := httptest.NewRecorder()
	handler.ServeHTTP(createRecordingResponse, createRecording)
	if createRecordingResponse.Code != http.StatusCreated {
		t.Fatalf("create recording status = %d; body = %s", createRecordingResponse.Code, createRecordingResponse.Body.String())
	}
	var recording domain.Recording
	if err := json.Unmarshal(createRecordingResponse.Body.Bytes(), &recording); err != nil || recording.Actor != "derek" {
		t.Fatalf("server-bound recording actor = %q; error = %v", recording.Actor, err)
	}

	createUser := newAuthenticatedRequest(http.MethodPost, "/api/auth/users", `{"username":"john","display_name":"John","password":"another secure passphrase","role":"member"}`, derekCookie, derekCSRF)
	createUserResponse := httptest.NewRecorder()
	handler.ServeHTTP(createUserResponse, createUser)
	if createUserResponse.Code != http.StatusCreated {
		t.Fatalf("create user status = %d; body = %s", createUserResponse.Code, createUserResponse.Body.String())
	}

	johnCookie, johnCSRF := loginTestUser(t, handler, "john", "another secure passphrase")
	johnStateRequest := newAuthenticatedRequest(http.MethodGet, "/api/state", "", johnCookie, "")
	johnStateResponse := httptest.NewRecorder()
	handler.ServeHTTP(johnStateResponse, johnStateRequest)
	if johnStateResponse.Code != http.StatusOK {
		t.Fatalf("john state status = %d; body = %s", johnStateResponse.Code, johnStateResponse.Body.String())
	}
	var johnState stateResponse
	if err := json.Unmarshal(johnStateResponse.Body.Bytes(), &johnState); err != nil {
		t.Fatal(err)
	}
	if johnState.CurrentUser.Username != "john" || len(johnState.MCPServers) != 0 || len(johnState.Recordings) != 0 {
		t.Fatalf("john received another user's state: current=%+v mcp=%+v recordings=%+v", johnState.CurrentUser, johnState.MCPServers, johnState.Recordings)
	}

	memberOAuth := newAuthenticatedRequest(http.MethodPut, "/api/git/oauth/github/configuration", `{"client_id":"id","client_secret":"secret"}`, johnCookie, johnCSRF)
	memberOAuthResponse := httptest.NewRecorder()
	handler.ServeHTTP(memberOAuthResponse, memberOAuth)
	if memberOAuthResponse.Code != http.StatusForbidden {
		t.Fatalf("member OAuth configuration status = %d; body = %s", memberOAuthResponse.Code, memberOAuthResponse.Body.String())
	}
}

func TestAdminCanArchiveAndRestoreUserWithoutDeletingIdentity(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	handler := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), capsule.Journal{}, ServerOptions{}).Handler()
	ownerCookie, ownerCSRF := setupTestOwner(t, handler, "derek", "correct horse battery staple")
	create := newAuthenticatedRequest(http.MethodPost, "/api/auth/users", `{"username":"john","display_name":"John","password":"another secure passphrase","role":"member"}`, ownerCookie, ownerCSRF)
	createResponse := httptest.NewRecorder()
	handler.ServeHTTP(createResponse, create)
	if createResponse.Code != http.StatusCreated {
		t.Fatalf("create user status = %d; body = %s", createResponse.Code, createResponse.Body.String())
	}
	var john domain.PublicUser
	if err := json.Unmarshal(createResponse.Body.Bytes(), &john); err != nil {
		t.Fatal(err)
	}
	johnCookie, _ := loginTestUser(t, handler, "john", "another secure passphrase")
	archive := newAuthenticatedRequest(http.MethodPost, "/api/auth/users/"+john.ID+"/archive", "", ownerCookie, ownerCSRF)
	archiveResponse := httptest.NewRecorder()
	handler.ServeHTTP(archiveResponse, archive)
	if archiveResponse.Code != http.StatusOK || !strings.Contains(archiveResponse.Body.String(), "archived_at") {
		t.Fatalf("archive status = %d; body = %s", archiveResponse.Code, archiveResponse.Body.String())
	}
	oldSession := httptest.NewRecorder()
	handler.ServeHTTP(oldSession, newAuthenticatedRequest(http.MethodGet, "/api/state", "", johnCookie, ""))
	if oldSession.Code != http.StatusUnauthorized {
		t.Fatalf("archived user's existing session status = %d; body = %s", oldSession.Code, oldSession.Body.String())
	}
	login := httptest.NewRequest(http.MethodPost, "/api/auth/login", strings.NewReader(`{"username":"john","password":"another secure passphrase"}`))
	login.Header.Set("Content-Type", "application/json")
	loginResponse := httptest.NewRecorder()
	handler.ServeHTTP(loginResponse, login)
	if loginResponse.Code != http.StatusUnauthorized {
		t.Fatalf("archived user login status = %d; body = %s", loginResponse.Code, loginResponse.Body.String())
	}
	restore := newAuthenticatedRequest(http.MethodPost, "/api/auth/users/"+john.ID+"/restore", "", ownerCookie, ownerCSRF)
	restoreResponse := httptest.NewRecorder()
	handler.ServeHTTP(restoreResponse, restore)
	if restoreResponse.Code != http.StatusOK || strings.Contains(restoreResponse.Body.String(), "archived_at") {
		t.Fatalf("restore status = %d; body = %s", restoreResponse.Code, restoreResponse.Body.String())
	}
	loginTestUser(t, handler, "john", "another secure passphrase")
}

func TestWorkerEndpointsRequireDedicatedBearerToken(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	handler := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), capsule.Journal{}, ServerOptions{WorkerToken: "worker-only-secret"}).Handler()
	body := `{"name":"worker","capabilities":{"tools":["codex"]}}`
	missing := httptest.NewRecorder()
	handler.ServeHTTP(missing, httptest.NewRequest(http.MethodPost, "/api/clients/register", strings.NewReader(body)))
	if missing.Code != http.StatusUnauthorized {
		t.Fatalf("worker without bearer status = %d; body = %s", missing.Code, missing.Body.String())
	}
	authorizedRequest := httptest.NewRequest(http.MethodPost, "/api/clients/register", strings.NewReader(body))
	authorizedRequest.Header.Set("Authorization", "Bearer worker-only-secret")
	authorized := httptest.NewRecorder()
	handler.ServeHTTP(authorized, authorizedRequest)
	if authorized.Code != http.StatusOK {
		t.Fatalf("worker with bearer status = %d; body = %s", authorized.Code, authorized.Body.String())
	}
}

func setupTestOwner(t *testing.T, handler http.Handler, username, password string) (*http.Cookie, string) {
	t.Helper()
	request := httptest.NewRequest(http.MethodPost, "/api/auth/setup", strings.NewReader(`{"username":"`+username+`","display_name":"Owner","password":"`+password+`"}`))
	request.Header.Set("Content-Type", "application/json")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusCreated || len(response.Result().Cookies()) != 1 {
		t.Fatalf("setup status = %d; body = %s", response.Code, response.Body.String())
	}
	var status authStatusResponse
	if err := json.Unmarshal(response.Body.Bytes(), &status); err != nil || !status.Authenticated || status.CSRFToken == "" {
		t.Fatalf("setup auth status = %+v; error = %v", status, err)
	}
	return response.Result().Cookies()[0], status.CSRFToken
}

func loginTestUser(t *testing.T, handler http.Handler, username, password string) (*http.Cookie, string) {
	t.Helper()
	request := httptest.NewRequest(http.MethodPost, "/api/auth/login", strings.NewReader(`{"username":"`+username+`","password":"`+password+`"}`))
	request.Header.Set("Content-Type", "application/json")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK || len(response.Result().Cookies()) != 1 {
		t.Fatalf("login status = %d; body = %s", response.Code, response.Body.String())
	}
	var status authStatusResponse
	if err := json.Unmarshal(response.Body.Bytes(), &status); err != nil || status.CSRFToken == "" {
		t.Fatalf("login auth status = %+v; error = %v", status, err)
	}
	return response.Result().Cookies()[0], status.CSRFToken
}

func newAuthenticatedRequest(method, target, body string, cookie *http.Cookie, csrf string) *http.Request {
	request := httptest.NewRequest(method, target, strings.NewReader(body))
	request.AddCookie(cookie)
	if body != "" {
		request.Header.Set("Content-Type", "application/json")
	}
	if csrf != "" {
		request.Header.Set(csrfHeader, csrf)
	}
	return request
}
