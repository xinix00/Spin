package server

import (
	"archive/zip"
	"context"
	"errors"
	"fmt"
	"html"
	"io"
	"io/fs"
	"mime"
	"net/http"
	"net/url"
	"path"
	"strconv"
	"strings"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

// A Job's deliverables live in every capsule of the Job as files under
// /root/deliverables: a Markdown document as a file, a visual deliverable
// (a page with its own CSS and JS, an image, a PDF) as a folder or a
// file. When a step starts they are put there; the agent edits on disk and
// puts one back as a new revision. A visual revision is a zip in the
// database, served to the reviewer under /preview in a sandbox.

// placeDeliverables puts the latest revision of every deliverable of the
// Job into the capsule.
func (s *Server) placeDeliverables(ctx context.Context, jobID string, composition domain.Composition) {
	if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return
	}
	latest := map[string]domain.Deliverable{}
	for _, deliverable := range s.store.Snapshot().Deliverables {
		if deliverable.JobID != jobID {
			continue
		}
		key := strings.ToLower(deliverable.Name)
		if current, ok := latest[key]; !ok || deliverable.Revision > current.Revision {
			latest[key] = deliverable
		}
	}
	if len(latest) == 0 {
		return
	}
	documents := map[string][]byte{}
	for _, deliverable := range latest {
		target := deliverable.CapsulePath()
		if domain.DeliverableIsBundle(deliverable.Kind) && deliverable.Bundle != nil {
			placer, ok := s.engine.(capsule.DeliverablePlacer)
			if !ok {
				continue
			}
			if err := placer.PlaceDeliverable(ctx, *composition.Runtime, target, *deliverable.Bundle); err != nil {
				s.logger.Warn("place deliverable", "composition", composition.ID, "deliverable", deliverable.Name, "error", err)
			}
			continue
		}
		documents[target] = []byte(deliverable.Content + "\n")
	}
	if len(documents) == 0 {
		return
	}
	tracked, ok := s.engine.(capsule.TrackedFiles)
	if !ok {
		return
	}
	if err := tracked.WriteTrackedFiles(ctx, *composition.Runtime, documents); err != nil {
		s.logger.Warn("place deliverable documents", "composition", composition.ID, "error", err)
	}
}

// putWorkflowDeliverable takes what the agent points at in its capsule and
// stores it as the revision of the running step.
func (s *Server) putWorkflowDeliverable(ctx context.Context, sessionID, name, putPath string) (domain.Deliverable, error) {
	_, _, _, phase, _, _, err := s.store.WorkflowForSession(sessionID)
	if err != nil {
		return domain.Deliverable{}, err
	}
	var definition *domain.DeliverableDefinition
	for index := range phase.Deliverables {
		if strings.EqualFold(phase.Deliverables[index].Name, name) {
			definition = &phase.Deliverables[index]
		}
	}
	if definition == nil {
		return domain.Deliverable{}, fmt.Errorf("deliverable %q is not declared by phase %s: %w", name, phase.Name, store.ErrConflict)
	}
	cleaned := path.Clean(strings.TrimSpace(putPath))
	if !strings.HasPrefix(cleaned, domain.DeliverableDirectory+"/") {
		return domain.Deliverable{}, fmt.Errorf("the path of a deliverable lies inside %s; move it there and put it again: %w", domain.DeliverableDirectory, store.ErrConflict)
	}
	operator := ""
	for _, candidate := range s.store.Snapshot().Sessions {
		if candidate.ID == sessionID {
			operator = candidate.Operator
		}
	}
	session, composition, err := s.sessionComposition(sessionID, operator)
	if err != nil {
		return domain.Deliverable{}, err
	}
	if composition.Runtime == nil || composition.Runtime.Status == "stopped" {
		return domain.Deliverable{}, fmt.Errorf("session %s has no running capsule: %w", session.ID, store.ErrConflict)
	}
	if domain.DeliverableIsBundle(definition.Kind) {
		bundler, ok := s.engine.(capsule.DeliverableBundler)
		if !ok {
			return domain.Deliverable{}, fmt.Errorf("the engine cannot bundle a deliverable: %w", store.ErrConflict)
		}
		bundle, err := bundler.BundleDeliverable(ctx, *composition.Runtime, cleaned)
		if err != nil {
			return domain.Deliverable{}, err
		}
		return s.store.PutWorkflowDeliverable(sessionID, definition.Name, "", &bundle)
	}
	if strings.ToLower(path.Ext(cleaned)) != ".md" {
		return domain.Deliverable{}, fmt.Errorf("deliverable %s is a Markdown document: put a .md file: %w", definition.Name, store.ErrConflict)
	}
	tracked, ok := s.engine.(capsule.TrackedFiles)
	if !ok {
		return domain.Deliverable{}, fmt.Errorf("the engine cannot read a file: %w", store.ErrConflict)
	}
	files, err := tracked.ReadTrackedFiles(ctx, *composition.Runtime, []string{cleaned})
	if err != nil {
		return domain.Deliverable{}, err
	}
	content, ok := files[cleaned]
	if !ok {
		return domain.Deliverable{}, fmt.Errorf("%s does not exist in the capsule: %w", cleaned, store.ErrConflict)
	}
	return s.store.PutWorkflowDeliverable(sessionID, definition.Name, string(content), nil)
}

