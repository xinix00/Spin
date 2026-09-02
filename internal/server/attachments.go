package server

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"mime"
	"net/http"
	"net/url"
	"path/filepath"
	"strings"
	"time"
	"unicode"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

const (
	maxJobAttachmentBytes = 15 << 20
	attachmentCapsuleDir  = "/spin/job-attachments/"
)

var allowedJobAttachmentTypes = map[string]bool{
	"application/pdf": true,
	"image/gif":       true,
	"image/jpeg":      true,
	"image/png":       true,
	"image/webp":      true,
}

func (s *Server) uploadStagedJobAttachment(w http.ResponseWriter, r *http.Request) {
	s.uploadJobAttachmentFor(w, r, "")
}

func (s *Server) uploadJobAttachment(w http.ResponseWriter, r *http.Request) {
	s.uploadJobAttachmentFor(w, r, strings.TrimSpace(r.PathValue("jobID")))
}

func (s *Server) uploadJobAttachmentFor(w http.ResponseWriter, r *http.Request, jobID string) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	if s.attachments == nil {
		writeError(w, fmt.Errorf("Job attachment storage is not configured: %w", store.ErrConflict))
		return
	}
	if jobID != "" {
		job, _, err := s.store.PrepareJobDeletion(jobID, operator)
		if err != nil {
			writeError(w, err)
			return
		}
		if job.Status == domain.JobDone || job.Status == domain.JobCancelled {
			writeError(w, fmt.Errorf("closed Job attachments are immutable: %w", store.ErrConflict))
			return
		}
	}
	r.Body = http.MaxBytesReader(w, r.Body, maxJobAttachmentBytes+(1<<20))
	if err := r.ParseMultipartForm(1 << 20); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "invalid attachment upload: " + err.Error()})
		return
	}
	if r.MultipartForm != nil {
		defer r.MultipartForm.RemoveAll()
	}
	upload, header, err := r.FormFile("file")
	if err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "multipart field file is required"})
		return
	}
	defer upload.Close()
	name := cleanAttachmentName(header.Filename)
	if name == "" {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "attachment filename is invalid"})
		return
	}
	first := make([]byte, 512)
	firstCount, readErr := io.ReadFull(upload, first)
	if readErr != nil && !errors.Is(readErr, io.EOF) && !errors.Is(readErr, io.ErrUnexpectedEOF) {
		writeError(w, readErr)
		return
	}
	first = first[:firstCount]
	mediaType := http.DetectContentType(first)
	if !allowedJobAttachmentTypes[mediaType] {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "only PDF, PNG, JPEG, WebP and GIF attachments are supported"})
		return
	}
	idValue, err := randomOAuthValue(18)
	if err != nil {
		writeError(w, err)
		return
	}
	attachmentID := "att_" + idValue
	data, err := io.ReadAll(io.LimitReader(io.MultiReader(bytes.NewReader(first), upload), maxJobAttachmentBytes+1))
	if err != nil {
		writeError(w, fmt.Errorf("read attachment: %w", err))
		return
	}
	if len(data) > maxJobAttachmentBytes {
		writeJSON(w, http.StatusRequestEntityTooLarge, map[string]string{"error": "an attachment may be at most 15 MiB"})
		return
	}
	hash := sha256.Sum256(data)
	if err := s.attachments.WriteFile(attachmentID, data); err != nil {
		writeError(w, fmt.Errorf("store attachment: %w", err))
		return
	}
	attachment, err := s.store.CreateJobAttachment(domain.CreateJobAttachmentRequest{
		ID: attachmentID, JobID: jobID, Name: name, MediaType: mediaType, Size: int64(len(data)),
		SHA256: hex.EncodeToString(hash[:]), CapsulePath: attachmentCapsuleDir + attachmentTargetName(attachmentID, name, mediaType), Operator: operator,
	})
	if err != nil {
		_ = s.attachments.Remove(attachmentID)
		writeError(w, err)
		return
	}
	if jobID != "" {
		s.injectAttachmentIntoLiveJob(jobID, operator, attachment)
	}
	writeJSON(w, http.StatusCreated, attachment)
}

