package worker

import (
	"context"
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
)

// A runner pulls an archived snapshot itself: 1 MiB per request from
// /api/snapshots/{digest}?offset=, each piece retried on its own, spooled
// to a file and loaded into Docker once complete. The runner link only
// carries the progress lines; a broken line pauses the pull instead of
// failing the launch.

const (
	pullChunkBytes = 1 << 20
	pullAttempts   = 8
)

type snapshotClient struct {
	base   string
	token  string
	client *http.Client
}

func newSnapshotClient(serverURL, token string) (*snapshotClient, error) {
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
	parsed.Path = strings.TrimRight(parsed.Path, "/") + "/api/snapshots/"
	parsed.RawQuery, parsed.Fragment = "", ""
	return &snapshotClient{base: parsed.String(), token: token, client: &http.Client{Timeout: 90 * time.Second}}, nil
}

// chunk fetches the piece at offset. done reports the end of the blob.
func (c *snapshotClient) chunk(ctx context.Context, digest string, offset int64) (data []byte, total int64, done bool, err error) {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, c.base+url.PathEscape(digest)+"?offset="+strconv.FormatInt(offset, 10), nil)
	if err != nil {
		return nil, 0, false, err
	}
	if c.token != "" {
		request.Header.Set("Authorization", "Bearer "+c.token)
	}
	response, err := c.client.Do(request)
	if err != nil {
		return nil, 0, false, err
	}
	defer response.Body.Close()
	total, _ = strconv.ParseInt(response.Header.Get("X-Spin-Size"), 10, 64)
	switch response.StatusCode {
	case http.StatusOK:
		data, err = io.ReadAll(io.LimitReader(response.Body, pullChunkBytes+1))
		if err != nil {
			return nil, total, false, err
		}
		if len(data) > pullChunkBytes {
			return nil, total, false, fmt.Errorf("snapshot chunk at %d exceeds %d bytes", offset, pullChunkBytes)
		}
		return data, total, false, nil
	case http.StatusRequestedRangeNotSatisfiable:
		return nil, total, true, nil
	case http.StatusNotFound:
		return nil, total, false, fmt.Errorf("snapshot %s is not in the archive", digest)
	case http.StatusServiceUnavailable:
		return nil, total, false, errPaused
	}
	body, _ := io.ReadAll(io.LimitReader(response.Body, 4096))
	err = fmt.Errorf("snapshot chunk at %d: status %d: %s", offset, response.StatusCode, strings.TrimSpace(string(body)))
	if response.StatusCode >= 400 && response.StatusCode < 500 && response.StatusCode != http.StatusRequestTimeout && response.StatusCode != http.StatusTooManyRequests {
		// The server said no; asking again will not change its mind.
		err = &permanentError{err}
	}
	return nil, total, false, err
}

var errPaused = fmt.Errorf("server pauses writes")

type permanentError struct{ error }

func (e *permanentError) Unwrap() error { return e.error }

// snapshotPullProcess is the stream the server reads progress from. Its
// Read side yields "SPIN_PULL <received> <total>" lines; Wait reports how
// the load ended.
type snapshotPullProcess struct {
	ctx      context.Context
	cancel   context.CancelFunc
	importer capsule.SnapshotImporter
	client   *snapshotClient
	payload  snapshotPullPayload
	progress *io.PipeReader
	writer   *io.PipeWriter
	done     chan struct{}

	mu        sync.Mutex
	execution capsule.Execution
	err       error
}

func newSnapshotPullProcess(ctx context.Context, importer capsule.SnapshotImporter, client *snapshotClient, payload snapshotPullPayload) *snapshotPullProcess {
	processCtx, cancel := context.WithCancel(ctx)
	reader, writer := io.Pipe()
	process := &snapshotPullProcess{ctx: processCtx, cancel: cancel, importer: importer, client: client, payload: payload, progress: reader, writer: writer, done: make(chan struct{})}
	go process.run()
	return process
}

