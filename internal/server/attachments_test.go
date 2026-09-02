package server

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"io"
	"log/slog"
	"mime/multipart"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

func TestJobAttachmentsAreStagedInjectedAndServedOutsideGit(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &testEngine{}
	attachmentDir := t.TempDir()
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true, AttachmentDir: attachmentDir})
	for _, line := range []string{
		"RECORD tool:git --scope=global --enable=git", "install git", "END RECORD",
		"RECORD tool:agent --scope=global --from=tool:git --enable=acp --command=agent-acp", "install agent", "END RECORD",
	} {
		if _, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: line}); err != nil {
			t.Fatal(err)
		}
	}
	repository, err := st.CreateGitRepository(domain.CreateGitRepositoryRequest{Operator: "derek", Name: "attachments", RemoteURL: "https://github.com/derek/attachments.git"})
	if err != nil {
		t.Fatal(err)
	}
	template, err := st.CreateWorkflowTemplate(domain.CreateWorkflowTemplateRequest{Operator: "derek", Name: "Inspect image", Phases: []domain.WorkflowPhase{{
		ID: "plan", Name: "Plan", Instructions: "Inspecteer de bijlage", Accept: domain.WorkflowTransition{Target: domain.WorkflowTargetDone}, Reject: domain.WorkflowTransition{Target: domain.WorkflowTargetSelf},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	attachment := uploadAttachmentForTest(t, srv.Handler(), "/api/job-attachments?operator=derek", "voorbeeld.png", append([]byte("\x89PNG\r\n\x1a\n"), bytes.Repeat([]byte{1}, 80)...), http.StatusCreated)
	created, err := st.CreateJob(domain.CreateJobRequest{
		Title: "Lees screenshot", Objective: "Gebruik het ontwerp", Operator: "derek", GitRepositoryID: repository.Repository.ID,
		EnvironmentSelector: "tool:agent", TemplateID: template.ID, AttachmentIDs: []string{attachment.ID},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(created.Job.AttachmentIDs) != 1 || st.Snapshot().JobAttachments[0].JobID != created.Job.ID {
		t.Fatalf("Job attachment was not bound: job=%+v snapshot=%+v", created.Job, st.Snapshot().JobAttachments)
	}
	if _, err := srv.useCapsule(t.Context(), domain.UseRequest{Selector: "session:" + created.Session.ID, Operator: "derek"}); err != nil {
		t.Fatal(err)
	}
	if len(engine.injected) != 1 || engine.injected[0].TargetPath != attachment.CapsulePath || strings.Contains(engine.injected[0].TargetPath, "/workspace") {
		t.Fatalf("injected attachments = %+v", engine.injected)
	}
	pdf := uploadAttachmentForTest(t, srv.Handler(), "/api/jobs/"+created.Job.ID+"/attachments?operator=derek", "specificatie.pdf", []byte("%PDF-1.7\n1 0 obj\n<<>>\nendobj\n"), http.StatusCreated)
	if len(engine.injected) != 2 || engine.injected[1].TargetPath != pdf.CapsulePath || len(st.JobAttachments(created.Job.ID)) != 2 {
		t.Fatalf("late Job attachment was not injected: injected=%+v attachments=%+v", engine.injected, st.JobAttachments(created.Job.ID))
	}
	prompt, err := srv.workflowPrompt(created.Session.ID)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(prompt, "JOB-BIJLAGEN") || !strings.Contains(prompt, attachment.CapsulePath) || !strings.Contains(prompt, attachment.Name) || !strings.Contains(prompt, pdf.CapsulePath) {
		t.Fatalf("workflow prompt misses attachment:\n%s", prompt)
	}
	blocks, err := srv.acpPromptAttachments(created.Session.ID, acpPromptCapabilities{Image: true, EmbeddedContext: true}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(blocks) != 2 || blocks[0].Block["type"] != "image" || blocks[1].Block["type"] != "resource" {
		t.Fatalf("rich ACP attachment blocks = %#v", blocks)
	}
	imageData, err := base64.StdEncoding.DecodeString(blocks[0].Block["data"].(string))
	if err != nil || !bytes.HasPrefix(imageData, []byte("\x89PNG")) {
		t.Fatalf("ACP image data is not the uploaded image: error=%v data=%q", err, imageData)
	}
	resource := blocks[1].Block["resource"].(map[string]any)
	pdfData, err := base64.StdEncoding.DecodeString(resource["blob"].(string))
	if err != nil || !bytes.HasPrefix(pdfData, []byte("%PDF")) || resource["mimeType"] != "application/pdf" {
		t.Fatalf("ACP PDF resource = %#v; error=%v", resource, err)
	}
	fallback, err := srv.acpPromptAttachments(created.Session.ID, acpPromptCapabilities{}, nil)
	if err != nil || len(fallback) != 2 || fallback[0].Block["type"] != "resource_link" || fallback[1].Block["type"] != "resource_link" {
		t.Fatalf("baseline ACP attachment blocks = %#v; error=%v", fallback, err)
	}
	if !strings.HasPrefix(fallback[0].Block["uri"].(string), "file:///spin/job-attachments/") {
		t.Fatalf("ACP resource URI = %q", fallback[0].Block["uri"])
	}
	download := httptest.NewRequest(http.MethodGet, "/api/job-attachments/"+attachment.ID+"?operator=john", nil)
	downloadResponse := httptest.NewRecorder()
	srv.Handler().ServeHTTP(downloadResponse, download)
	if downloadResponse.Code != http.StatusOK || downloadResponse.Header().Get("Content-Type") != "image/png" || !bytes.HasPrefix(downloadResponse.Body.Bytes(), []byte("\x89PNG")) {
		t.Fatalf("download status=%d type=%q body=%q", downloadResponse.Code, downloadResponse.Header().Get("Content-Type"), downloadResponse.Body.Bytes())
	}
}

func TestJobAttachmentRejectsUnsupportedContent(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), &testEngine{}, ServerOptions{DisableAuthentication: true, AttachmentDir: t.TempDir()})
	uploadAttachmentForTest(t, srv.Handler(), "/api/job-attachments?operator=derek", "notes.txt", []byte("plain text is not an image"), http.StatusBadRequest)
	if len(st.Snapshot().JobAttachments) != 0 {
		t.Fatalf("unsupported attachment persisted: %+v", st.Snapshot().JobAttachments)
	}
}

func uploadAttachmentForTest(t *testing.T, handler http.Handler, path, name string, content []byte, expectedStatus int) domain.JobAttachment {
	t.Helper()
	var body bytes.Buffer
	writer := multipart.NewWriter(&body)
	part, err := writer.CreateFormFile("file", name)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := part.Write(content); err != nil {
		t.Fatal(err)
	}
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	request := httptest.NewRequest(http.MethodPost, path, &body)
	request.Header.Set("Content-Type", writer.FormDataContentType())
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != expectedStatus {
		t.Fatalf("upload status=%d body=%s", response.Code, response.Body.String())
	}
	if expectedStatus != http.StatusCreated {
		return domain.JobAttachment{}
	}
	var attachment domain.JobAttachment
	if err := json.Unmarshal(response.Body.Bytes(), &attachment); err != nil {
		t.Fatal(err)
	}
	return attachment
}
