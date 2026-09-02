package server

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"

	"easyacp/internal/capsule"
	"easyacp/internal/store"
	"github.com/gorilla/websocket"
)

type terminalMessage struct {
	Type     string `json:"type"`
	Command  string `json:"command,omitempty"`
	Data     string `json:"data,omitempty"`
	Rows     uint16 `json:"rows,omitempty"`
	Cols     uint16 `json:"cols,omitempty"`
	ExitCode *int   `json:"exit_code,omitempty"`
	Error    string `json:"error,omitempty"`
}

type activeTerminal struct {
	cancel  context.CancelFunc
	process capsule.InteractiveProcess
}

var terminalUpgrader = websocket.Upgrader{
	ReadBufferSize:  4096,
	WriteBufferSize: 4096,
}

func (s *Server) recordingTerminal(w http.ResponseWriter, r *http.Request) {
	interactive, ok := s.engine.(capsule.InteractiveEngine)
	if !ok {
		writeJSON(w, http.StatusNotImplemented, map[string]string{"error": "capsule engine has no interactive terminal"})
		return
	}
	actor := s.requestOperator(r, r.URL.Query().Get("operator"))
	recording, err := s.store.Recording(r.PathValue("recordingID"))
	if err != nil {
		writeError(w, err)
		return
	}
	open, err := s.store.OpenRecording(actor)
	if err != nil || open.ID != recording.ID || actor != recording.Actor {
		writeError(w, store.ErrConflict)
		return
	}
	connection, err := terminalUpgrader.Upgrade(w, r, nil)
	if err != nil {
		s.logger.Debug("upgrade capsule terminal", "error", err)
		return
	}
	defer connection.Close()
	connection.SetReadLimit(1 << 20)

	var start terminalMessage
	if err := connection.ReadJSON(&start); err != nil || start.Type != "start" || strings.TrimSpace(start.Command) == "" {
		_ = connection.WriteJSON(terminalMessage{Type: "error", Error: "first terminal message must start a command"})
		return
	}
	terminal, ok := s.reserveTerminal(recording.ID)
	if !ok {
		_ = connection.WriteJSON(terminalMessage{Type: "error", Error: "this capsule already has the maximum of 8 interactive processes"})
		return
	}
	defer s.releaseTerminal(recording.ID, terminal)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	process, err := interactive.StartInteractive(ctx, recording, start.Command, boundedTerminalSize(start.Rows, 30), boundedTerminalSize(start.Cols, 120))
	if err != nil {
		_ = connection.WriteJSON(terminalMessage{Type: "error", Error: err.Error()})
		return
	}
	s.bindTerminal(terminal, cancel, process)
	defer process.Close()
	if err := connection.WriteJSON(terminalMessage{Type: "ready"}); err != nil {
		return
	}

	disconnected := make(chan struct{})
	go func() {
		defer close(disconnected)
		for {
			var message terminalMessage
			if err := connection.ReadJSON(&message); err != nil {
				cancel()
				_ = process.Close()
				return
			}
			switch message.Type {
			case "input":
				_, _ = process.Write([]byte(message.Data))
			case "interrupt":
				_, _ = process.Write([]byte{3})
			case "resize":
				_ = process.Resize(boundedTerminalSize(message.Rows, 30), boundedTerminalSize(message.Cols, 120))
			}
		}
	}()

	buffer := make([]byte, 32<<10)
	for {
		count, readErr := process.Read(buffer)
		if count > 0 {
			if err := connection.WriteJSON(terminalMessage{Type: "output", Data: string(buffer[:count])}); err != nil {
				cancel()
				_ = process.Close()
				break
			}
		}
		if readErr != nil {
			if !errors.Is(readErr, io.EOF) {
				s.logger.Debug("read capsule PTY", "recording", recording.ID, "error", readErr)
			}
			break
		}
	}
	execution, waitErr := process.Wait()
	exitCode := execution.ExitCode
	_, appendErr := s.store.RecordExecution(recording.ID, actor, &exitCode)
	if waitErr != nil && appendErr == nil {
		appendErr = fmt.Errorf("wait for interactive command: %w", waitErr)
	}
	if appendErr != nil {
		_ = connection.WriteJSON(terminalMessage{Type: "error", Error: appendErr.Error()})
		return
	}
	_ = connection.WriteJSON(terminalMessage{Type: "exit", ExitCode: &exitCode})
	select {
	case <-disconnected:
	default:
	}
}

func boundedTerminalSize(value, fallback uint16) uint16 {
	if value == 0 {
		return fallback
	}
	if value > 500 {
		return 500
	}
	return value
}

func (s *Server) reserveTerminal(recordingID string) (*activeTerminal, bool) {
	s.terminalMu.Lock()
	defer s.terminalMu.Unlock()
	active := s.terminals[recordingID]
	if active == nil {
		active = map[*activeTerminal]struct{}{}
		s.terminals[recordingID] = active
	}
	if len(active) >= 8 {
		return nil, false
	}
	terminal := &activeTerminal{}
	active[terminal] = struct{}{}
	return terminal, true
}

func (s *Server) bindTerminal(terminal *activeTerminal, cancel context.CancelFunc, process capsule.InteractiveProcess) {
	s.terminalMu.Lock()
	defer s.terminalMu.Unlock()
	terminal.cancel = cancel
	terminal.process = process
}

func (s *Server) releaseTerminal(recordingID string, terminal *activeTerminal) {
	s.terminalMu.Lock()
	if active := s.terminals[recordingID]; active != nil {
		delete(active, terminal)
		if len(active) == 0 {
			delete(s.terminals, recordingID)
		}
	}
	s.terminalMu.Unlock()
}

func (s *Server) terminalBusy(recordingID string) bool {
	s.terminalMu.Lock()
	defer s.terminalMu.Unlock()
	return len(s.terminals[recordingID]) > 0
}

func (s *Server) stopTerminal(recordingID string) {
	s.terminalMu.Lock()
	active := make([]*activeTerminal, 0, len(s.terminals[recordingID]))
	for terminal := range s.terminals[recordingID] {
		active = append(active, terminal)
	}
	s.terminalMu.Unlock()
	for _, terminal := range active {
		if terminal.process != nil {
			_, _ = terminal.process.Write([]byte{3})
			_ = terminal.process.Close()
		}
		if terminal.cancel != nil {
			terminal.cancel()
		}
	}
}
