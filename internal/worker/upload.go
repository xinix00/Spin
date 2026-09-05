package worker

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

// The runner archives a snapshot the way a browser restores a backup: the
// export lands in a local file first, then travels as separate 1 MiB requests,
// a few in flight at once, each acknowledged with the committed offset. Lean
// accepts nothing larger per request, and a single long stream through the
// tunnel starved the runner socket of its pings while the server wrote.

const uploadAttempts = 5

type uploadSession struct {
	ID        string `json:"id"`
	Offset    int64  `json:"offset"`
	ChunkSize int64  `json:"chunk_size"`
	Parallel  int    `json:"parallel"`
	Size      int64  `json:"size"`
	Error     string `json:"error,omitempty"`
}

type uploadClient struct {
	base   string
	token  string
	client *http.Client
}

func newUploadClient(serverURL, token string) (*uploadClient, error) {
	parsed, err := url.Parse(strings.TrimSpace(serverURL))
	if err != nil {
		return nil, err
	}
	switch parsed.Scheme {
	case "ws":
		parsed.Scheme = "http"
	case "wss":
		parsed.Scheme = "https"
	case "http", "https":
	default:
		return nil, fmt.Errorf("unsupported server URL scheme %q", parsed.Scheme)
	}
	parsed.Path = strings.TrimRight(parsed.Path, "/") + "/api/uploads"
	parsed.RawQuery, parsed.Fragment = "", ""
	return &uploadClient{base: parsed.String(), token: token, client: &http.Client{Timeout: 2 * time.Minute}}, nil
}

func (c *uploadClient) do(ctx context.Context, method, path string, body io.Reader, contentLength int64, headers map[string]string) (int, []byte, error) {
	request, err := http.NewRequestWithContext(ctx, method, c.base+path, body)
	if err != nil {
		return 0, nil, err
	}
	if contentLength > 0 {
		request.ContentLength = contentLength
	}
	request.Header.Set("Authorization", "Bearer "+c.token)
	for key, value := range headers {
		request.Header.Set(key, value)
	}
	response, err := c.client.Do(request)
	if err != nil {
		return 0, nil, err
	}
	defer response.Body.Close()
	payload, err := io.ReadAll(io.LimitReader(response.Body, 1<<20))
	if err != nil {
		return response.StatusCode, nil, err
	}
	return response.StatusCode, payload, nil
}

func (c *uploadClient) create(ctx context.Context, snapshot domain.CapsuleSnapshot, size int64) (uploadSession, error) {
	body, _ := json.Marshal(map[string]any{"kind": "snapshot", "name": snapshot.Ref, "size": size, "snapshot": snapshot})
	status, payload, err := c.do(ctx, http.MethodPost, "", bytes.NewReader(body), int64(len(body)), map[string]string{"Content-Type": "application/json"})
	if err != nil {
		return uploadSession{}, err
	}
	var session uploadSession
	_ = json.Unmarshal(payload, &session)
	if status != http.StatusCreated || session.ID == "" {
		return uploadSession{}, fmt.Errorf("create upload: status %d: %s", status, strings.TrimSpace(firstNonEmpty(session.Error, string(payload))))
	}
	if session.ChunkSize <= 0 || session.ChunkSize > 1<<20 {
		session.ChunkSize = 1 << 20
	}
	if session.Parallel < 1 {
		session.Parallel = 1
	}
	if session.Parallel > 16 {
		session.Parallel = 16
	}
	return session, nil
}

func (c *uploadClient) status(ctx context.Context, id string) (uploadSession, int, error) {
	status, payload, err := c.do(ctx, http.MethodGet, "/"+url.PathEscape(id), nil, 0, nil)
	if err != nil {
		return uploadSession{}, 0, err
	}
	var session uploadSession
	_ = json.Unmarshal(payload, &session)
	return session, status, nil
}

// put sends one chunk and returns the committed offset the server reported.
func (c *uploadClient) put(ctx context.Context, id string, offset int64, data []byte) (int64, int, error) {
	status, payload, err := c.do(ctx, http.MethodPut, "/"+url.PathEscape(id), bytes.NewReader(data), int64(len(data)), map[string]string{
		"Content-Type": "application/octet-stream", "X-Spin-Upload-Offset": strconv.FormatInt(offset, 10),
	})
	if err != nil {
		return 0, 0, err
	}
	var session uploadSession
	_ = json.Unmarshal(payload, &session)
	if status != http.StatusOK && status != http.StatusConflict {
		return session.Offset, status, fmt.Errorf("upload chunk at %d: status %d: %s", offset, status, strings.TrimSpace(firstNonEmpty(session.Error, string(payload))))
	}
	return session.Offset, status, nil
}

