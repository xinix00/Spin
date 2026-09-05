// uploadprobe drives Spin's chunked upload API the way a runner does, with
// test data, and reports throughput. It creates a snapshot upload, sends 1 MiB
// chunks with the requested parallelism, reads the status and aborts (or
// completes when asked). Usage:
//
//	go run ./tools/uploadprobe <base-url> <worker-token> <MiB> <parallel> [complete]
//
// Base URL is the server root: http://192.168.1.122 straight into the slot on
// the LAN, or https://bollenloods.getspin.app through Cloudflare and the
// tunnel. Aborting leaves nothing behind; completing stores an orphan object.
package main

import (
	"bytes"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"strconv"
	"sync"
	"time"
)

func main() {
	base := os.Args[1] + "/api/uploads"
	token := os.Args[2]
	mib, _ := strconv.Atoi(os.Args[3])
	parallel, _ := strconv.Atoi(os.Args[4])
	size := int64(mib) << 20
	data := make([]byte, size)
	_, _ = rand.Read(data)
	client := &http.Client{Timeout: 2 * time.Minute}
	do := func(method, path string, body []byte, headers map[string]string) (int, []byte, time.Duration) {
		req, _ := http.NewRequest(method, base+path, bytes.NewReader(body))
		req.ContentLength = int64(len(body))
		req.Header.Set("Authorization", "Bearer "+token)
		for k, v := range headers {
			req.Header.Set(k, v)
		}
		start := time.Now()
		resp, err := client.Do(req)
		if err != nil {
			return 0, []byte(err.Error()), time.Since(start)
		}
		defer resp.Body.Close()
		payload, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<16))
		return resp.StatusCode, payload, time.Since(start)
	}
	createBody, _ := json.Marshal(map[string]any{"kind": "snapshot", "name": "spin-selftest", "size": size,
		"snapshot": map[string]any{"driver": "docker", "ref": "spin/selftest:probe", "digest": "sha256:spin-selftest-probe", "restorable": true}})
	code, payload, took := do(http.MethodPost, "", createBody, map[string]string{"Content-Type": "application/json"})
	fmt.Printf("create: %d %s (%s)\n", code, bytes.TrimSpace(payload), took.Round(time.Millisecond))
	if code != 201 {
		os.Exit(1)
	}
	var session struct {
		ID string `json:"id"`
	}
	_ = json.Unmarshal(payload, &session)
	var mu sync.Mutex
	next := int64(0)
	var wg sync.WaitGroup
	start := time.Now()
	failures := 0
	for w := 0; w < parallel; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for {
				mu.Lock()
				offset := next
				next += 1 << 20
				mu.Unlock()
				if offset >= size {
					return
				}
				end := offset + 1<<20
				if end > size {
					end = size
				}
				code, payload, took := do(http.MethodPut, "/"+session.ID, data[offset:end], map[string]string{"Content-Type": "application/octet-stream", "X-Spin-Upload-Offset": strconv.FormatInt(offset, 10)})
				if code != 200 {
					mu.Lock()
					failures++
					mu.Unlock()
					fmt.Printf("chunk %3d MiB: %d %s (%s)\n", offset>>20, code, bytes.TrimSpace(payload), took.Round(time.Millisecond))
				} else if offset>>20%8 == 0 {
					fmt.Printf("chunk %3d MiB: ok (%s)\n", offset>>20, took.Round(time.Millisecond))
				}
			}
		}()
	}
	wg.Wait()
	elapsed := time.Since(start)
	fmt.Printf("uploaded %d MiB in %s = %.1f MB/s, failures=%d\n", mib, elapsed.Round(time.Millisecond), float64(size)/elapsed.Seconds()/1e6, failures)
	code, payload, took = do(http.MethodGet, "/"+session.ID, nil, nil)
	fmt.Printf("status: %d %s (%s)\n", code, bytes.TrimSpace(payload), took.Round(time.Millisecond))
	if len(os.Args) > 5 && os.Args[5] == "complete" {
		code, payload, took = do(http.MethodPost, "/"+session.ID+"/complete", nil, nil)
		fmt.Printf("complete: %d %s (%s)\n", code, bytes.TrimSpace(payload), took.Round(time.Millisecond))
		return
	}
	code, payload, took = do(http.MethodDelete, "/"+session.ID, nil, nil)
	fmt.Printf("abort: %d %s (%s)\n", code, bytes.TrimSpace(payload), took.Round(time.Millisecond))
}
