package replica

import (
	"encoding/binary"
	"sort"
	"sync"

	"github.com/ncruces/go-sqlite3/vfs"
)

// The tracker sits between SQLite and the storage as a VFS and remembers
// which pages of the main database file were written. A sync pass takes
// those pages, reads them under a read transaction (so no write is half
// way) and ships them. Writes that arrive in between mark their pages again.
// The first write after a sync also flips the marker on storage to unclean,
// so a crash before the next sync is known at the next start; the dirty
// log names the pages of those writes, so that start continues instead of
// copying everything.

type tracker struct {
	mu       sync.Mutex
	pageSize int
	dirty    map[uint32]struct{}
	// pending holds byte ranges written before the page size was known.
	pending [][2]int64
	size    int64
	clean   bool
	// onUnclean runs under the lock when the first write after a sync
	// arrives; it records the unclean state on storage before the write.
	onUnclean func() error
	revision  uint64
	log       *dirtyLog
}

func newTracker(log *dirtyLog) *tracker {
	return &tracker{dirty: map[uint32]struct{}{}, clean: true, log: log}
}

func (t *tracker) learnHeader(header []byte) {
	if len(header) < 18 {
		return
	}
	size := int(binary.BigEndian.Uint16(header[16:18]))
	if size == 1 {
		size = 65536
	}
	if size < 512 || size > 65536 || size&(size-1) != 0 {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	t.pageSize = size
	for _, span := range t.pending {
		t.markLocked(span[0], span[1])
	}
	t.pending = nil
}

func (t *tracker) markLocked(offset, length int64) {
	if t.pageSize == 0 {
		t.pending = append(t.pending, [2]int64{offset, length})
		t.log.markBroken()
		return
	}
	first := offset / int64(t.pageSize)
	last := (offset + length - 1) / int64(t.pageSize)
	for page := first; page <= last; page++ {
		t.dirty[uint32(page+1)] = struct{}{}
		t.log.note(uint32(page + 1))
	}
	if end := offset + length; end > t.size {
		t.size = end
	}
}

// write records a write; the first one after a sync reports unclean first.
func (t *tracker) write(offset int64, data []byte) error {
	if len(data) == 0 {
		return nil
	}
	if len(data) >= 18 && offset == 0 {
		t.learnHeader(data)
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	if err := t.uncleanLocked(); err != nil {
		return err
	}
	t.markLocked(offset, int64(len(data)))
	return nil
}

// The callback must become durable before SQLite is allowed to change storage.
// Keeping the tracker lock also serializes this transition with settle.
func (t *tracker) uncleanLocked() error {
	if t.clean && t.onUnclean != nil {
		if err := t.onUnclean(); err != nil {
			return err
		}
	}
	t.clean = false
	t.revision++
	return nil
}

func (t *tracker) truncate(size int64) error {
	t.mu.Lock()
	defer t.mu.Unlock()
	if err := t.uncleanLocked(); err != nil {
		return err
	}
	t.size = size
	// A size-only change must be shipped too. Page 1 carries SQLite's size.
	t.dirty[1] = struct{}{}
	t.log.note(1)
	return nil
}

// rewriteLog starts the dirty log over with what is still dirty, after a
// sync committed at seq.
func (t *tracker) rewriteLog(generation string, seq int64) error {
	t.mu.Lock()
	defer t.mu.Unlock()
	pages := make([]uint32, 0, len(t.dirty))
	for page := range t.dirty {
		pages = append(pages, page)
	}
	sort.Slice(pages, func(i, j int) bool { return pages[i] < pages[j] })
	if len(t.pending) > 0 {
		// Writes the tracker could not place yet stay unplaceable.
		if err := t.log.rewrite(generation, seq, pages); err != nil {
			return err
		}
		t.log.markBroken()
		return t.log.flush()
	}
	return t.log.rewrite(generation, seq, pages)
}

// markAll marks every page of a database of the given size: the start of a
// generation, or a database whose replica cannot be trusted.
func (t *tracker) markAll(size int64) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.pageSize == 0 {
		return
	}
	t.size = size
	pages := (size + int64(t.pageSize) - 1) / int64(t.pageSize)
	for page := int64(1); page <= pages; page++ {
		t.dirty[uint32(page)] = struct{}{}
	}
	t.clean = false
}

// take removes up to limit dirty pages, lowest first.
func (t *tracker) take(limit int) []uint32 {
	t.mu.Lock()
	defer t.mu.Unlock()
	pages := make([]uint32, 0, min(limit, len(t.dirty)))
	for page := range t.dirty {
		pages = append(pages, page)
	}
	sort.Slice(pages, func(i, j int) bool { return pages[i] < pages[j] })
	if len(pages) > limit {
		pages = pages[:limit]
	}
	for _, page := range pages {
		delete(t.dirty, page)
	}
	return pages
}

// markPages marks the pages the dirty log named at a start after an
// unclean stop; the tracker is unclean from then on.
func (t *tracker) markPages(pages []uint32) {
	t.mu.Lock()
	defer t.mu.Unlock()
	for _, page := range pages {
		t.dirty[page] = struct{}{}
	}
	if len(pages) > 0 {
		t.clean = false
	}
}

func (t *tracker) putBack(pages []uint32) {
	t.mu.Lock()
	defer t.mu.Unlock()
	for _, page := range pages {
		t.dirty[page] = struct{}{}
	}
}

func (t *tracker) pendingPages() int {
	t.mu.Lock()
	defer t.mu.Unlock()
	return len(t.dirty)
}

// settle persists clean while excluding new writes, and only if capture
// still covers the current revision.
func (t *tracker) settle(revision uint64, persist func() error) error {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.revision != revision || len(t.dirty) > 0 || len(t.pending) > 0 {
		return nil
	}
	if err := persist(); err != nil {
		// A failed fsync may still have written Clean=true. Force the next write
		// through onUnclean even when its durability acknowledgment was lost.
		t.clean = true
		return err
	}
	t.clean = true
	return nil
}

func (t *tracker) version() uint64 {
	t.mu.Lock()
	defer t.mu.Unlock()
	return t.revision
}

func (t *tracker) currentPageSize() int {
	t.mu.Lock()
	defer t.mu.Unlock()
	return t.pageSize
}

// trackingVFS wraps the storage VFS and watches the main database file.
type trackingVFS struct {
	inner   vfs.VFS
	main    string
	tracker *tracker
}

// fullPath resolves a name the way the storage VFS does. The OS VFS
// reports a resolved symlink as an error code next to a valid path, so the
// path counts whenever there is one.
func fullPath(inner vfs.VFS, name string) string {
	full, _ := inner.FullPathname(name)
	if full == "" {
		return name
	}
	return full
}

func (v *trackingVFS) isMain(name string) bool {
	return name == v.main || fullPath(v.inner, name) == v.main
}

func (v *trackingVFS) Open(name string, flags vfs.OpenFlag) (vfs.File, vfs.OpenFlag, error) {
	file, flags, err := v.inner.Open(name, flags)
	return v.wrap(name, file, flags, err)
}

// OpenFilename is the entry SQLite uses for a VFS that understands its
// filename type; the OS VFS opens files only this way.
func (v *trackingVFS) OpenFilename(name *vfs.Filename, flags vfs.OpenFlag) (vfs.File, vfs.OpenFlag, error) {
	plain := ""
	if name != nil {
		plain = name.String()
	}
	if byFilename, ok := v.inner.(vfs.VFSFilename); ok {
		file, flags, err := byFilename.OpenFilename(name, flags)
		return v.wrap(plain, file, flags, err)
	}
	return v.Open(plain, flags)
}

func (v *trackingVFS) wrap(name string, file vfs.File, flags vfs.OpenFlag, err error) (vfs.File, vfs.OpenFlag, error) {
	if err != nil || name == "" || !v.isMain(name) {
		return file, flags, err
	}
	var header [100]byte
	if count, readErr := file.ReadAt(header[:], 0); readErr == nil && count == len(header) {
		v.tracker.learnHeader(header[:])
	}
	return &trackedFile{File: file, tracker: v.tracker}, flags, nil
}

func (v *trackingVFS) Delete(name string, syncDir bool) error { return v.inner.Delete(name, syncDir) }
func (v *trackingVFS) Access(name string, flags vfs.AccessFlag) (bool, error) {
	return v.inner.Access(name, flags)
}
func (v *trackingVFS) FullPathname(name string) (string, error) { return v.inner.FullPathname(name) }

type trackedFile struct {
	vfs.File
	tracker *tracker
}

func (f *trackedFile) WriteAt(data []byte, offset int64) (int, error) {
	if err := f.tracker.write(offset, data); err != nil {
		return 0, err
	}
	return f.File.WriteAt(data, offset)
}

func (f *trackedFile) Truncate(size int64) error {
	if err := f.tracker.truncate(size); err != nil {
		return err
	}
	return f.File.Truncate(size)
}

// Sync puts the dirty log on disk before the pages it names.
func (f *trackedFile) Sync(flags vfs.SyncFlag) error {
	if err := f.tracker.log.flush(); err != nil {
		return err
	}
	return f.File.Sync(flags)
}

// Unlock ends a transaction; what it wrote without a sync is logged now.
func (f *trackedFile) Unlock(lock vfs.LockLevel) error {
	if err := f.tracker.log.flush(); err != nil {
		return err
	}
	return f.File.Unlock(lock)
}

// DeviceCharacteristics drops capabilities the wrapper does not forward as
// optional interfaces, so SQLite never relies on one the file lacks.
func (f *trackedFile) DeviceCharacteristics() vfs.DeviceCharacteristic {
	return f.File.DeviceCharacteristics() &^ (vfs.IOCAP_BATCH_ATOMIC | vfs.IOCAP_IMMUTABLE)
}
