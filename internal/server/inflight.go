package server

import (
	"log/slog"
	"net/http"
	"runtime"
	"strings"
	"sync"
	"time"
)

// A request that runs long is logged with the stacks of the goroutines that
// are doing something. On HopOS a goroutine that never blocks starves the
// whole app, and the log is the only place such a hang shows; the request
// line at the start says what was asked, the dump says where it sits.

const (
	slowRequestAfter = 20 * time.Second
	slowRequestCheck = 5 * time.Second
	stackDumpLimit   = 64 << 10
)

type inflightRequest struct {
	method, path string
	started      time.Time
	reported     bool
}

type inflight struct {
	mu       sync.Mutex
	next     uint64
	requests map[uint64]*inflightRequest
	watching sync.Once
}

func (i *inflight) begin(r *http.Request, logger *slog.Logger) uint64 {
	i.mu.Lock()
	defer i.mu.Unlock()
	if i.requests == nil {
		i.requests = map[uint64]*inflightRequest{}
	}
	i.next++
	i.requests[i.next] = &inflightRequest{method: r.Method, path: r.URL.Path, started: time.Now()}
	i.watching.Do(func() { go i.watch(logger) })
	return i.next
}

func (i *inflight) end(id uint64) {
	i.mu.Lock()
	defer i.mu.Unlock()
	delete(i.requests, id)
}

func (i *inflight) watch(logger *slog.Logger) {
	for range time.Tick(slowRequestCheck) {
		var slow []*inflightRequest
		i.mu.Lock()
		for _, request := range i.requests {
			if !request.reported && time.Since(request.started) > slowRequestAfter && !quietRequestPath(request.path) {
				request.reported = true
				slow = append(slow, request)
			}
		}
		i.mu.Unlock()
		if len(slow) == 0 {
			continue
		}
		for _, request := range slow {
			logger.Warn("slow http request", "method", request.method, "path", request.path, "running", time.Since(request.started).Round(time.Second))
		}
		logger.Warn("goroutines doing something", "stacks", busyGoroutines())
	}
}

// busyGoroutines is the stack dump without the goroutines that merely wait
// for the network, a channel or a timer.
func busyGoroutines() string {
	buffer := make([]byte, 1<<20)
	size := runtime.Stack(buffer, true)
	var kept []string
	total := 0
	for _, block := range strings.Split(string(buffer[:size]), "\n\n") {
		header, _, _ := strings.Cut(block, "\n")
		if strings.Contains(header, "[IO wait") || strings.Contains(header, "[select") || strings.Contains(header, "[chan ") || strings.Contains(header, "[sleep") || strings.Contains(header, "[idle") {
			continue
		}
		if total+len(block) > stackDumpLimit {
			break
		}
		kept = append(kept, block)
		total += len(block)
	}
	return strings.Join(kept, "\n\n")
}

// quietRequestPath is a request that comes often or lives long by design:
// health checks, the state stream, the runner link, asset and chunk traffic.
func quietRequestPath(path string) bool {
	return path == "/healthz" || path == "/api/auth/status" || path == "/api/state" || path == "/api/state/ws" || path == "/api/runner/ws" ||
		strings.HasPrefix(path, "/assets/") || strings.HasPrefix(path, "/api/uploads") || strings.HasPrefix(path, "/api/snapshots/") || strings.HasPrefix(path, "/api/sessions/")
}
