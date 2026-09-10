package replica

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"time"
)

// The index remembers, per page, a hash of what the bucket holds. A start
// after an unclean stop hashes the database, compares, and ships only the
// pages that differ, so the generation continues where it was instead of
// starting over with everything. The index is written after every
// committed sync; a missing or torn index costs a full snapshot, never a
// missed page. The index carries the sequence of the sync it describes:
// one that does not match the marker is stale (its write failed after a
// commit) and is refused, since a page that meanwhile returned to its old
// bytes would otherwise never ship. On disk: magic, page size, sequence,
// page count, the hashes, and a checksum over all of it.

const indexMagic = "SPINIDX2"

type pageHash [8]byte

func hashPage(data []byte) pageHash {
	sum := sha256.Sum256(data)
	var hash pageHash
	copy(hash[:], sum[:len(hash)])
	return hash
}

func (s segment) hashes() []pageHash {
	hashes := make([]pageHash, len(s.Data))
	for index, data := range s.Data {
		hashes[index] = hashPage(data)
	}
	return hashes
}

type pageIndex struct {
	pageSize int
	// seq is the marker sequence the hashes belong to.
	seq int64
	// hashes[i] belongs to page i+1.
	hashes []pageHash
}

// apply records shipped pages; the index follows the database size.
func (x *pageIndex) apply(pageSize int, size int64, pages []uint32, hashes []pageHash) {
	x.pageSize = pageSize
	count := int(size / int64(pageSize))
	if len(x.hashes) > count {
		x.hashes = x.hashes[:count]
	}
	for len(x.hashes) < count {
		x.hashes = append(x.hashes, pageHash{})
	}
	for index, page := range pages {
		if page > 0 && int(page) <= count && index < len(hashes) {
			x.hashes[page-1] = hashes[index]
		}
	}
}

func encodeIndex(x pageIndex) []byte {
	var buffer bytes.Buffer
	buffer.WriteString(indexMagic)
	_ = binary.Write(&buffer, binary.BigEndian, uint32(x.pageSize))
	_ = binary.Write(&buffer, binary.BigEndian, uint64(x.seq))
	_ = binary.Write(&buffer, binary.BigEndian, uint32(len(x.hashes)))
	for _, hash := range x.hashes {
		buffer.Write(hash[:])
	}
	sum := sha256.Sum256(buffer.Bytes())
	buffer.Write(sum[:])
	return buffer.Bytes()
}

func decodeIndex(data []byte) (pageIndex, error) {
	const head = len(indexMagic) + 16
	if len(data) < head+sha256.Size || string(data[:len(indexMagic)]) != indexMagic {
		return pageIndex{}, errors.New("not a page index")
	}
	body, sum := data[:len(data)-sha256.Size], data[len(data)-sha256.Size:]
	if expected := sha256.Sum256(body); !bytes.Equal(expected[:], sum) {
		return pageIndex{}, errors.New("page index checksum does not match")
	}
	pageSize := int(binary.BigEndian.Uint32(body[len(indexMagic):]))
	seq := int64(binary.BigEndian.Uint64(body[len(indexMagic)+4:]))
	count := int(binary.BigEndian.Uint32(body[len(indexMagic)+12:]))
	if pageSize < 512 || pageSize > 65536 || pageSize&(pageSize-1) != 0 || seq < 0 || len(body) != head+count*len(pageHash{}) {
		return pageIndex{}, errors.New("page index is malformed")
	}
	x := pageIndex{pageSize: pageSize, seq: seq, hashes: make([]pageHash, count)}
	for index := range x.hashes {
		copy(x.hashes[index][:], body[head+index*len(pageHash{}):])
	}
	return x, nil
}

func (r *Replica) indexPath() string { return r.path + ".replica-index" }

// writeIndex puts the index on disk; when that fails, whatever is there
// is removed, so a start never trusts an index older than the marker.
func (r *Replica) writeIndex() error {
	r.indexMu.Lock()
	data := encodeIndex(r.index)
	r.indexMu.Unlock()
	if err := r.writeLocal(r.indexPath(), data); err != nil {
		_ = r.files.Remove(r.indexPath())
		return err
	}
	return nil
}

