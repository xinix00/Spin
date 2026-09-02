package server

import (
	"bytes"
	"context"
	"io"
	"log/slog"
	"net"
	"net/http"
	"strings"
	"sync"
	"testing"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
	"github.com/gorilla/websocket"
)

type interactiveTestEngine struct {
	testEngine
}

type singleConnListener struct {
	conn net.Conn
	once sync.Once
}

func (l *singleConnListener) Accept() (net.Conn, error) {
	var connection net.Conn
	l.once.Do(func() { connection = l.conn })
	if connection == nil {
		return nil, net.ErrClosed
	}
	return connection, nil
}

func (l *singleConnListener) Close() error   { return nil }
func (l *singleConnListener) Addr() net.Addr { return pipeAddr("spin.test") }

type pipeAddr string

func (a pipeAddr) Network() string { return "pipe" }
func (a pipeAddr) String() string  { return string(a) }

func (e *interactiveTestEngine) StartInteractive(_ context.Context, _ domain.Recording, _ string, _, _ uint16) (capsule.InteractiveProcess, error) {
	return newInteractiveTestProcess(), nil
}

type interactiveTestProcess struct {
	reader *io.PipeReader
	writer *io.PipeWriter
	mu     sync.Mutex
	input  bytes.Buffer
	once   sync.Once
}

func newInteractiveTestProcess() *interactiveTestProcess {
	reader, writer := io.Pipe()
	process := &interactiveTestProcess{reader: reader, writer: writer}
	go func() { _, _ = writer.Write([]byte("device code? ")) }()
	return process
}

func (p *interactiveTestProcess) Read(buffer []byte) (int, error) { return p.reader.Read(buffer) }

func (p *interactiveTestProcess) Write(buffer []byte) (int, error) {
	p.mu.Lock()
	_, _ = p.input.Write(buffer)
	p.mu.Unlock()
	if bytes.Contains(buffer, []byte("1234")) {
		p.once.Do(func() {
			_, _ = p.writer.Write([]byte("authenticated\r\n"))
			_ = p.writer.Close()
		})
	}
	return len(buffer), nil
}

func (p *interactiveTestProcess) Close() error {
	p.once.Do(func() { _ = p.writer.Close() })
	return p.reader.Close()
}

func (p *interactiveTestProcess) Resize(_, _ uint16) error { return nil }

func (p *interactiveTestProcess) Wait() (capsule.Execution, error) {
	return capsule.Execution{Output: "device code? authenticated", ExitCode: 0}, nil
}

func TestRecordingTerminalStreamsInputOutputAndAuditsCommand(t *testing.T) {
	st, err := store.Open("")
	if err != nil {
		t.Fatal(err)
	}
	engine := &interactiveTestEngine{}
	srv := NewWithOptions(st, slog.New(slog.NewTextHandler(io.Discard, nil)), engine, ServerOptions{DisableAuthentication: true})
	started, err := srv.runCommand(domain.CommandRequest{Operator: "derek", Line: "RECORD tool:codex --scope=global"})
	if err != nil {
		t.Fatal(err)
	}
	recording := started.Recording
	if recording == nil {
		t.Fatal("recording was not created")
	}

	clientConn, serverConn := net.Pipe()
	go func() { _ = (&http.Server{Handler: srv.Handler()}).Serve(&singleConnListener{conn: serverConn}) }()
	dialer := websocket.Dialer{NetDial: func(_, _ string) (net.Conn, error) { return clientConn, nil }}
	url := "ws://spin.test/api/recordings/" + recording.ID + "/terminal?operator=derek"
	connection, _, err := dialer.Dial(url, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if err := connection.WriteJSON(terminalMessage{Type: "start", Command: "codex login --device-auth", Rows: 24, Cols: 100}); err != nil {
		t.Fatal(err)
	}

	sawPrompt, sawAuthenticated, sawExit := false, false, false
	for !sawExit {
		var message terminalMessage
		if err := connection.ReadJSON(&message); err != nil {
			t.Fatal(err)
		}
		switch message.Type {
		case "output":
			sawPrompt = sawPrompt || strings.Contains(message.Data, "device code?")
			sawAuthenticated = sawAuthenticated || strings.Contains(message.Data, "authenticated")
			if sawPrompt && !sawAuthenticated {
				if err := connection.WriteJSON(terminalMessage{Type: "input", Data: "1234\r"}); err != nil {
					t.Fatal(err)
				}
			}
		case "error":
			t.Fatalf("terminal error: %s", message.Error)
		case "exit":
			sawExit = message.ExitCode != nil && *message.ExitCode == 0
		}
	}
	if !sawPrompt || !sawAuthenticated {
		t.Fatalf("streamed output: prompt=%t authenticated=%t", sawPrompt, sawAuthenticated)
	}
	stored, err := st.Recording(recording.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(stored.Commands) != 1 || stored.Commands[0].ExitCode == nil || *stored.Commands[0].ExitCode != 0 {
		t.Fatalf("recording audit = %+v", stored.Commands)
	}
}

func TestCapsuleAllowsEightParallelTerminals(t *testing.T) {
	srv := &Server{terminals: map[string]map[*activeTerminal]struct{}{}}
	terminals := make([]*activeTerminal, 0, 8)
	for range 8 {
		terminal, ok := srv.reserveTerminal("recording")
		if !ok {
			t.Fatalf("terminal %d was rejected", len(terminals)+1)
		}
		terminals = append(terminals, terminal)
	}
	if terminal, ok := srv.reserveTerminal("recording"); ok || terminal != nil {
		t.Fatal("ninth terminal was accepted")
	}
	srv.releaseTerminal("recording", terminals[0])
	if !srv.terminalBusy("recording") {
		t.Fatal("releasing one terminal hid the other live terminals")
	}
	for _, terminal := range terminals[1:] {
		srv.releaseTerminal("recording", terminal)
	}
	if srv.terminalBusy("recording") {
		t.Fatal("recording remained busy after every terminal exited")
	}
}
