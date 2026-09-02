package server

import (
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"encoding/hex"
	"fmt"
	"hash"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

const (
	passwordIterations = 600_000
	sessionLifetime    = 24 * time.Hour
	csrfHeader         = "X-Spin-CSRF"
)

type authContextKey struct{}
type workerContextKey struct{}

type authenticatedIdentity struct {
	User    domain.User
	Session domain.AuthSession
}

type loginAttempt struct {
	Failures     int
	WindowStart  time.Time
	BlockedUntil time.Time
}

type loginLimiter struct {
	mu       sync.Mutex
	attempts map[string]loginAttempt
}

type authStatusResponse struct {
	Configured    bool               `json:"configured"`
	Authenticated bool               `json:"authenticated"`
	User          *domain.PublicUser `json:"user,omitempty"`
	CSRFToken     string             `json:"csrf_token,omitempty"`
}

func (s *Server) authentication(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if s.authDisabled || !strings.HasPrefix(r.URL.Path, "/api/") || publicAPIPath(r) {
			next.ServeHTTP(w, r)
			return
		}
		if workerAPIPath(r) && s.validWorkerBearer(r.Header.Get("Authorization")) {
			next.ServeHTTP(w, r.WithContext(context.WithValue(r.Context(), workerContextKey{}, true)))
			return
		}
		identity, err := s.requestIdentity(r)
		if err != nil {
			writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "authentication required"})
			return
		}
		if isMutation(r.Method) {
			provided := secretHash(r.Header.Get(csrfHeader))
			if subtle.ConstantTimeCompare([]byte(provided), []byte(identity.Session.CSRFHash)) != 1 || !sameOriginRequest(r) {
				writeJSON(w, http.StatusForbidden, map[string]string{"error": "invalid CSRF token or request origin"})
				return
			}
		}
		next.ServeHTTP(w, r.WithContext(context.WithValue(r.Context(), authContextKey{}, identity)))
	})
}

func publicAPIPath(r *http.Request) bool {
	if r.Method == http.MethodPost && strings.HasPrefix(r.URL.Path, "/api/workflow/mcp/") {
		return true // the workflow handler validates its short-lived bearer token
	}
	if r.URL.Path == "/healthz" || r.URL.Path == "/api/auth/status" || r.URL.Path == "/api/auth/setup" || r.URL.Path == "/api/auth/login" {
		return true
	}
	return r.Method == http.MethodGet && strings.HasPrefix(r.URL.Path, "/api/git/oauth/") && strings.HasSuffix(r.URL.Path, "/callback")
}

func workerAPIPath(r *http.Request) bool {
	path := r.URL.Path
	return path == "/api/runner/ws" || path == "/api/clients/register" || path == "/api/sessions/claim" || strings.HasPrefix(path, "/api/sessions/") || strings.HasPrefix(path, "/api/activations/")
}

func isMutation(method string) bool {
	return method == http.MethodPost || method == http.MethodPut || method == http.MethodPatch || method == http.MethodDelete
}

func sameOriginRequest(r *http.Request) bool {
	origin := strings.TrimSpace(r.Header.Get("Origin"))
	if origin == "" {
		// CLI clients commonly omit Origin. Cookie-authenticated mutations still
		// require the non-simple CSRF header, which cross-origin forms cannot set.
		return true
	}
	parsed, err := url.Parse(origin)
	return err == nil && strings.EqualFold(parsed.Host, r.Host)
}

func (s *Server) requestIdentity(r *http.Request) (authenticatedIdentity, error) {
	for _, name := range []string{"__Host-spin_session", "spin_session"} {
		cookie, err := r.Cookie(name)
		if err != nil || cookie.Value == "" {
			continue
		}
		user, session, err := s.store.AuthenticateSession(secretHash(cookie.Value))
		if err == nil {
			return authenticatedIdentity{User: user, Session: session}, nil
		}
	}
	return authenticatedIdentity{}, store.ErrNotFound
}

func identityFromRequest(r *http.Request) (authenticatedIdentity, bool) {
	identity, ok := r.Context().Value(authContextKey{}).(authenticatedIdentity)
	return identity, ok
}

func (s *Server) requestOperator(r *http.Request, fallback string) string {
	if identity, ok := identityFromRequest(r); ok {
		return identity.User.Username
	}
	return normalizeOperator(fallback)
}

func (s *Server) requestUserID(r *http.Request) string {
	if identity, ok := identityFromRequest(r); ok {
		return identity.User.ID
	}
	return ""
}

func (s *Server) validWorkerBearer(header string) bool {
	if s.workerToken == "" || !strings.HasPrefix(header, "Bearer ") {
		return false
	}
	provided := strings.TrimSpace(strings.TrimPrefix(header, "Bearer "))
	return subtle.ConstantTimeCompare([]byte(secretHash(provided)), []byte(secretHash(s.workerToken))) == 1
}

