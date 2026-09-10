package server

import (
	"archive/zip"
	"bytes"
	"context"
	"encoding/base64"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/persistence"
	"easyacp/internal/store"
)

// A visual revision is a zip in the database; the viewer gets its files
// under /preview, each response sandboxed, the entry at the root, and the
// zip itself as the download. A runner fetches the same zip in chunks.
func TestPreviewServesAVisualRevisionFromItsBundleInASandbox(t *testing.T) {
	database, err := persistence.Open(filepath.Join(t.TempDir(), "spin.db"), persistence.OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer database.Close()
	st, err := store.OpenWithBackend("state", store.OpenOptions{MasterKey: base64.RawStdEncoding.EncodeToString(bytes.Repeat([]byte{0x11}, 32))}, database)
	if err != nil {
		t.Fatal(err)
	}
	engine := &deliverableTestEngine{files: map[string][]byte{}}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true, Database: database, WorkerToken: "runner-secret"})
	buildLayers(t, srv, "derek", gitLayer(), agentLayer("agent", "agent-acp"))
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "shop", RemoteURL: "https://example.com/shop.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Design", Phases: []domain.WorkflowPhase{{
		ID: "design", Name: "Ontwerp", Instructions: "Maak de preview", Deliverables: []domain.DeliverableDefinition{{Name: "Website", Kind: domain.DeliverableKindVisual, Required: true}},
		Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	created, err := st.CreateJob(domain.CreateJobRequest{Title: "Shop", Objective: "Preview", Operator: "derek", GitRepositoryID: repository.Repository.ID, EnvironmentSelector: "tool:agent", TemplateID: template.ID})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := st.MarkWorkflowPhaseRunning(created.Session.ID); err != nil {
		t.Fatal(err)
	}
	// The zip a runner would upload, sent through the same chunked path.
	var archive bytes.Buffer
	writer := zip.NewWriter(&archive)
	for name, content := range map[string]string{"index.html": `<!doctype html><link rel="stylesheet" href="style.css"><script src="app.js"></script><h1>Shop</h1>`, "style.css": "h1{color:red}", "app.js": "document.title='shop'"} {
		part, err := writer.Create(name)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := part.Write([]byte(content)); err != nil {
			t.Fatal(err)
		}
	}
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	upload, err := database.BeginBundleUpload(context.Background(), int64(archive.Len()))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := upload.WriteAt(context.Background(), 0, int64(archive.Len()), bytes.NewReader(archive.Bytes())); err != nil {
		t.Fatal(err)
	}
	info, err := upload.Complete(context.Background())
	if err != nil || !strings.HasPrefix(info.Ref, "bundle:") {
		t.Fatalf("bundle upload = %+v, %v", info, err)
	}
	deliverable, err := st.PutWorkflowDeliverable(created.Session.ID, "Website", "", &domain.DeliverableBundle{Ref: info.Ref, Digest: info.Digest, Size: info.Size, Files: 3, Entry: "index.html", ContentType: "text/html; charset=utf-8"})
	if err != nil {
		t.Fatal(err)
	}
	get := func(path string, headers map[string]string) *httptest.ResponseRecorder {
		request := httptest.NewRequest(http.MethodGet, path, nil)
		for key, value := range headers {
			request.Header.Set(key, value)
		}
		response := httptest.NewRecorder()
		srv.Handler().ServeHTTP(response, request)
		return response
	}
	root := get("/preview/"+deliverable.ID+"/", nil)
	if root.Code != http.StatusOK || !strings.Contains(root.Body.String(), "<h1>Shop</h1>") || !strings.HasPrefix(root.Header().Get("Content-Type"), "text/html") {
		t.Fatalf("root = %d %s %s", root.Code, root.Header().Get("Content-Type"), root.Body.String())
	}
	policy := root.Header().Get("Content-Security-Policy")
	for _, directive := range []string{"sandbox allow-scripts", "connect-src 'none'", "form-action 'none'", "frame-ancestors 'self'"} {
		if !strings.Contains(policy, directive) {
			t.Fatalf("preview policy lacks %q: %s", directive, policy)
		}
	}
	style := get("/preview/"+deliverable.ID+"/style.css", nil)
	if style.Code != http.StatusOK || style.Body.String() != "h1{color:red}" || !strings.HasPrefix(style.Header().Get("Content-Type"), "text/css") {
		t.Fatalf("style = %d %s %s", style.Code, style.Header().Get("Content-Type"), style.Body.String())
	}
	if missing := get("/preview/"+deliverable.ID+"/nope.png", nil); missing.Code != http.StatusNotFound {
		t.Fatalf("missing file = %d", missing.Code)
	}
	if escape := get("/preview/"+deliverable.ID+"/../../etc/passwd", nil); escape.Code == http.StatusOK && strings.Contains(escape.Body.String(), "root:") {
		t.Fatal("a path outside the bundle was served")
	}
	download := get("/api/deliverables/"+deliverable.ID+"/download", nil)
	if download.Code != http.StatusOK || download.Header().Get("Content-Type") != "application/zip" || !bytes.Equal(download.Body.Bytes(), archive.Bytes()) {
		t.Fatalf("download = %d %s (%d bytes)", download.Code, download.Header().Get("Content-Type"), download.Body.Len())
	}
	// A runner fetches the bundle in chunks with the worker token.
	chunk := get("/api/blobs/"+info.Ref+"?offset=0", map[string]string{"Authorization": "Bearer runner-secret"})
	if chunk.Code != http.StatusOK || !bytes.Equal(chunk.Body.Bytes(), archive.Bytes()) || chunk.Header().Get("X-Spin-Size") == "" {
		t.Fatalf("chunk = %d (%d bytes)", chunk.Code, chunk.Body.Len())
	}
	if end := get("/api/blobs/"+info.Ref+"?offset=1048576", map[string]string{"Authorization": "Bearer runner-secret"}); end.Code != http.StatusRequestedRangeNotSatisfiable {
		t.Fatalf("chunk past the end = %d", end.Code)
	}
	if anonymous := get("/api/blobs/"+info.Ref+"?offset=0", nil); anonymous.Code != http.StatusUnauthorized {
		t.Fatalf("chunk without the worker token = %d", anonymous.Code)
	}
	// A comment on a visual revision needs no selection.
	comment, err := st.AddDeliverableComment(deliverable.ID, "john", domain.CreateDeliverableCommentRequest{Body: "De kop mag groter."})
	if err != nil || comment.SelectedText != "" {
		t.Fatalf("comment on a visual revision = %+v, %v", comment, err)
	}
	// The next step starts with the deliverable placed in its capsule.
	composition := domain.Composition{ID: "cmp", Runtime: &domain.CapsuleRuntime{Driver: "test", ContainerID: "c", Status: "ready"}}
	srv.placeDeliverables(context.Background(), created.Job.ID, composition)
	if len(engine.placed) != 1 || engine.placed[0] != "/root/deliverables/website" {
		t.Fatalf("placed = %v", engine.placed)
	}
}
