package worker

import (
	"archive/tar"
	"archive/zip"
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"mime"
	"net/http"
	"net/url"
	"os"
	"path"
	"strconv"
	"strings"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

// A visual deliverable travels as a zip: the runner streams the folder or
// file out of the capsule as a tar, writes a zip, and sends that to the
// server in chunks like a layer archive; when a step starts, the runner
// fetches the zip in chunks and unpacks it into the capsule again. The
// server never handles the files, only the blob.

const (
	maxBundleBytes = 25 << 20
	maxBundleFiles = 2000
)

// bundleDeliverable zips what the path holds and uploads it. A folder must
// hold index.html at its root; a single file is its own entry.
func (w *Worker) bundleDeliverable(ctx context.Context, bundler capsule.WorkspaceBundler, runtime domain.CapsuleRuntime, bundlePath string) (domain.DeliverableBundle, error) {
	client, err := newUploadClient(w.config.ServerURL, w.config.Token)
	if err != nil {
		return domain.DeliverableBundle{}, err
	}
	file, err := os.CreateTemp("", "spin-bundle-*.zip")
	if err != nil {
		return domain.DeliverableBundle{}, err
	}
	defer func() {
		_ = file.Close()
		_ = os.Remove(file.Name())
	}()
	reader, writer := io.Pipe()
	streamErr := make(chan error, 1)
	go func() {
		err := bundler.BundleWorkspace(ctx, runtime, bundlePath, writer)
		_ = writer.CloseWithError(err)
		streamErr <- err
	}()
	entry, files, total, zipErr := tarToZip(reader, file)
	_ = reader.CloseWithError(zipErr)
	if err := <-streamErr; err != nil {
		return domain.DeliverableBundle{}, err
	}
	if zipErr != nil {
		return domain.DeliverableBundle{}, zipErr
	}
	info, err := file.Stat()
	if err != nil {
		return domain.DeliverableBundle{}, err
	}
	logger := w.logger.With("bundle", bundlePath)
	logger.Info("bundle: zipped", "files", files, "bytes", total, "zip_bytes", info.Size(), "entry", entry)
	result, err := uploadBundle(ctx, client, path.Base(strings.TrimRight(bundlePath, "/")), file, info.Size(), logger)
	if err != nil {
		return domain.DeliverableBundle{}, err
	}
	return domain.DeliverableBundle{Ref: result.Ref, Digest: result.Digest, Size: result.Size, Files: files, Entry: entry, ContentType: bundleContentType(entry)}, nil
}

// tarToZip writes the regular files of a tar to a zip and names the entry:
// the one file of a single-file bundle, or index.html of a folder.
func tarToZip(source io.Reader, destination *os.File) (entry string, files int, total int64, err error) {
	archive := zip.NewWriter(destination)
	reader := tar.NewReader(source)
	var names []string
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return "", 0, 0, err
		}
		if header.Typeflag != tar.TypeReg {
			continue
		}
		name := path.Clean(strings.TrimPrefix(header.Name, "./"))
		if name == "." || name == "" || strings.HasPrefix(name, "../") || path.IsAbs(name) {
			continue
		}
		files++
		total += header.Size
		if files > maxBundleFiles {
			return "", 0, 0, fmt.Errorf("a bundle holds at most %d files", maxBundleFiles)
		}
		if total > maxBundleBytes {
			return "", 0, 0, fmt.Errorf("a bundle holds at most %d MiB", maxBundleBytes>>20)
		}
		part, err := archive.CreateHeader(&zip.FileHeader{Name: name, Method: zip.Deflate, Modified: header.ModTime})
		if err != nil {
			return "", 0, 0, err
		}
		if _, err := io.Copy(part, reader); err != nil {
			return "", 0, 0, err
		}
		names = append(names, name)
	}
	if err := archive.Close(); err != nil {
		return "", 0, 0, err
	}
	if files == 0 {
		return "", 0, 0, errors.New("the bundle is empty")
	}
	if files == 1 && !strings.Contains(names[0], "/") && names[0] != "index.html" {
		return names[0], files, total, nil
	}
	for _, name := range names {
		if name == "index.html" {
			return "index.html", files, total, nil
		}
	}
	return "", 0, 0, errors.New("a folder bundle needs index.html at its root")
}

func bundleContentType(entry string) string {
	switch strings.ToLower(path.Ext(entry)) {
	case ".md", ".markdown":
		return "text/markdown; charset=utf-8"
	case ".html", ".htm":
		return "text/html; charset=utf-8"
	}
	if kind := mime.TypeByExtension(path.Ext(entry)); kind != "" {
		return kind
	}
	return "application/octet-stream"
}