// usableIndex is the index on disk when it describes the marker's sync.
func (r *Replica) usableIndex(current marker) (pageIndex, error) {
	index, err := r.readIndex()
	if err != nil {
		return pageIndex{}, err
	}
	if index.pageSize != current.PageSize {
		return pageIndex{}, errors.New("page index is for another page size")
	}
	if index.seq != current.Seq {
		return pageIndex{}, fmt.Errorf("page index is at sequence %d, the marker at %d", index.seq, current.Seq)
	}
	return index, nil
}

func (r *Replica) readIndex() (pageIndex, error) {
	file, err := r.files.Open(r.indexPath(), false)
	if err != nil {
		return pageIndex{}, err
	}
	defer file.Close()
	size, err := file.Size()
	if err != nil {
		return pageIndex{}, err
	}
	if size > 1<<30 {
		return pageIndex{}, errors.New("page index is too large")
	}
	data := make([]byte, size)
	if _, err := file.ReadAt(data, 0); err != nil && !errors.Is(err, io.EOF) {
		return pageIndex{}, err
	}
	return decodeIndex(data)
}

// hashDatabase hashes every page of the database file.
func (r *Replica) hashDatabase(pageSize int) ([]pageHash, error) {
	file, err := r.files.Open(r.path, false)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	size, err := file.Size()
	if err != nil {
		return nil, err
	}
	if size%int64(pageSize) != 0 {
		return nil, errors.New("database size is not page aligned")
	}
	count := int(size / int64(pageSize))
	hashes := make([]pageHash, 0, count)
	run := make([]byte, max(pageSize, readRunBytes/pageSize*pageSize))
	started := time.Now()
	r.logger.Info("replica: indexing the database", "domain", r.domain, "bytes", size, "page_size", pageSize)
	defer func() {
		took := time.Since(started)
		r.logger.Info("replica: database indexed", "domain", r.domain, "bytes", size, "took", took.Round(time.Millisecond), "mib_per_second", float64(size)/(1<<20)/max(took.Seconds(), 0.001))
	}()
	for offset := int64(0); offset < size; offset += int64(len(run)) {
		if r.Progress != nil && offset%(256<<20) == 0 {
			r.Progress(fmt.Sprintf("Database indexeren: %d van %d MiB", offset>>20, size>>20))
		}
		chunk := run[:min(int64(len(run)), size-offset)]
		read, err := file.ReadAt(chunk, offset)
		if err != nil && !errors.Is(err, io.EOF) {
			return nil, err
		}
		if read != len(chunk) {
			return nil, io.ErrUnexpectedEOF
		}
		for start := 0; start < len(chunk); start += pageSize {
			hashes = append(hashes, hashPage(chunk[start:start+pageSize]))
		}
	}
	return hashes, nil
}

// rebuildIndex makes the index say the bucket holds the database as it is
// on disk: after a restore, or when the marker says every write shipped.
func (r *Replica) rebuildIndex(pageSize int, seq int64) error {
	hashes, err := r.hashDatabase(pageSize)
	if err != nil {
		return err
	}
	r.indexMu.Lock()
	r.index = pageIndex{pageSize: pageSize, seq: seq, hashes: hashes}
	r.indexMu.Unlock()
	return r.writeIndex()
}

// differingPages names the pages whose content is not what the index says
// the bucket holds, the pages the index does not know, and page 1 when the
// size changed (it carries the size).
func (r *Replica) differingPages(index pageIndex) ([]uint32, int, error) {
	hashes, err := r.hashDatabase(index.pageSize)
	if err != nil {
		return nil, 0, err
	}
	var pages []uint32
	for position, hash := range hashes {
		if position >= len(index.hashes) || index.hashes[position] != hash {
			pages = append(pages, uint32(position+1))
		}
	}
	if len(hashes) != len(index.hashes) && (len(pages) == 0 || pages[0] != 1) {
		pages = append([]uint32{1}, pages...)
	}
	return pages, len(hashes), nil
}
