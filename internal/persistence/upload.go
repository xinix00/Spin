package persistence

import (
	"context"
	"errors"
	"fmt"
	"io"
	"sort"
	"sync"
)

// ErrUploadOffset reports a chunk that straddles the committed prefix or lands
// beyond the reorder window. The committed offset travels with it so a client
// can resume from the first byte that did not reach storage.
var ErrUploadOffset = errors.New("upload offset mismatch")

// ErrBackupUploadOffset is the historical name of ErrUploadOffset.
var ErrBackupUploadOffset = ErrUploadOffset

// uploadReorderWindow bounds how far ahead of the committed prefix a chunk may
// land. Parallel uploaders keep a handful of chunks in flight; anything further
// ahead is a client bug rather than reordering.
const uploadReorderWindow int64 = 64 << 20

// chunkSink stores the bytes of one chunk at an offset. It never decides what
// a chunk means: ordering, retries and completeness are the assembler's.
type chunkSink interface {
	writeChunk(ctx context.Context, offset, length int64, source io.Reader) (int64, error)
}

type uploadSpan struct{ start, end int64 }

// chunkAssembler is the one definition of a chunked upload in Spin. Every
// transport that has to move something large through a 1 MiB-per-request
// boundary, whether a browser restoring a backup or a runner archiving a
// snapshot, hands its chunks here. Chunks may arrive concurrently and out of
// order; the assembler tracks the contiguous committed prefix and keeps
// retries idempotent.
type chunkAssembler struct {
	mu       sync.Mutex
	sink     chunkSink
	maxBytes int64
	offset   int64        // contiguous committed prefix
	ahead    []uploadSpan // completed chunks beyond offset, sorted by start
	pending  int          // writes in flight
	closed   bool
}

func newChunkAssembler(sink chunkSink, maxBytes int64) *chunkAssembler {
	return &chunkAssembler{sink: sink, maxBytes: maxBytes}
}

// Offset reports the committed prefix.
func (a *chunkAssembler) Offset() int64 {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.offset
}

// WriteAt stores one chunk and reports the committed prefix. A chunk entirely
// inside the prefix is acknowledged without rewriting it, so retries stay
// idempotent. A chunk that straddles the prefix boundary or lands beyond the
// reorder window reports ErrUploadOffset with the prefix. The sink write runs
// outside the lock so several chunks can stream at once.
func (a *chunkAssembler) WriteAt(ctx context.Context, offset, length int64, source io.Reader) (int64, error) {
	a.mu.Lock()
	if a.closed {
		a.mu.Unlock()
		return a.offset, errors.New("upload is closed")
	}
	if length <= 0 {
		a.mu.Unlock()
		return a.offset, errors.New("upload chunk is empty")
	}
	if offset < 0 || length > a.maxBytes-offset {
		a.mu.Unlock()
		return a.offset, errors.New("upload exceeds its declared size")
	}
	if offset+length <= a.offset {
		committed := a.offset
		a.mu.Unlock()
		return committed, nil
	}
	if offset < a.offset || offset > a.offset+uploadReorderWindow {
		committed := a.offset
		a.mu.Unlock()
		return committed, fmt.Errorf("%w: received %d, committed %d", ErrUploadOffset, offset, committed)
	}
	a.pending++
	a.mu.Unlock()

	written, err := a.sink.writeChunk(ctx, offset, length, io.LimitReader(source, length))
	if err == nil && written != length {
		err = io.ErrUnexpectedEOF
	}

	a.mu.Lock()
	defer a.mu.Unlock()
	a.pending--
	if a.closed {
		return a.offset, errors.New("upload is closed")
	}
	if err != nil {
		return a.offset, err
	}
	a.commit(uploadSpan{start: offset, end: offset + length})
	return a.offset, nil
}

func (a *chunkAssembler) commit(span uploadSpan) {
	index := sort.Search(len(a.ahead), func(i int) bool { return a.ahead[i].start >= span.start })
	a.ahead = append(a.ahead, uploadSpan{})
	copy(a.ahead[index+1:], a.ahead[index:])
	a.ahead[index] = span
	consumed := 0
	for consumed < len(a.ahead) && a.ahead[consumed].start <= a.offset {
		if a.ahead[consumed].end > a.offset {
			a.offset = a.ahead[consumed].end
		}
		consumed++
	}
	a.ahead = append(a.ahead[:0], a.ahead[consumed:]...)
}

// finish checks that every byte up to expectedSize has been committed, that
// nothing is still in flight, and closes the assembler to further writes.
func (a *chunkAssembler) finish(expectedSize int64) error {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.closed {
		return errors.New("upload is closed")
	}
	if a.pending > 0 {
		return errors.New("upload still has chunks in flight")
	}
	if a.offset != expectedSize {
		return fmt.Errorf("upload is incomplete: received %d of %d bytes", a.offset, expectedSize)
	}
	a.closed = true
	return nil
}

// close stops further writes; it reports whether the assembler was still open.
func (a *chunkAssembler) close() bool {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.closed {
		return false
	}
	a.closed = true
	return true
}
