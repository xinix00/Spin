package worker

import (
	"bytes"
	"context"
	"errors"
	"io"
	"os"
	"testing"
	"time"

	"easyacp/internal/domain"
)

type recordingImporter struct {
	got []byte
	err error
}

func (r *recordingImporter) ImportSnapshot(_ context.Context, _ domain.CapsuleSnapshot, source io.Reader) error {
	data, err := io.ReadAll(source)
	r.got = data
	if err != nil {
		return err
	}
	return r.err
}

// The import spools to disk and loads only once the stream closes; the bytes
// arrive intact and the spool is gone afterwards.
func TestSnapshotImportProcessSpoolsThenLoads(t *testing.T) {
	importer := &recordingImporter{}
	process := newSnapshotImportProcess(context.Background(), importer, domain.CapsuleSnapshot{Driver: "docker", Ref: "spin/artifact:x"})
	payload := bytes.Repeat([]byte("layer"), 100000)
	for offset := 0; offset < len(payload); offset += 4096 {
		end := min(offset+4096, len(payload))
		if _, err := process.Write(payload[offset:end]); err != nil {
			t.Fatal(err)
		}
	}
	if importer.got != nil {
		t.Fatal("import started before the stream closed")
	}
	spool := process.spool.Name()
	if err := process.Close(); err != nil {
		t.Fatal(err)
	}
	execution, err := process.Wait()
	if err != nil || execution.ExitCode != 0 || !bytes.Equal(importer.got, payload) {
		t.Fatalf("import = %+v, %v, %d bytes", execution, err, len(importer.got))
	}
	if _, err := process.Write([]byte("late")); !errors.Is(err, io.ErrClosedPipe) {
		t.Fatalf("write after close = %v", err)
	}
	if _, statErr := osStat(spool); statErr == nil {
		t.Fatal("spool file was not removed")
	}
}

// A stream abandoned before it closed never loads a truncated image.
func TestSnapshotImportProcessFailsWhenAbandoned(t *testing.T) {
	importer := &recordingImporter{}
	ctx, cancel := context.WithCancel(context.Background())
	process := newSnapshotImportProcess(ctx, importer, domain.CapsuleSnapshot{Driver: "docker", Ref: "spin/artifact:x"})
	if _, err := process.Write([]byte("half")); err != nil {
		t.Fatal(err)
	}
	cancel()
	finished := make(chan struct{})
	go func() { _, _ = process.Wait(); close(finished) }()
	select {
	case <-finished:
	case <-time.After(2 * time.Second):
		t.Fatal("abandoned import did not finish")
	}
	if importer.got != nil {
		t.Fatal("abandoned import loaded a truncated image")
	}
}

// A one-shot transfer on a peer that loses its connection fails at once,
// while an interactive stream on the same peer survives the reconnect.
func TestBulkStreamsFailWhenThePeerDisconnects(t *testing.T) {
	peer := newRunnerPeer(domain.Client{ID: "cli_x"})
	bulk, pty := newRemoteProcess(peer, "str_bulk"), newRemoteProcess(peer, "str_pty")
	bulk.bulk = true
	peer.streams[bulk.id], peer.streams[pty.id] = bulk, pty
	peer.failBulkStreams("runner connection lost during the transfer")
	if _, err := bulk.Wait(); err == nil {
		t.Fatal("bulk stream survived the disconnect")
	}
	select {
	case <-pty.done:
		t.Fatal("interactive stream was failed by the disconnect")
	default:
	}
	if _, ok := peer.streams[pty.id]; !ok {
		t.Fatal("interactive stream was dropped from the peer")
	}
}

func osStat(name string) (os.FileInfo, error) { return os.Stat(name) }

// Request IDs must never repeat across server incarnations: a runner answers
// a repeated ID from its cache of earlier responses, so a restarted server
// counting from one again would receive answers to other requests.
func TestBrokerRequestIDsAreUniquePerIncarnation(t *testing.T) {
	first, second := NewBroker(nil, nil), NewBroker(nil, nil)
	seen := map[string]bool{}
	for _, broker := range []*Broker{first, second} {
		for range 3 {
			id := broker.requestID("rpc")
			if seen[id] {
				t.Fatalf("request ID %s repeated across brokers", id)
			}
			seen[id] = true
		}
	}
	if first.requestID("str") == second.requestID("str") {
		t.Fatal("two server incarnations produced the same stream ID")
	}
}
