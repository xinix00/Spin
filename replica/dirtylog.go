package replica

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"sync"
)

// The dirty log is how a start after an unclean stop knows which pages
// changed since the last sync without reading the database, the way a
// filesystem knows which blocks a snapshot must carry because it noted
// them when they were written. Before the database file is synced, the
// numbers of the pages written since the last sync are appended to a small
// file next to it and synced first, as a journal goes before the pages it
// protects; a page can then be on disk only when the log names it. After a
// committed sync the log is rewritten with what is still dirty, into the
// other of two files, so a crash during the rewrite leaves the previous
// log, which names more, never less. A record is a page number and its
// complement: a crash mid-append leaves a torn last record, which is
// dropped; anything else wrong makes the log untrusted, and the generation
// starts over.

const (
	dirtyLogMagic = "SPINDRT1"
	// The header is the magic, the sequence and its complement, and the
	// generation the sequence counts in; a rewrite puts the records down
	// first and the header last, so a header that reads means the records
	// are all there.
	dirtyLogFixedHeader = len(dirtyLogMagic) + 16 + 2
	dirtyLogRecord      = 8
	dirtyLogSum         = 8
	dirtyLogChunkBytes  = 64 << 10
)

// The header ends in a checksum over the rest of it, so a damaged
// generation or sequence is refused rather than read as another one.
func dirtyLogHeaderSize(generation string) int {
	return dirtyLogFixedHeader + len(generation) + dirtyLogSum
}

func dirtyLogChecksum(header []byte) []byte {
	sum := sha256.Sum256(header[:len(header)-dirtyLogSum])
	return sum[:dirtyLogSum]
}

type dirtyLog struct {
	files Storage
	paths [2]string

	mu      sync.Mutex
	current int
	file    File
	size    int64
	noted   map[uint32]struct{}
	pending []uint32
	broken  bool
}

func newDirtyLog(files Storage, path string) *dirtyLog {
	return &dirtyLog{files: files, paths: [2]string{path + ".replica-dirty-a", path + ".replica-dirty-b"}, noted: map[uint32]struct{}{}}
}

// note remembers a page written since the last sync; it goes to disk at
// the next flush.
func (l *dirtyLog) note(page uint32) {
	l.mu.Lock()
	defer l.mu.Unlock()
	if _, seen := l.noted[page]; seen {
		return
	}
	l.noted[page] = struct{}{}
	l.pending = append(l.pending, page)
}

// markBroken records that a write could not be attributed to pages; the
// log then says so and the next start takes a full snapshot.
func (l *dirtyLog) markBroken() {
	l.mu.Lock()
	defer l.mu.Unlock()
	if !l.broken {
		l.broken = true
		l.pending = append(l.pending, 0)
	}
}

// Records can name millions of pages; only their numbers need to be in
// memory together, not a second, encoded copy of the entire log.
func writeDirtyRecords(file File, pages []uint32, offset int64) error {
	data := make([]byte, min(len(pages), dirtyLogChunkBytes/dirtyLogRecord)*dirtyLogRecord)
	for len(pages) > 0 {
		count := min(len(pages), len(data)/dirtyLogRecord)
		chunk := data[:count*dirtyLogRecord]
		for i, page := range pages[:count] {
			record := chunk[i*dirtyLogRecord:]
			binary.BigEndian.PutUint32(record[:4], page)
			binary.BigEndian.PutUint32(record[4:], ^page)
		}
		if err := writeAt(file, chunk, offset); err != nil {
			return err
		}
		offset += int64(len(chunk))
		pages = pages[count:]
	}
	return nil
}

// flush appends what was noted since the last flush and syncs the log.
// It runs before the database file is synced.
func (l *dirtyLog) flush() error {
	l.mu.Lock()
	defer l.mu.Unlock()
	if len(l.pending) == 0 {
		return nil
	}
	if l.file == nil {
		// A host that skipped Prepare: the log starts at sequence zero,
		// which no marker continues from, and stays consistent.
		return l.rewriteLocked("", 0, l.pending)
	}
	if err := writeDirtyRecords(l.file, l.pending, l.size); err != nil {
		return fmt.Errorf("dirty log: %w", err)
	}
	if err := l.file.Sync(); err != nil {
		return fmt.Errorf("dirty log: %w", err)
	}
	l.size += int64(len(l.pending)) * dirtyLogRecord
	l.pending = l.pending[:0]
	return nil
}