func (s *Server) authStatus(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Cache-Control", "no-store")
	status := authStatusResponse{Configured: s.store.HasUsers()}
	if !status.Configured {
		writeJSON(w, http.StatusOK, status)
		return
	}
	identity, err := s.requestIdentity(r)
	if err == nil {
		user := publicUser(identity.User)
		status.Authenticated = true
		status.User = &user
		status.CSRFToken = s.csrfTokens.get(identity.Session.ID)
		if status.CSRFToken == "" {
			// Sessions survive restarts, while plaintext CSRF values intentionally
			// do not. Rotate to a fresh session transparently.
			_ = s.store.DeleteAuthSession(identity.Session.ID)
			s.issueSession(w, r, identity.User, &status)
		}
	}
	writeJSON(w, http.StatusOK, status)
}

func (s *Server) setupOwner(w http.ResponseWriter, r *http.Request) {
	if s.store.HasUsers() {
		writeError(w, fmt.Errorf("owner setup is already complete: %w", store.ErrConflict))
		return
	}
	if !sameOriginRequest(r) {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "invalid request origin"})
		return
	}
	var req domain.SetupUserRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	passwordHash, err := hashPassword(req.Password)
	if err != nil {
		writeError(w, err)
		return
	}
	created, err := s.store.CreateInitialUser(domain.User{Username: req.Username, DisplayName: req.DisplayName, PasswordHash: passwordHash})
	if err != nil {
		writeError(w, err)
		return
	}
	user, _ := s.store.User(created.ID)
	status := authStatusResponse{Configured: true, Authenticated: true}
	if err := s.issueSession(w, r, user, &status); err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, status)
}

func (s *Server) login(w http.ResponseWriter, r *http.Request) {
	if !sameOriginRequest(r) {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "invalid request origin"})
		return
	}
	key := clientAddress(r)
	if retryAfter := s.loginLimiter.blocked(key); retryAfter > 0 {
		w.Header().Set("Retry-After", strconv.Itoa(int(retryAfter.Seconds())+1))
		writeJSON(w, http.StatusTooManyRequests, map[string]string{"error": "too many login attempts; try again shortly"})
		return
	}
	var req domain.LoginRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	user, lookupErr := s.store.UserByUsername(req.Username)
	hash := fakePasswordHash
	if lookupErr == nil {
		hash = user.PasswordHash
	}
	valid := verifyPassword(req.Password, hash)
	if lookupErr != nil || !valid {
		s.loginLimiter.failed(key)
		writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "invalid username or password"})
		return
	}
	s.loginLimiter.succeeded(key)
	status := authStatusResponse{Configured: true, Authenticated: true}
	if err := s.issueSession(w, r, user, &status); err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, status)
}

func (s *Server) logout(w http.ResponseWriter, r *http.Request) {
	if identity, ok := identityFromRequest(r); ok {
		_ = s.store.DeleteAuthSession(identity.Session.ID)
		s.csrfTokens.delete(identity.Session.ID)
	}
	for _, name := range []string{"__Host-spin_session", "spin_session"} {
		http.SetCookie(w, &http.Cookie{Name: name, Value: "", Path: "/", MaxAge: -1, HttpOnly: true, Secure: name == "__Host-spin_session", SameSite: http.SameSiteLaxMode})
	}
	w.WriteHeader(http.StatusNoContent)
}

func (s *Server) createUser(w http.ResponseWriter, r *http.Request) {
	identity, ok := identityFromRequest(r)
	if !ok || identity.User.Role != domain.UserAdmin {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "admin role required"})
		return
	}
	var req domain.CreateUserRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	passwordHash, err := hashPassword(req.Password)
	if err != nil {
		writeError(w, err)
		return
	}
	created, err := s.store.CreateUser(identity.User.ID, domain.User{Username: req.Username, DisplayName: req.DisplayName, Role: req.Role, PasswordHash: passwordHash})
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, created)
}

func (s *Server) issueSession(w http.ResponseWriter, r *http.Request, user domain.User, status *authStatusResponse) error {
	token, err := randomOAuthValue(32)
	if err != nil {
		return err
	}
	csrf, err := randomOAuthValue(32)
	if err != nil {
		return err
	}
	expires := time.Now().UTC().Add(sessionLifetime)
	session, err := s.store.CreateAuthSession(user.ID, secretHash(token), secretHash(csrf), expires)
	if err != nil {
		return err
	}
	secure := r.TLS != nil || strings.HasPrefix(strings.ToLower(s.gitOAuth.publicURL), "https://")
	name := "spin_session"
	if secure {
		name = "__Host-spin_session"
	}
	http.SetCookie(w, &http.Cookie{Name: name, Value: token, Path: "/", Expires: expires, MaxAge: int(sessionLifetime.Seconds()), HttpOnly: true, Secure: secure, SameSite: http.SameSiteLaxMode})
	s.csrfTokens.set(session.ID, csrf)
	public := publicUser(user)
	status.User = &public
	status.CSRFToken = csrf
	return nil
}

