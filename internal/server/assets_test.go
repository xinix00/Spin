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
	dashboard := dashboardResponse.Body.String()
	assetPrefix := "/assets/v" + frontendAssetVersion + "/"
	if got := dashboardResponse.Header().Get("X-Spin-UI-Version"); got != frontendAssetVersion {
		t.Fatalf("dashboard UI version = %q, want %q", got, frontendAssetVersion)
	}
	assertNotCached(t, dashboardResponse)
	if strings.Contains(dashboard, "__SPIN_UI_VERSION__") {
		t.Fatal("dashboard still contains the frontend-version placeholder")
	}
	for _, asset := range []string{"material-symbols-outlined.css", "spin.css", "marked-18.0.11.js", "dompurify-3.4.14.min.js", "spin.js"} {
		if !strings.Contains(dashboardResponse.Body.String(), asset) {
			t.Fatalf("dashboard does not reference %s", asset)
		}
		if !strings.Contains(dashboard, assetPrefix) {
			t.Fatalf("dashboard does not use versioned asset prefix %q", assetPrefix)
		}
	}

	var applicationScript, renderingScript string
	for _, path := range []string{
		assetPrefix + "spin.css",
		assetPrefix + "spin.js",
		assetPrefix + "ui/app.js",
		assetPrefix + "ui/render.js",
		assetPrefix + "ui/navigation.js",
		assetPrefix + "ui/appearance.js",
		assetPrefix + "vendor/material-symbols-outlined.css",
		assetPrefix + "vendor/material-symbols-outlined.woff2",
		assetPrefix + "vendor/tactile-theme.css",
		assetPrefix + "vendor/tactile-matte-material.css",
		assetPrefix + "vendor/tactile95-material.css",
		assetPrefix + "vendor/tactile-picker.js",
		assetPrefix + "vendor/markdown-1.6.1.js",
		assetPrefix + "vendor/markdown-1.6.1.css",
		assetPrefix + "vendor/tactile-elements.css",
		assetPrefix + "vendor/tactile-components.css",
		assetPrefix + "vendor/tactile-select.js",
		assetPrefix + "vendor/tactile-dialog.js",
		assetPrefix + "vendor/tactile-combobox.js",
		assetPrefix + "vendor/tactile-markdown.js",
		assetPrefix + "vendor/marked-18.0.11.js",
		assetPrefix + "vendor/dompurify-3.4.14.min.js",
		assetPrefix + "vendor/xterm-5.5.0.js",
		assetPrefix + "vendor/xterm-5.5.0.css",
		assetPrefix + "vendor/xterm-addon-fit-0.10.0.js",
		assetPrefix + "vendor/mermaid-11.17.2.min.js",
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
		if path == assetPrefix+"ui/app.js" {
			applicationScript = response.Body.String()
		}
		if path == assetPrefix+"ui/render.js" {
			renderingScript = response.Body.String()
		}
	}

	for _, fallbackPath := range []string{
		"/assets/vendor/marked-18.0.11.js",
		"/assets/v0/vendor/marked-18.0.11.js",
		"/assets/v0/spin.js",
		"/assets/v0/ui/app.js",
		"/assets/v0/ui/render.js",
		"/assets/v0/ui/navigation.js",
	} {
		fallbackRequest := httptest.NewRequest(http.MethodGet, fallbackPath, nil)
		fallbackResponse := httptest.NewRecorder()
		server.Handler().ServeHTTP(fallbackResponse, fallbackRequest)
		if fallbackResponse.Code != http.StatusOK || fallbackResponse.Body.Len() == 0 {
			t.Fatalf("fallback GET %s = %d, %d bytes", fallbackPath, fallbackResponse.Code, fallbackResponse.Body.Len())
		}
		assertNotCached(t, fallbackResponse)
	}

	if strings.Contains(dashboard, "cdn.jsdelivr.net") || strings.Contains(dashboard, "@latest") {
		t.Fatal("dashboard must use local, pinned Markdown assets")
	}
	if strings.Contains(dashboard, "fonts.googleapis.com") {
		t.Fatal("dashboard still depends on the external Google Fonts stylesheet")
	}
	for _, obsolete := range []string{"Agent workbench", "Snapshots → Sessions → reviewable Git result"} {
		if strings.Contains(dashboard, obsolete) {
			t.Fatalf("dashboard still contains obsolete header %q", obsolete)
		}
	}
	for _, expected := range []string{"class=\"job-created\"", "class=\"workflow-step", "job.workflow_status==='busy'", "stepIsRunning"} {
		if !strings.Contains(applicationScript, expected) {
			t.Fatalf("application script does not contain workflow refinement %q", expected)
		}
	}
	if !strings.Contains(renderingScript, "mermaid-11.17.2.min.js") {
		t.Fatal("rendering script does not reference the pinned Mermaid asset")
	}

	apiRequest := httptest.NewRequest(http.MethodGet, "/api/auth/status", nil)
	apiResponse := httptest.NewRecorder()
	server.Handler().ServeHTTP(apiResponse, apiRequest)
	assertNotCached(t, apiResponse)
}

func assertNotCached(t *testing.T, response *httptest.ResponseRecorder) {
	t.Helper()
	if cache := response.Header().Get("Cache-Control"); !strings.Contains(cache, "no-store") || !strings.Contains(cache, "must-revalidate") {
		t.Fatalf("cache-control = %q", cache)
	}
	for _, header := range []string{"CDN-Cache-Control", "Cloudflare-CDN-Cache-Control", "Surrogate-Control"} {
		if got := response.Header().Get(header); got != "no-store" {
			t.Fatalf("%s = %q, want no-store", header, got)
		}
	}
}