func (s *Server) downloadJobAttachment(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	attachment, err := s.store.JobAttachment(r.PathValue("attachmentID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	if s.attachments == nil {
		writeError(w, store.ErrNotFound)
		return
	}
	data, err := s.attachments.ReadFile(attachment.ID)
	if err != nil {
		writeError(w, store.ErrNotFound)
		return
	}
	if err := verifyAttachmentData(attachment, data); err != nil {
		writeError(w, err)
		return
	}
	disposition := mime.FormatMediaType("inline", map[string]string{"filename": attachment.Name})
	w.Header().Set("Content-Type", attachment.MediaType)
	w.Header().Set("Content-Disposition", disposition)
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Header().Set("Content-Security-Policy", "sandbox; default-src 'none'; img-src 'self' data:")
	w.Header().Set("Cache-Control", "private, no-store")
	http.ServeContent(w, r, attachment.Name, attachment.CreatedAt, bytes.NewReader(data))
}

func (s *Server) deleteStagedJobAttachment(w http.ResponseWriter, r *http.Request) {
	operator := s.requestOperator(r, r.URL.Query().Get("operator"))
	attachment, err := s.store.DeleteStagedJobAttachment(r.PathValue("attachmentID"), operator)
	if err != nil {
		writeError(w, err)
		return
	}
	if s.attachments != nil {
		_ = s.attachments.Remove(attachment.ID)
	}
	writeJSON(w, http.StatusOK, attachment)
}

// startACPPrompt enriches the textual turn with any Job attachments that this
// concrete ACP session has not received yet.
func (s *Server) startACPPrompt(active *activeACP, text string) error {
	attachments, err := s.acpPromptAttachments(active.sessionID, active.promptCapabilities(), active.sentAttachmentSnapshot())
	if err != nil {
		return fmt.Errorf("prepare ACP attachments: %w", err)
	}
	return active.startPromptWithAttachments(text, attachments)
}

func (s *Server) acpPromptAttachments(sessionID string, capabilities acpPromptCapabilities, alreadySent map[string]bool) ([]acpPromptAttachment, error) {
	snapshot := s.store.Snapshot()
	job := domain.Job{}
	for _, session := range snapshot.Sessions {
		if session.ID == strings.TrimSpace(sessionID) {
			for _, candidate := range snapshot.Jobs {
				if candidate.ID == session.JobID {
					job = candidate
					break
				}
			}
			break
		}
	}
	if job.ID == "" {
		return nil, nil
	}
	attachments := s.jobContextAttachments(job)
	blocks := make([]acpPromptAttachment, 0, len(attachments))
	for _, attachment := range attachments {
		if alreadySent[attachment.ID] {
			continue
		}
		uri := (&url.URL{Scheme: "file", Path: attachment.CapsulePath}).String()
		block := map[string]any{
			"type": "resource_link", "uri": uri, "name": attachment.Name,
			"mimeType": attachment.MediaType, "size": attachment.Size,
			"description": "Immutable Job attachment supplied by " + attachment.CreatedBy,
		}
		isImage := strings.HasPrefix(attachment.MediaType, "image/")
		if (isImage && capabilities.Image) || (!isImage && capabilities.EmbeddedContext) {
			if s.attachments == nil {
				return nil, fmt.Errorf("attachment storage is not configured")
			}
			data, err := s.attachments.ReadFile(attachment.ID)
			if err != nil {
				return nil, fmt.Errorf("read %s: %w", attachment.Name, err)
			}
			if err := verifyAttachmentData(attachment, data); err != nil {
				return nil, err
			}
			encoded := base64.StdEncoding.EncodeToString(data)
			if isImage {
				block = map[string]any{"type": "image", "data": encoded, "mimeType": attachment.MediaType, "uri": uri}
			} else {
				block = map[string]any{"type": "resource", "resource": map[string]any{"uri": uri, "mimeType": attachment.MediaType, "blob": encoded}}
			}
		}
		blocks = append(blocks, acpPromptAttachment{ID: attachment.ID, Block: block})
	}
	if !capabilities.EmbeddedContext {
		return blocks, nil
	}
	_, _, _, phase, deliverables, _, err := s.store.WorkflowForSession(sessionID)
	if err != nil {
		if errors.Is(err, store.ErrNotFound) {
			return blocks, nil
		}
		return nil, err
	}
	latest := map[string]domain.Deliverable{}
	for _, deliverable := range deliverables {
		key := strings.ToLower(strings.TrimSpace(deliverable.Name))
		if current, ok := latest[key]; !ok || deliverable.Revision > current.Revision {
			latest[key] = deliverable
		}
	}
	for _, name := range phase.Inject {
		deliverable, ok := latest[strings.ToLower(strings.TrimSpace(name))]
		if !ok {
			return nil, fmt.Errorf("injected deliverable %s is not available for phase %s", name, phase.Name)
		}
		attachmentID := "deliverable:" + deliverable.ID
		if alreadySent[attachmentID] {
			continue
		}
		uri := (&url.URL{Scheme: "spin", Host: "deliverables", Path: "/" + deliverable.ID + "/document.md"}).String()
		blocks = append(blocks, acpPromptAttachment{ID: attachmentID, Block: map[string]any{
			"type": "resource",
			"resource": map[string]any{
				"uri": uri, "mimeType": "text/markdown", "text": deliverable.Content,
			},
			"_meta": map[string]any{"name": deliverable.Name, "revision": deliverable.Revision},
		}})
	}
	if job.ForkedFromJobID != "" {
		sourceLatest := map[string]domain.Deliverable{}
		for _, deliverable := range snapshot.Deliverables {
			if deliverable.JobID != job.ForkedFromJobID {
				continue
			}
			key := strings.ToLower(strings.TrimSpace(deliverable.Name))
			if current, ok := sourceLatest[key]; !ok || deliverable.Revision > current.Revision {
				sourceLatest[key] = deliverable
			}
		}
		for _, deliverable := range sourceLatest {
			attachmentID := "fork-deliverable:" + deliverable.ID
			if alreadySent[attachmentID] {
				continue
			}
			uri := (&url.URL{Scheme: "spin", Host: "jobs", Path: "/" + job.ForkedFromJobID + "/deliverables/" + deliverable.ID + "/document.md"}).String()
			blocks = append(blocks, acpPromptAttachment{ID: attachmentID, Block: map[string]any{
				"type": "resource",
				"resource": map[string]any{
					"uri": uri, "mimeType": "text/markdown", "text": deliverable.Content,
				},
				"_meta": map[string]any{"name": deliverable.Name, "revision": deliverable.Revision, "sourceJobId": job.ForkedFromJobID},
			}})
		}
	}
	return blocks, nil
}

func (s *Server) jobContextAttachments(job domain.Job) []domain.JobAttachment {
	attachments := s.store.JobAttachments(job.ID)
	if job.ForkedFromJobID != "" {
		attachments = append(attachments, s.store.JobAttachments(job.ForkedFromJobID)...)
	}
	return attachments
}

func (s *Server) injectAttachmentIntoLiveJob(jobID, operator string, attachment domain.JobAttachment) {
	_, compositions, err := s.store.PrepareJobDeletion(jobID, operator)
	if err != nil {
		return
	}
	injector, ok := s.engine.(capsule.WorkspaceAttachmentInjector)
	if !ok {
		return
	}
	for _, composition := range compositions {
		if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
			continue
		}
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		file, fileErr := s.capsuleAttachment(attachment)
		if fileErr != nil {
			s.logger.Warn("read new Job attachment", "job", jobID, "attachment", attachment.ID, "error", fileErr)
			cancel()
			continue
		}
		err := injector.InjectWorkspaceAttachments(ctx, *composition.Runtime, []capsule.WorkspaceAttachment{file})
		cancel()
		if err != nil {
			s.logger.Warn("inject new Job attachment", "job", jobID, "attachment", attachment.ID, "composition", composition.ID, "error", err)
		}
	}
}