// uploadBundle drives one chunked upload of a bundle to completion.
func uploadBundle(ctx context.Context, client *uploadClient, name string, source io.ReaderAt, size int64, logger *slog.Logger) (archiveResult, error) {
	if size <= 0 {
		return archiveResult{}, errors.New("bundle is empty")
	}
	session, err := client.createBundle(ctx, name, size)
	if err != nil {
		return archiveResult{}, err
	}
	if err := uploadChunks(ctx, client, session, source, size, logger); err != nil {
		client.abort(session.ID)
		return archiveResult{}, err
	}
	result, err := client.complete(ctx, session.ID)
	if err != nil {
		client.abort(session.ID)
		return archiveResult{}, err
	}
	return result, nil
}

// placeDeliverable fetches a bundle from the server and unpacks it at
// target: a folder bundle into the folder, a single file at the path.
func (w *Worker) placeDeliverable(ctx context.Context, placer capsule.BundlePlacer, runtime domain.CapsuleRuntime, target string, bundle domain.DeliverableBundle) error {
	file, err := os.CreateTemp("", "spin-bundle-*.zip")
	if err != nil {
		return err
	}
	defer func() {
		_ = file.Close()
		_ = os.Remove(file.Name())
	}()
	if err := downloadBlob(ctx, w.config.ServerURL, w.config.Token, bundle.Ref, file); err != nil {
		return err
	}
	info, err := file.Stat()
	if err != nil {
		return err
	}
	archive, err := zip.NewReader(file, info.Size())
	if err != nil {
		return fmt.Errorf("bundle %s: %w", bundle.Ref, err)
	}
	single := len(archive.File) == 1 && bundle.Entry != "index.html"
	directory := target
	rename := ""
	if single {
		directory, rename = path.Dir(target), path.Base(target)
	}
	reader, writer := io.Pipe()
	go func() {
		_ = writer.CloseWithError(zipToTar(archive, writer, rename))
	}()
	err = placer.PlaceBundle(ctx, runtime, directory, reader)
	_ = reader.Close()
	return err
}

// zipToTar streams a zip as a tar; a rename names the single file.
func zipToTar(archive *zip.Reader, destination io.Writer, rename string) error {
	writer := tar.NewWriter(destination)
	for _, file := range archive.File {
		if file.FileInfo().IsDir() {
			continue
		}
		name := file.Name
		if rename != "" {
			name = rename
		}
		if err := writer.WriteHeader(&tar.Header{Name: name, Mode: 0o644, Size: int64(file.UncompressedSize64), ModTime: file.Modified, Typeflag: tar.TypeReg}); err != nil {
			return err
		}
		part, err := file.Open()
		if err != nil {
			return err
		}
		if _, err := io.Copy(writer, part); err != nil {
			part.Close()
			return err
		}
		part.Close()
	}
	return writer.Close()
}

// downloadBlob fetches a bundle from the server one chunk at a time.
func downloadBlob(ctx context.Context, serverURL, token, ref string, destination io.Writer) error {
	parsed, err := url.Parse(strings.TrimSpace(serverURL))
	if err != nil {
		return err
	}
	switch parsed.Scheme {
	case "ws":
		parsed.Scheme = "http"
	case "wss":
		parsed.Scheme = "https"
	}
	parsed.Path = strings.TrimRight(parsed.Path, "/") + "/api/blobs/" + url.PathEscape(ref)
	parsed.RawQuery, parsed.Fragment = "", ""
	client := &http.Client{Timeout: 90 * time.Second}
	for offset := int64(0); ; {
		request, err := http.NewRequestWithContext(ctx, http.MethodGet, parsed.String()+"?offset="+strconv.FormatInt(offset, 10), nil)
		if err != nil {
			return err
		}
		if token != "" {
			request.Header.Set("Authorization", "Bearer "+token)
		}
		response, err := client.Do(request)
		if err != nil {
			return err
		}
		data, err := io.ReadAll(io.LimitReader(response.Body, pullChunkBytes+1))
		response.Body.Close()
		if err != nil {
			return err
		}
		switch response.StatusCode {
		case http.StatusOK:
			if len(data) > pullChunkBytes {
				return fmt.Errorf("blob chunk at %d exceeds %d bytes", offset, pullChunkBytes)
			}
			if _, err := destination.Write(data); err != nil {
				return err
			}
			offset += int64(len(data))
			if total, _ := strconv.ParseInt(response.Header.Get("X-Spin-Size"), 10, 64); total > 0 && offset >= total {
				return nil
			}
		case http.StatusRequestedRangeNotSatisfiable:
			return nil
		default:
			return fmt.Errorf("blob %s at %d: status %d: %s", ref, offset, response.StatusCode, strings.TrimSpace(string(data)))
		}
	}
}
