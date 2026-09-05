package persistence

import (
	"bytes"
	"crypto/rand"
	"errors"
	"testing"
)

type fakeStorage struct {
	data  []byte
	calls int
}

func (f *fakeStorage) write(offset int64, data []byte) error {
	f.calls++
	end := int(offset) + len(data)
	if end > len(f.data) {
		f.data = append(f.data, make([]byte, end-len(f.data))...)
	}
	copy(f.data[offset:], data)
	return nil
}

// Page-sized sequential writes, the way SQLite fills a journal and a database,
// collapse into one call per limit; the bytes that land are exactly the bytes
// written.
func TestWriteCoalescerJoinsSequentialPagesAndKeepsBytesExact(t *testing.T) {
	storage := &fakeStorage{}
	coalescer := newWriteCoalescer(1<<20, storage.write)
	reference := make([]byte, 3<<20+4096)
	if _, err := rand.Read(reference); err != nil {
		t.Fatal(err)
	}
	for offset := 0; offset < len(reference); offset += 4096 {
		end := min(offset+4096, len(reference))
		if err := coalescer.Write(int64(offset), reference[offset:end]); err != nil {
			t.Fatal(err)
		}
	}
	if storage.calls != 3 {
		t.Fatalf("%d calls before flush, want 3 full runs", storage.calls)
	}
	if !coalescer.Overlaps(int64(len(reference))-100, 50) || coalescer.Overlaps(0, 4096) {
		t.Fatal("overlap detection is wrong about where the pending run is")
	}
	if coalescer.End() != int64(len(reference)) {
		t.Fatalf("pending end = %d", coalescer.End())
	}
	if err := coalescer.Flush(); err != nil {
		t.Fatal(err)
	}
	if storage.calls != 4 || !bytes.Equal(storage.data, reference) {
		t.Fatalf("calls=%d equal=%t", storage.calls, bytes.Equal(storage.data, reference))
	}
	if coalescer.End() != 0 || coalescer.Overlaps(0, int64(len(reference))) {
		t.Fatal("flushed coalescer still reports pending bytes")
	}
}

// Non-sequential writes, overwrites and large writes all end up correct.
func TestWriteCoalescerHandlesGapsOverwritesAndLargeWrites(t *testing.T) {
	storage := &fakeStorage{}
	coalescer := newWriteCoalescer(16, storage.write)
	reference := make([]byte, 90)
	apply := func(offset int, p []byte) {
		t.Helper()
		copy(reference[offset:], p)
		if err := coalescer.Write(int64(offset), p); err != nil {
			t.Fatal(err)
		}
	}
	apply(0, []byte("abcd"))
	apply(4, []byte("efgh"))
	apply(40, []byte("QRST")) // gap: flushes the first run
	if storage.calls != 1 {
		t.Fatalf("gap did not flush: %d calls", storage.calls)
	}
	apply(2, []byte("XY"))                   // overwrite behind the pending run: flushes, then starts anew
	apply(50, bytes.Repeat([]byte("L"), 40)) // larger than the limit: straight through in pieces
	if err := coalescer.Flush(); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(storage.data, reference) {
		t.Fatalf("storage %q\nwant    %q", storage.data, reference)
	}
}

func TestWriteCoalescerDiscardDropsBytesBeyondATruncate(t *testing.T) {
	storage := &fakeStorage{}
	coalescer := newWriteCoalescer(64, storage.write)
	if err := coalescer.Write(10, []byte("0123456789")); err != nil {
		t.Fatal(err)
	}
	coalescer.Discard(15)
	if coalescer.End() != 15 {
		t.Fatalf("pending end after partial discard = %d", coalescer.End())
	}
	coalescer.Discard(5)
	if coalescer.End() != 0 || storage.calls != 0 {
		t.Fatalf("full discard left pending=%d calls=%d", coalescer.End(), storage.calls)
	}
}

func TestWriteCoalescerReportsStorageErrors(t *testing.T) {
	boom := errors.New("volume unavailable")
	coalescer := newWriteCoalescer(8, func(int64, []byte) error { return boom })
	if err := coalescer.Write(0, []byte("1234")); err != nil {
		t.Fatal(err)
	}
	if err := coalescer.Flush(); !errors.Is(err, boom) {
		t.Fatalf("flush error = %v", err)
	}
	if err := coalescer.Write(0, make([]byte, 16)); !errors.Is(err, boom) {
		t.Fatalf("direct write error = %v", err)
	}
}