// rewrite starts the log over with the pages still dirty at sequence seq
// of a generation, in the other file; the previous file stays until the
// next rewrite.
func (l *dirtyLog) rewrite(generation string, seq int64, pages []uint32) error {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.rewriteLocked(generation, seq, pages)
}

func (l *dirtyLog) rewriteLocked(generation string, seq int64, pages []uint32) error {
	if len(generation) > 65535 {
		return errors.New("generation id is too long for the dirty log")
	}
	next := 1 - l.current
	if l.file == nil {
		next = 0
	}
	file, err := l.files.Open(l.paths[next], true)
	if err != nil {
		return err
	}
	header := make([]byte, dirtyLogHeaderSize(generation))
	copy(header, dirtyLogMagic)
	binary.BigEndian.PutUint64(header[len(dirtyLogMagic):], uint64(seq))
	binary.BigEndian.PutUint64(header[len(dirtyLogMagic)+8:], ^uint64(seq))
	binary.BigEndian.PutUint16(header[len(dirtyLogMagic)+16:], uint16(len(generation)))
	copy(header[dirtyLogFixedHeader:], generation)
	copy(header[len(header)-dirtyLogSum:], dirtyLogChecksum(header))
	size := int64(len(header)) + int64(len(pages))*dirtyLogRecord
	fail := func(err error) error {
		file.Close()
		return fmt.Errorf("dirty log: %w", err)
	}
	// The old content goes first, so a torn header never fronts stale
	// records; then the records, then the header.
	if err := file.Truncate(0); err != nil {
		return fail(err)
	}
	if len(pages) > 0 {
		if err := writeDirtyRecords(file, pages, int64(len(header))); err != nil {
			return fail(err)
		}
		if err := file.Sync(); err != nil {
			return fail(err)
		}
	}
	// Once a header write is attempted, it may be readable even when the
	// write or sync reports failure. All later appends must use this file:
	// otherwise a restart could choose its newer sequence and miss pages
	// appended to the old log after that failure.
	if l.file != nil {
		l.file.Close()
	}
	l.file, l.current, l.size, l.broken = file, next, size, false
	l.noted = make(map[uint32]struct{}, len(pages))
	for _, page := range pages {
		if page == 0 {
			l.broken = true
		} else {
			l.noted[page] = struct{}{}
		}
	}
	l.pending = l.pending[:0]
	failActive := func(err error) error {
		// The next database sync must also persist the uncertainty. This
		// includes unplaceable writes whose caller could not append its
		// broken-log record after a failed rewrite.
		l.broken = true
		l.pending = append(l.pending, 0)
		return fmt.Errorf("dirty log: %w", err)
	}
	if err := writeAt(file, header, 0); err != nil {
		return failActive(err)
	}
	if err := file.Sync(); err != nil {
		return failActive(err)
	}
	return nil
}

func (l *dirtyLog) close() {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.file != nil {
		l.file.Close()
		l.file = nil
	}
}

// read names the pages the log holds for a marker: the file of that
// generation with the highest sequence not past the marker. A file at an
// older sequence is the one a crash between the marker write and the
// rewrite left, and names every page of the sync since, and more. A file
// of another generation is left over from before that generation started;
// a file that is there but does not read is not trusted, nor is one past
// the marker.
func (l *dirtyLog) read(generation string, markerSeq int64) ([]uint32, error) {
	var best []uint32
	bestSeq := int64(-1)
	for _, path := range l.paths {
		exists, err := l.files.Exists(path)
		if err != nil {
			return nil, err
		}
		if !exists {
			continue
		}
		fileGeneration, seq, pages, err := l.readFile(path)
		if err != nil {
			return nil, fmt.Errorf("%s: %w", path, err)
		}
		if fileGeneration != generation {
			continue
		}
		if seq > markerSeq {
			return nil, fmt.Errorf("%s is at sequence %d, past the marker at %d", path, seq, markerSeq)
		}
		if seq > bestSeq {
			best, bestSeq = pages, seq
		}
	}
	if bestSeq < 0 {
		return nil, errors.New("no dirty log of this generation next to the database")
	}
	return best, nil
}