func (s *Server) injectJobAttachments(ctx context.Context, composition domain.Composition, runtime domain.CapsuleRuntime) error {
	if composition.SessionID == "" {
		return nil
	}
	snapshot := s.store.Snapshot()
	job := domain.Job{}
	for _, session := range snapshot.Sessions {
		if session.ID == composition.SessionID {
			for _, candidate := range snapshot.Jobs {
				if candidate.ID == session.JobID {
					job = candidate
					break
				}
			}
			break
		}
	}
	if job.ID == "" {
		return nil
	}
	attachments := s.jobContextAttachments(job)
	if len(attachments) == 0 {
		return nil
	}
	injector, ok := s.engine.(capsule.WorkspaceAttachmentInjector)
	if !ok {
		return fmt.Errorf("capsule engine cannot inject Job attachments: %w", store.ErrConflict)
	}
	files := make([]capsule.WorkspaceAttachment, 0, len(attachments))
	for _, attachment := range attachments {
		file, err := s.capsuleAttachment(attachment)
		if err != nil {
			return err
		}
		files = append(files, file)
	}
	return injector.InjectWorkspaceAttachments(ctx, runtime, files)
}

func (s *Server) capsuleAttachment(attachment domain.JobAttachment) (capsule.WorkspaceAttachment, error) {
	if s.attachments == nil {
		return capsule.WorkspaceAttachment{}, fmt.Errorf("Job attachment storage is not configured: %w", store.ErrConflict)
	}
	if localPath := s.attachments.LocalPath(attachment.ID); localPath != "" {
		return capsule.WorkspaceAttachment{SourcePath: localPath, TargetPath: attachment.CapsulePath}, nil
	}
	data, err := s.attachments.ReadFile(attachment.ID)
	if err != nil {
		return capsule.WorkspaceAttachment{}, fmt.Errorf("read attachment %s: %w", attachment.Name, err)
	}
	if err := verifyAttachmentData(attachment, data); err != nil {
		return capsule.WorkspaceAttachment{}, err
	}
	return capsule.WorkspaceAttachment{Data: data, TargetPath: attachment.CapsulePath}, nil
}

