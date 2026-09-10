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

	mu         sync.Mutex
	ready      bool
	current    int
	file       File
	size       int64
	seq        int64
	generation string
	noted      map[uint32]struct{}
	pending    []uint32
	broken     bool
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

func encodeDirtyRecords(pages []uint32) []byte {
	data := make([]byte, 0, len(pages)*dirtyLogRecord)
	var record [dirtyLogRecord]byte
	for _, page := range pages {
		binary.BigEndian.PutUint32(record[:4], page)
		binary.BigEndian.PutUint32(record[4:], ^page)
		data = append(data, record[:]...)
	}
	return data
}

// flush appends what was noted since the last flush and syncs the log.
// It runs before the database file is synced.
func (l *dirtyLog) flush() error {
	l.mu.Lock()
	defer l.mu.Unlock()
	if len(l.pending) == 0 {
		return nil
	}
	if !l.ready || l.file == nil {
		// A host that skipped Prepare: the log starts at sequence zero,
		// which no marker continues from, and stays consistent.
		pending := append([]uint32(nil), l.pending...)
		if err := l.rewriteLocked("", 0, nil); err != nil {
			return err
		}
		l.pending = pending
	}
	data := encodeDirtyRecords(l.pending)
	if err := writeAt(l.file, data, l.size); err != nil {
		return fmt.Errorf("dirty log: %w", err)
	}
	if err := l.file.Sync(); err != nil {
		return fmt.Errorf("dirty log: %w", err)
	}
	l.size += int64(len(data))
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
	if !l.ready {
		next = 0
	}
	file, err := l.files.Open(l.paths[next], true)
	if err != nil {
		return err
	}
	records := encodeDirtyRecords(pages)
	header := make([]byte, dirtyLogHeaderSize(generation))
	copy(header, dirtyLogMagic)
	binary.BigEndian.PutUint64(header[len(dirtyLogMagic):], uint64(seq))
	binary.BigEndian.PutUint64(header[len(dirtyLogMagic)+8:], ^uint64(seq))
	binary.BigEndian.PutUint16(header[len(dirtyLogMagic)+16:], uint16(len(generation)))
	copy(header[dirtyLogFixedHeader:], generation)
	copy(header[len(header)-dirtyLogSum:], dirtyLogChecksum(header))
	size := int64(len(header) + len(records))
	fail := func(err error) error {
		file.Close()
		return fmt.Errorf("dirty log: %w", err)
	}
	// The old content goes first, so a torn header never fronts stale
	// records; then the records, then the header.
	if err := file.Truncate(0); err != nil {
		return fail(err)
	}
	if len(records) > 0 {
		if err := writeAt(file, records, int64(len(header))); err != nil {
			return fail(err)
		}
		if err := file.Sync(); err != nil {
			return fail(err)
		}
	}
	if err := writeAt(file, header, 0); err != nil {
		return fail(err)
	}
	if err := file.Sync(); err != nil {
		return fail(err)
	}
	if l.file != nil {
		l.file.Close()
	}
	l.file, l.current, l.size, l.seq, l.generation, l.ready, l.broken = file, next, size, seq, generation, true, false
	l.noted = make(map[uint32]struct{}, len(pages))
	for _, page := range pages {
		l.noted[page] = struct{}{}
	}
	l.pending = l.pending[:0]
	return nil
}

func (l *dirtyLog) close() {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.file != nil {
		l.file.Close()
		l.file = nil
	}
	l.ready = false
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
	data := make([]byte, size)
	if _, err := file.ReadAt(data, 0); err != nil && !errors.Is(err, io.EOF) {
		return "", 0, nil, err
	}
	if string(data[:len(dirtyLogMagic)]) != dirtyLogMagic {
		return "", 0, nil, errors.New("not a dirty log")
	}
	seq := int64(binary.BigEndian.Uint64(data[len(dirtyLogMagic):]))
	if seq < 0 || binary.BigEndian.Uint64(data[len(dirtyLogMagic)+8:]) != ^uint64(seq) {
		return "", 0, nil, errors.New("dirty log header is damaged")
	}
	generationLength := int(binary.BigEndian.Uint16(data[len(dirtyLogMagic)+16:]))
	headerSize := dirtyLogFixedHeader + generationLength + dirtyLogSum
	if len(data) < headerSize {
		return "", 0, nil, errors.New("dirty log has no complete header")
	}
	header := data[:headerSize]
	if !bytes.Equal(header[headerSize-dirtyLogSum:], dirtyLogChecksum(header)) {
		return "", 0, nil, errors.New("dirty log header is damaged")
	}
	generation := string(header[dirtyLogFixedHeader : dirtyLogFixedHeader+generationLength])
	body := data[headerSize:]
	// A crash mid-append leaves a torn last record.
	body = body[:len(body)/dirtyLogRecord*dirtyLogRecord]
	pages := make([]uint32, 0, len(body)/dirtyLogRecord)
	for offset := 0; offset < len(body); offset += dirtyLogRecord {
		page := binary.BigEndian.Uint32(body[offset:])
		if binary.BigEndian.Uint32(body[offset+4:]) != ^page {
			return "", 0, nil, errors.New("dirty log record is damaged")
		}
		if page == 0 {
			return "", 0, nil, errors.New("dirty log names a write it could not place")
		}
		pages = append(pages, page)
	}
	return generation, seq, pages, nil
}