func (l *dirtyLog) readFile(path string) (string, int64, []uint32, error) {
	file, err := l.files.Open(path, false)
	if err != nil {
		return "", 0, nil, err
	}
	defer file.Close()
	size, err := file.Size()
	if err != nil {
		return "", 0, nil, err
	}
	if size < int64(dirtyLogFixedHeader) || size > 1<<30 {
		return "", 0, nil, errors.New("dirty log has no complete header")
	}
	var fixed [dirtyLogFixedHeader]byte
	if err := readDirtyLogAt(file, fixed[:], 0); err != nil {
		return "", 0, nil, err
	}
	if string(fixed[:len(dirtyLogMagic)]) != dirtyLogMagic {
		return "", 0, nil, errors.New("not a dirty log")
	}
	seq := int64(binary.BigEndian.Uint64(fixed[len(dirtyLogMagic):]))
	if seq < 0 || binary.BigEndian.Uint64(fixed[len(dirtyLogMagic)+8:]) != ^uint64(seq) {
		return "", 0, nil, errors.New("dirty log header is damaged")
	}
	generationLength := int(binary.BigEndian.Uint16(fixed[len(dirtyLogMagic)+16:]))
	headerSize := dirtyLogFixedHeader + generationLength + dirtyLogSum
	if size < int64(headerSize) {
		return "", 0, nil, errors.New("dirty log has no complete header")
	}
	header := make([]byte, headerSize)
	copy(header, fixed[:])
	if err := readDirtyLogAt(file, header[dirtyLogFixedHeader:], int64(dirtyLogFixedHeader)); err != nil {
		return "", 0, nil, err
	}
	if !bytes.Equal(header[headerSize-dirtyLogSum:], dirtyLogChecksum(header)) {
		return "", 0, nil, errors.New("dirty log header is damaged")
	}
	generation := string(header[dirtyLogFixedHeader : dirtyLogFixedHeader+generationLength])
	// A crash mid-append leaves a torn last record.
	pages := make([]uint32, (size-int64(headerSize))/dirtyLogRecord)
	data := make([]byte, min(len(pages), dirtyLogChunkBytes/dirtyLogRecord)*dirtyLogRecord)
	for first := 0; first < len(pages); {
		count := min(len(pages)-first, len(data)/dirtyLogRecord)
		chunk := data[:count*dirtyLogRecord]
		if err := readDirtyLogAt(file, chunk, int64(headerSize)+int64(first)*dirtyLogRecord); err != nil {
			return "", 0, nil, err
		}
		for i := 0; i < count; i++ {
			record := chunk[i*dirtyLogRecord:]
			page := binary.BigEndian.Uint32(record[:4])
			if binary.BigEndian.Uint32(record[4:]) != ^page {
				return "", 0, nil, errors.New("dirty log record is damaged")
			}
			if page == 0 {
				return "", 0, nil, errors.New("dirty log names a write it could not place")
			}
			pages[first+i] = page
		}
		first += count
	}
	return generation, seq, pages, nil
}

func readDirtyLogAt(file File, data []byte, offset int64) error {
	for len(data) > 0 {
		chunk := data[:min(len(data), dirtyLogChunkBytes)]
		n, err := file.ReadAt(chunk, offset)
		if err != nil && !errors.Is(err, io.EOF) {
			return err
		}
		if n != len(chunk) {
			return io.ErrUnexpectedEOF
		}
		offset += int64(len(chunk))
		data = data[len(chunk):]
	}
	return nil
}
