package server

import (
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"easyacp/internal/store"
)

func TestDashboardServesPinnedRichMarkdownAssets(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	server := New(st, slog.New(slog.NewTextHandler(io.Discard, nil)))

	dashboardRequest := httptest.NewRequest(http.MethodGet, "/", nil)
	dashboardResponse := httptest.NewRecorder()
	server.Handler().ServeHTTP(dashboardResponse, dashboardRequest)
	if dashboardResponse.Code != http.StatusOK {
		t.Fatalf("dashboard status = %d", dashboardResponse.Code)
	}
	for _, asset := range []string{"material-symbols-outlined.css", "marked-18.0.11.js", "dompurify-3.4.14.min.js", "mermaid-11.17.2.min.js"} {
		if !strings.Contains(dashboardResponse.Body.String(), asset) {
			t.Fatalf("dashboard does not reference %s", asset)
		}
	}

	for _, path := range []string{
		"/assets/vendor/material-symbols-outlined.css",
		"/assets/vendor/material-symbols-outlined.woff2",
		"/assets/vendor/marked-18.0.11.js",
		"/assets/vendor/dompurify-3.4.14.min.js",
		"/assets/vendor/mermaid-11.17.2.min.js",
	} {
		request := httptest.NewRequest(http.MethodGet, path, nil)
		response := httptest.NewRecorder()
		server.Handler().ServeHTTP(response, request)
		if response.Code != http.StatusOK || response.Body.Len() == 0 {
			t.Fatalf("GET %s = %d, %d bytes", path, response.Code, response.Body.Len())
		}
		if cache := response.Header().Get("Cache-Control"); !strings.Contains(cache, "immutable") {
			t.Fatalf("GET %s cache-control = %q", path, cache)
		}
	}
	if strings.Contains(dashboardResponse.Body.String(), "fonts.googleapis.com") {
		t.Fatal("dashboard still depends on the external Google Fonts stylesheet")
	}
	dashboard := dashboardResponse.Body.String()
	for _, obsolete := range []string{"Agent workbench", "Snapshots → Sessions → reviewable Git result"} {
		if strings.Contains(dashboard, obsolete) {
			t.Fatalf("dashboard still contains obsolete header %q", obsolete)
		}
	}
	for _, expected := range []string{"class=\"job-created\"", "class=\"workflow-step", "job.workflow_status==='busy'", "stepIsRunning"} {
		if !strings.Contains(dashboard, expected) {
			t.Fatalf("dashboard does not contain workflow refinement %q", expected)
		}
	}
}