func hashPassword(password string) (string, error) {
	if len(password) < 12 || len(password) > 256 {
		return "", fmt.Errorf("password must contain 12 to 256 characters: %w", store.ErrConflict)
	}
	salt := make([]byte, 16)
	if _, err := rand.Read(salt); err != nil {
		return "", err
	}
	derived := pbkdf2SHA256([]byte(password), salt, passwordIterations, 32)
	return fmt.Sprintf("pbkdf2-sha256$%d$%s$%s", passwordIterations, base64.RawStdEncoding.EncodeToString(salt), base64.RawStdEncoding.EncodeToString(derived)), nil
}

func verifyPassword(password, encoded string) bool {
	parts := strings.Split(encoded, "$")
	if len(parts) != 4 || parts[0] != "pbkdf2-sha256" {
		return false
	}
	iterations, err := strconv.Atoi(parts[1])
	salt, saltErr := base64.RawStdEncoding.DecodeString(parts[2])
	expected, hashErr := base64.RawStdEncoding.DecodeString(parts[3])
	if err != nil || saltErr != nil || hashErr != nil || iterations < 1 || iterations > 2_000_000 || len(expected) != 32 || len(password) > 256 {
		return false
	}
	actual := pbkdf2SHA256([]byte(password), salt, iterations, len(expected))
	return subtle.ConstantTimeCompare(actual, expected) == 1
}

func pbkdf2SHA256(password, salt []byte, iterations, length int) []byte {
	return pbkdf2Key(password, salt, iterations, length, sha256.New)
}

func pbkdf2Key(password, salt []byte, iterations, length int, newHash func() hash.Hash) []byte {
	prf := hmac.New(newHash, password)
	hashLength := prf.Size()
	blocks := (length + hashLength - 1) / hashLength
	result := make([]byte, 0, blocks*hashLength)
	buffer := make([]byte, 4)
	for block := 1; block <= blocks; block++ {
		prf.Reset()
		_, _ = prf.Write(salt)
		buffer[0], buffer[1], buffer[2], buffer[3] = byte(block>>24), byte(block>>16), byte(block>>8), byte(block)
		_, _ = prf.Write(buffer)
		u := prf.Sum(nil)
		t := append([]byte{}, u...)
		for iteration := 1; iteration < iterations; iteration++ {
			prf.Reset()
			_, _ = prf.Write(u)
			u = prf.Sum(u[:0])
			for index := range t {
				t[index] ^= u[index]
			}
		}
		result = append(result, t...)
	}
	return result[:length]
}

func secretHash(value string) string {
	sum := sha256.Sum256([]byte(value))
	return hex.EncodeToString(sum[:])
}

func publicUser(user domain.User) domain.PublicUser {
	return domain.PublicUser{ID: user.ID, Username: user.Username, DisplayName: user.DisplayName, Role: user.Role, CreatedAt: user.CreatedAt}
}

func clientAddress(r *http.Request) string {
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err == nil {
		return host
	}
	return r.RemoteAddr
}

func (l *loginLimiter) blocked(key string) time.Duration {
	l.mu.Lock()
	defer l.mu.Unlock()
	attempt := l.attempts[key]
	return time.Until(attempt.BlockedUntil).Truncate(time.Second)
}

func (l *loginLimiter) failed(key string) {
	l.mu.Lock()
	defer l.mu.Unlock()
	now := time.Now()
	attempt := l.attempts[key]
	if now.Sub(attempt.WindowStart) > 5*time.Minute {
		attempt = loginAttempt{WindowStart: now}
	}
	attempt.Failures++
	if attempt.Failures >= 5 {
		attempt.BlockedUntil = now.Add(time.Minute)
	}
	l.attempts[key] = attempt
}

func (l *loginLimiter) succeeded(key string) {
	l.mu.Lock()
	delete(l.attempts, key)
	l.mu.Unlock()
}

type csrfTokenCache struct {
	mu     sync.Mutex
	values map[string]string
}

func (c *csrfTokenCache) get(sessionID string) string {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.values[sessionID]
}

func (c *csrfTokenCache) set(sessionID, token string) {
	c.mu.Lock()
	c.values[sessionID] = token
	c.mu.Unlock()
}

func (c *csrfTokenCache) delete(sessionID string) {
	c.mu.Lock()
	delete(c.values, sessionID)
	c.mu.Unlock()
}

var fakePasswordHash = func() string {
	// Constant valid encoding used to equalize unknown-user login work.
	salt := make([]byte, 16)
	derived := pbkdf2SHA256([]byte("not-a-real-password"), salt, passwordIterations, 32)
	return fmt.Sprintf("pbkdf2-sha256$%d$%s$%s", passwordIterations, base64.RawStdEncoding.EncodeToString(salt), base64.RawStdEncoding.EncodeToString(derived))
}()