// deliverableShape says what a revision is, for the prompt.
func deliverableShape(deliverable domain.Deliverable) string {
	if !domain.DeliverableIsBundle(deliverable.Kind) || deliverable.Bundle == nil {
		return "Markdown"
	}
	if deliverable.Bundle.Folder {
		if deliverable.Bundle.Entry != "" {
			return fmt.Sprintf("map met index.html, %d bestanden", deliverable.Bundle.Files)
		}
		return fmt.Sprintf("map, %d bestanden", deliverable.Bundle.Files)
	}
	return deliverable.Bundle.ContentType
}

// deliverableAsk says what a definition asks for, for the prompt.
func deliverableAsk(definition domain.DeliverableDefinition) string {
	slug := domain.DeliverableSlug(definition.Name)
	switch definition.Kind {
	case domain.DeliverableKindPDF:
		return fmt.Sprintf("Eén PDF-bestand; bijvoorbeeld %s/%s.pdf", domain.DeliverableDirectory, slug)
	case domain.DeliverableKindImage:
		return fmt.Sprintf("Eén afbeelding (png, jpg, gif, webp, svg); bijvoorbeeld %s/%s.png", domain.DeliverableDirectory, slug)
	case domain.DeliverableKindFolder:
		return fmt.Sprintf("Een map met minstens één bestand; met index.html erin (eigen CSS, JS en afbeeldingen mogen los) toont Spin de pagina; bijvoorbeeld %s/%s/", domain.DeliverableDirectory, slug)
	case domain.DeliverableKindFile:
		return fmt.Sprintf("Eén bestand, welke vorm ook; bijvoorbeeld %s/%s.<ext>", domain.DeliverableDirectory, slug)
	}
	return fmt.Sprintf("Markdown-bestand; bijvoorbeeld %s/%s.md", domain.DeliverableDirectory, slug)
}

