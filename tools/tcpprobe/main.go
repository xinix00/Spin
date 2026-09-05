package main

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strconv"
	"time"
)

// tcpprobe times one chunk PUT over a raw TCP connection: how long pushing the
// 1 MiB body into the slot takes versus how long the server then takes to
// answer. Usage:
//
//	go run ./tools/tcpprobe <host> <worker-token>
//
// Host is the slot's LAN address (port 80). Eight chunks are sent on fresh
// connections; the upload is aborted afterwards.
func main() {
	host, token := os.Args[1], os.Args[2]
	client := &http.Client{Timeout: 30 * time.Second}
	createBody, _ := json.Marshal(map[string]any{"kind": "snapshot", "name": "tcp-probe", "size": 8 << 20,
		"snapshot": map[string]any{"driver": "docker", "ref": "spin/selftest:tcp", "digest": "sha256:spin-selftest-tcp", "restorable": true}})
	req, _ := http.NewRequest("POST", "http://"+host+"/api/uploads", bytes.NewReader(createBody))
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Content-Type", "application/json")
	resp, err := client.Do(req)
	if err != nil {
		panic(err)
	}
	var session struct {
		ID string `json:"id"`
	}
	_ = json.NewDecoder(resp.Body).Decode(&session)
	resp.Body.Close()
	fmt.Println("upload", session.ID)
	data := make([]byte, 1<<20)
	_, _ = rand.Read(data)
	for i := 0; i < 8; i++ {
		start := time.Now()
		conn, err := net.DialTimeout("tcp", host+":80", 5*time.Second)
		if err != nil {
			panic(err)
		}
		dialed := time.Since(start)
		header := fmt.Sprintf("PUT /api/uploads/%s HTTP/1.1\r\nHost: %s\r\nAuthorization: Bearer %s\r\nContent-Type: application/octet-stream\r\nX-Spin-Upload-Offset: %d\r\nContent-Length: %d\r\nConnection: close\r\n\r\n", session.ID, host, token, i<<20, len(data))
		t0 := time.Now()
		_, _ = conn.Write([]byte(header))
		_, err = conn.Write(data)
		pushed := time.Since(t0)
		if err != nil {
			panic(err)
		}
		t1 := time.Now()
		reader := bufio.NewReader(conn)
		line, _ := reader.ReadString('\n')
		responded := time.Since(t1)
		_, _ = io.Copy(io.Discard, reader)
		conn.Close()
		fmt.Printf("chunk %d: dial %s · body pushed in %s · response after %s more · %s\n", i, dialed.Round(time.Millisecond), pushed.Round(time.Millisecond), responded.Round(time.Millisecond), bytes.TrimSpace([]byte(line)))
	}
	del, _ := http.NewRequest("DELETE", "http://"+host+"/api/uploads/"+session.ID, nil)
	del.Header.Set("Authorization", "Bearer "+token)
	if resp, err := client.Do(del); err == nil {
		resp.Body.Close()
		fmt.Println("abort", strconv.Itoa(resp.StatusCode))
	}
}