func (c *uploadClient) complete(ctx context.Context, id string) (archiveResult, error) {
	status, payload, err := c.do(ctx, http.MethodPost, "/"+url.PathEscape(id)+"/complete", nil, 0, nil)
	if err != nil {
		return archiveResult{}, err
	}
	if status != http.StatusOK {
		var failure uploadSession
		_ = json.Unmarshal(payload, &failure)
		return archiveResult{}, fmt.Errorf("complete upload: status %d: %s", status, strings.TrimSpace(firstNonEmpty(failure.Error, string(payload))))
	}
	var result archiveResult
	if err := json.Unmarshal(payload, &result); err != nil {
		return archiveResult{}, err
	}
	return result, nil
}

func (c *uploadClient) abort(id string) {
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	_, _, _ = c.do(ctx, http.MethodDelete, "/"+url.PathEscape(id), nil, 0, nil)
}

// archiveSnapshot exports the snapshot to a temporary file and uploads it.
func (w *Worker) archiveSnapshot(ctx context.Context, exporter capsule.SnapshotExporter, snapshot domain.CapsuleSnapshot) (archiveResult, error) {
	client, err := newUploadClient(w.config.ServerURL, w.config.Token)
	if err != nil {
		return archiveResult{}, err
	}
	file, err := os.CreateTemp("", "spin-snapshot-*.tar")
	if err != nil {
		return archiveResult{}, err
	}
	defer func() {
		_ = file.Close()
		_ = os.Remove(file.Name())
	}()
	if err := exporter.ExportSnapshot(ctx, snapshot, file); err != nil {
		return archiveResult{}, err
	}
	info, err := file.Stat()
	if err != nil {
		return archiveResult{}, err
	}
	return uploadSnapshot(ctx, client, snapshot, file, info.Size())
}

// uploadSnapshot drives one chunked upload to completion: create, a pool of
// workers sending chunks with retries, then complete. Any failure aborts the
// upload server-side so no half-assembled object lingers.
func uploadSnapshot(ctx context.Context, client *uploadClient, snapshot domain.CapsuleSnapshot, source io.ReaderAt, size int64) (archiveResult, error) {
	if size <= 0 {
		return archiveResult{}, errors.New("snapshot export is empty")
	}
	session, err := client.create(ctx, snapshot, size)
	if err != nil {
		return archiveResult{}, err
	}
	if err := uploadChunks(ctx, client, session, source, size); err != nil {
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

func uploadChunks(ctx context.Context, client *uploadClient, session uploadSession, source io.ReaderAt, size int64) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	var (
		mu       sync.Mutex
		next     = session.Offset
		firstErr error
	)
	fail := func(err error) {
		mu.Lock()
		if firstErr == nil {
			firstErr = err
		}
		mu.Unlock()
		cancel()
	}
	take := func() (int64, int64, bool) {
		mu.Lock()
		defer mu.Unlock()
		if next >= size || firstErr != nil {
			return 0, 0, false
		}
		offset := next
		end := min(offset+session.ChunkSize, size)
		next = end
		return offset, end, true
	}
	var group sync.WaitGroup
	for worker := 0; worker < session.Parallel; worker++ {
		group.Add(1)
		go func() {
			defer group.Done()
			buffer := make([]byte, session.ChunkSize)
			for {
				offset, end, ok := take()
				if !ok {
					return
				}
				chunk := buffer[:end-offset]
				if _, err := source.ReadAt(chunk, offset); err != nil && !errors.Is(err, io.EOF) {
					fail(fmt.Errorf("read snapshot at %d: %w", offset, err))
					return
				}
				if err := sendChunk(ctx, client, session.ID, offset, end, chunk); err != nil {
					fail(err)
					return
				}
			}
		}()
	}
	group.Wait()
	return firstErr
}

// sendChunk retries one chunk with backoff. A 409 whose committed offset has
// already passed this chunk means an earlier attempt landed; a 404 means the
// upload is gone and nothing here can bring it back.
func sendChunk(ctx context.Context, client *uploadClient, id string, offset, end int64, chunk []byte) error {
	var last error
	for attempt := 1; attempt <= uploadAttempts; attempt++ {
		committed, status, err := client.put(ctx, id, offset, chunk)
		switch {
		case err == nil && status == http.StatusOK:
			return nil
		case err == nil && status == http.StatusConflict && committed >= end:
			return nil
		case status == http.StatusNotFound:
			return fmt.Errorf("upload %s disappeared while sending chunk at %d", id, offset)
		case status == http.StatusRequestEntityTooLarge:
			return err
		}
		if err == nil {
			err = fmt.Errorf("chunk at %d was not accepted (status %d, committed %d)", offset, status, committed)
		}
		last = err
		if ctx.Err() != nil {
			return ctx.Err()
		}
		delay := time.Duration(500*(1<<(attempt-1))) * time.Millisecond
		if delay > 5*time.Second {
			delay = 5 * time.Second
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(delay):
		}
	}
	return fmt.Errorf("chunk at %d failed after %d attempts: %w", offset, uploadAttempts, last)
}

func firstNonEmpty(values ...string) string {
	for _, value := range values {
		if strings.TrimSpace(value) != "" {
			return value
		}
	}
	return ""
}