func (p *snapshotPullProcess) run() {
	err := p.pull()
	p.mu.Lock()
	p.err = err
	if err != nil {
		p.execution.ExitCode = 1
		p.execution.Output = err.Error()
	}
	p.mu.Unlock()
	_ = p.writer.Close()
	close(p.done)
}

func (p *snapshotPullProcess) report(received, total int64) {
	_, _ = fmt.Fprintf(p.writer, "SPIN_PULL %d %d\n", received, total)
}

// pull downloads every chunk with retries, then loads the spool.
func (p *snapshotPullProcess) pull() error {
	spool, err := os.CreateTemp("", "spin-pull-*.tar")
	if err != nil {
		return fmt.Errorf("spool snapshot pull: %w", err)
	}
	defer func() {
		_ = spool.Close()
		_ = os.Remove(spool.Name())
	}()
	digest := strings.TrimSpace(p.payload.Snapshot.Digest)
	if digest == "" {
		return fmt.Errorf("snapshot has no digest to pull")
	}
	total := p.payload.Size
	var received int64
	for {
		data, size, done, err := p.fetch(digest, received)
		if err != nil {
			return err
		}
		if size > 0 {
			total = size
		}
		if done {
			break
		}
		if _, err := spool.Write(data); err != nil {
			return fmt.Errorf("spool snapshot chunk: %w", err)
		}
		received += int64(len(data))
		p.report(received, total)
		if total > 0 && received >= total {
			break
		}
	}
	if total > 0 && received != total {
		return fmt.Errorf("snapshot pull received %d of %d bytes", received, total)
	}
	if err := spool.Sync(); err != nil {
		return err
	}
	if _, err := spool.Seek(0, io.SeekStart); err != nil {
		return err
	}
	return p.importer.ImportSnapshot(p.ctx, p.payload.Snapshot, spool)
}

// fetch retries one chunk with backoff; a server that pauses writes is
// waited for rather than counted as an attempt.
func (p *snapshotPullProcess) fetch(digest string, offset int64) ([]byte, int64, bool, error) {
	var last error
	pauseDeadline := time.Now().Add(uploadPauseLimit)
	for attempt := 1; attempt <= pullAttempts; attempt++ {
		data, total, done, err := p.client.chunk(p.ctx, digest, offset)
		if err == nil {
			return data, total, done, nil
		}
		if p.ctx.Err() != nil {
			return nil, 0, false, p.ctx.Err()
		}
		var permanent *permanentError
		if errors.As(err, &permanent) {
			return nil, 0, false, err
		}
		last = err
		delay := time.Duration(attempt) * 2 * time.Second
		if err == errPaused && time.Now().Before(pauseDeadline) {
			attempt--
			delay = uploadPauseWait
		}
		if delay > 20*time.Second {
			delay = 20 * time.Second
		}
		select {
		case <-p.ctx.Done():
			return nil, 0, false, p.ctx.Err()
		case <-time.After(delay):
		}
	}
	return nil, 0, false, fmt.Errorf("snapshot chunk at %d failed after %d attempts: %w", offset, pullAttempts, last)
}

func (p *snapshotPullProcess) Read(target []byte) (int, error) { return p.progress.Read(target) }
func (p *snapshotPullProcess) Write(data []byte) (int, error)  { return len(data), nil }
func (p *snapshotPullProcess) Close() error                    { return nil }
func (p *snapshotPullProcess) Wait() (capsule.Execution, error) {
	<-p.done
	p.cancel()
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.execution, p.err
}

// parsePullProgress reads one progress line of the pull stream.
func parsePullProgress(line string) (received, total int64, ok bool) {
	fields := strings.Fields(strings.TrimSpace(line))
	if len(fields) != 3 || fields[0] != "SPIN_PULL" {
		return 0, 0, false
	}
	received, err1 := strconv.ParseInt(fields[1], 10, 64)
	total, err2 := strconv.ParseInt(fields[2], 10, 64)
	return received, total, err1 == nil && err2 == nil
}