// previewDeliverable serves one file out of a visual revision's bundle.
// Every response is sandboxed: the page runs with an opaque origin, may
// not reach the network or post anywhere, and cannot touch Spin's state,
// whether framed in the viewer or opened on its own.
func (s *Server) previewDeliverable(w http.ResponseWriter, r *http.Request) {
	if !s.authDisabled {
		if _, err := s.requestIdentity(r); err != nil {
			http.Error(w, "authentication required", http.StatusUnauthorized)
			return
		}
	}
	deliverable, err := s.store.Deliverable(r.PathValue("deliverableID"))
	if err != nil || !domain.DeliverableIsBundle(deliverable.Kind) || deliverable.Bundle == nil || s.database == nil {
		http.NotFound(w, r)
		return
	}
	name := path.Clean("/" + r.PathValue("file"))
	name = strings.TrimPrefix(name, "/")
	readerAt, info, err := s.database.BlobReaderAt(r.Context(), deliverable.Bundle.Ref)
	if err != nil {
		writeError(w, err)
		return
	}
	archive, err := zip.NewReader(readerAt, info.Size)
	if err != nil {
		writeError(w, err)
		return
	}
	if name == "" || name == "." {
		if deliverable.Bundle.Entry == "" {
			// A folder without index.html: the files, each a link.
			s.previewListing(w, deliverable, archive)
			return
		}
		name = deliverable.Bundle.Entry
	}
	var file *zip.File
	for _, candidate := range archive.File {
		if candidate.Name == name {
			file = candidate
			break
		}
	}
	if file == nil {
		http.NotFound(w, r)
		return
	}
	content, err := file.Open()
	if err != nil {
		writeError(w, err)
		return
	}
	defer content.Close()
	contentType := mime.TypeByExtension(path.Ext(name))
	if contentType == "" {
		contentType = "application/octet-stream"
	}
	w.Header().Set("Content-Type", contentType)
	w.Header().Set("Content-Length", strconv.FormatUint(file.UncompressedSize64, 10))
	w.Header().Set("Content-Security-Policy", previewPolicy(contentType))
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Header().Set("Referrer-Policy", "no-referrer")
	w.Header().Set("Cross-Origin-Resource-Policy", "same-origin")
	w.Header().Set("Cache-Control", "private, max-age=300")
	w.WriteHeader(http.StatusOK)
	_, _ = io.Copy(w, content)
}

// downloadBundle sends a visual revision as the zip it is kept as.
func (s *Server) downloadBundle(w http.ResponseWriter, r *http.Request, deliverable domain.Deliverable) {
	if s.database == nil {
		writeError(w, fmt.Errorf("SQLite storage is not configured: %w", store.ErrConflict))
		return
	}
	readerAt, info, err := s.database.BlobReaderAt(r.Context(), deliverable.Bundle.Ref)
	if err != nil {
		writeError(w, err)
		return
	}
	filename := fmt.Sprintf("%s-r%d.zip", safeFilename(deliverable.Name), deliverable.Revision)
	w.Header().Set("Content-Type", "application/zip")
	w.Header().Set("Content-Disposition", mime.FormatMediaType("attachment", map[string]string{"filename": filename}))
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Header().Set("Cache-Control", "private, no-store")
	http.ServeContent(w, r, filename, deliverable.CreatedAt, io.NewSectionReader(readerAt, 0, info.Size))
}

// blobChunkHandler hands a runner one chunk of a bundle, the way snapshot
// chunks travel.
func (s *Server) blobChunkHandler(w http.ResponseWriter, r *http.Request) {
	if !s.workerRequest(r) {
		writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "runner authorization required"})
		return
	}
	ref := strings.TrimSpace(r.PathValue("ref"))
	offset, err := strconv.ParseInt(strings.TrimSpace(r.URL.Query().Get("offset")), 10, 64)
	if !strings.HasPrefix(ref, "bundle:") || err != nil || offset < 0 || s.database == nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "a bundle ref and a non-negative offset are required"})
		return
	}
	chunk, info, err := s.database.ReadBlobChunk(r.Context(), ref, offset)
	if errors.Is(err, fs.ErrNotExist) {
		writeJSON(w, http.StatusNotFound, map[string]string{"error": "bundle is not in the archive"})
		return
	}
	if err != nil {
		writeError(w, err)
		return
	}
	w.Header().Set("X-Spin-Size", strconv.FormatInt(info.Size, 10))
	w.Header().Set("X-Spin-Digest", info.Digest)
	if len(chunk) == 0 {
		w.WriteHeader(http.StatusRequestedRangeNotSatisfiable)
		return
	}
	w.Header().Set("Content-Type", "application/octet-stream")
	w.Header().Set("Content-Length", strconv.Itoa(len(chunk)))
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(chunk)
}