func verifyAttachmentData(attachment domain.JobAttachment, data []byte) error {
	if int64(len(data)) != attachment.Size {
		return fmt.Errorf("attachment %s changed after upload", attachment.Name)
	}
	hash := sha256.Sum256(data)
	if !strings.EqualFold(hex.EncodeToString(hash[:]), attachment.SHA256) {
		return fmt.Errorf("attachment %s checksum changed after upload", attachment.Name)
	}
	return nil
}

func cleanAttachmentName(value string) string {
	value = filepath.Base(strings.ReplaceAll(strings.TrimSpace(value), "\\", "/"))
	value = strings.Map(func(character rune) rune {
		if unicode.IsControl(character) {
			return -1
		}
		return character
	}, value)
	if value == "." || value == "" {
		return ""
	}
	characters := []rune(value)
	if len(characters) > 180 {
		value = string(characters[:180])
	}
	return value
}

func attachmentTargetName(id, name, mediaType string) string {
	extension := map[string]string{"application/pdf": ".pdf", "image/gif": ".gif", "image/jpeg": ".jpg", "image/png": ".png", "image/webp": ".webp"}[mediaType]
	base := strings.TrimSuffix(name, filepath.Ext(name))
	var cleaned strings.Builder
	for _, character := range strings.ToLower(base) {
		if character >= 'a' && character <= 'z' || character >= '0' && character <= '9' || character == '-' || character == '_' {
			cleaned.WriteRune(character)
		} else if cleaned.Len() == 0 || !strings.HasSuffix(cleaned.String(), "-") {
			cleaned.WriteByte('-')
		}
		if cleaned.Len() >= 64 {
			break
		}
	}
	value := strings.Trim(cleaned.String(), "-")
	if value == "" {
		value = "attachment"
	}
	return id + "-" + value + strings.ToLower(extension)
}