// jobBundleRefs lists the bundles a Job's deliverables use.
func (s *Server) jobBundleRefs(jobID string) []string {
	var refs []string
	for _, deliverable := range s.store.Snapshot().Deliverables {
		if deliverable.JobID == jobID && deliverable.Bundle != nil {
			refs = append(refs, deliverable.Bundle.Ref)
		}
	}
	return refs
}

// removeUnusedBundles drops bundles no remaining deliverable refers to.
func (s *Server) removeUnusedBundles(refs []string) {
	if s.database == nil || len(refs) == 0 {
		return
	}
	inUse := map[string]bool{}
	for _, deliverable := range s.store.Snapshot().Deliverables {
		if deliverable.Bundle != nil {
			inUse[deliverable.Bundle.Ref] = true
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()
	for _, ref := range refs {
		if inUse[ref] {
			continue
		}
		if err := s.database.DeleteBlob(ctx, ref); err != nil && !errors.Is(err, fs.ErrNotExist) {
			s.logger.Warn("remove deliverable bundle", "ref", ref, "error", err)
		}
	}
}

// previewPolicy sandboxes what can run. A PDF or a raster image runs
// nothing of its own and is shown by the browser's own viewer, which a
// sandbox would refuse; everything else (pages, scripts, styles, SVG) gets
// an opaque origin, no network and nowhere to post.
func previewPolicy(contentType string) string {
	lower := strings.ToLower(contentType)
	if strings.HasPrefix(lower, "application/pdf") || (strings.HasPrefix(lower, "image/") && !strings.Contains(lower, "svg")) {
		return "default-src 'none'; frame-ancestors 'self'"
	}
	return "sandbox allow-scripts allow-forms allow-modals; default-src 'self' data: blob: 'unsafe-inline' 'unsafe-eval'; connect-src 'none'; form-action 'none'; frame-ancestors 'self'"
}

// previewListing is the page for a folder without index.html: its files.
func (s *Server) previewListing(w http.ResponseWriter, deliverable domain.Deliverable, archive *zip.Reader) {
	var page strings.Builder
	page.WriteString("<!doctype html><meta charset=\"utf-8\"><title>" + html.EscapeString(deliverable.Name) + "</title><style>body{font:14px/1.6 system-ui,sans-serif;margin:24px;color:#111}a{display:block;padding:4px 0;color:#1a56b3}small{color:#666}</style><h1>" + html.EscapeString(deliverable.Name) + " <small>revisie " + strconv.Itoa(deliverable.Revision) + "</small></h1>")
	for _, file := range archive.File {
		if file.FileInfo().IsDir() {
			continue
		}
		href := (&url.URL{Path: file.Name}).EscapedPath()
		page.WriteString("<a href=\"" + href + "\">" + html.EscapeString(file.Name) + " <small>" + html.EscapeString(formatBytesGo(int64(file.UncompressedSize64))) + "</small></a>")
	}
	body := page.String()
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Content-Length", strconv.Itoa(len(body)))
	w.Header().Set("Content-Security-Policy", previewPolicy("text/html"))
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Header().Set("Cache-Control", "private, max-age=300")
	w.WriteHeader(http.StatusOK)
	_, _ = io.WriteString(w, body)
}

func formatBytesGo(size int64) string {
	switch {
	case size >= 1<<20:
		return fmt.Sprintf("%.1f MiB", float64(size)/(1<<20))
	case size >= 1<<10:
		return fmt.Sprintf("%.0f KiB", float64(size)/(1<<10))
	}
	return fmt.Sprintf("%d B", size)
}
