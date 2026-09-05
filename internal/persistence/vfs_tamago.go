//go:build tamago

package persistence

import (
	"errors"
	"io"
	"io/fs"
	"path"
	"strings"
	"sync/atomic"

	"github.com/ncruces/go-sqlite3/vfs"
	"github.com/xinix00/HopOS/metal/v2/app/applib"
)

const hopVFSName = "spin-hop"

// RegisterHopVFS connects SQLite's random-access VFS to HopOS' volume ABI.
// The ABI operations are synchronous, so Sync is a deliberate no-op.
func RegisterHopVFS(app *applib.App) string {
	physicalHopApp = app
	vfs.Register(hopVFSName, &hopVFS{app: app})
	return hopVFSName
}

type hopVFS struct {
	app  *applib.App
	temp atomic.Uint64
}

func (v *hopVFS) Open(name string, flags vfs.OpenFlag) (vfs.File, vfs.OpenFlag, error) {
	if name == "" {
		name = "/data/.spin-sqlite-temp-" + decimal(v.temp.Add(1))
		flags |= vfs.OPEN_DELETEONCLOSE
	}
	name, err := cleanHopPath(name)
	if err != nil {
		return nil, flags, err
	}
	_, statErr := v.app.Stat(name)
	if statErr != nil {
		if flags&vfs.OPEN_CREATE == 0 {
			return nil, flags, statErr
		}
		if err := v.app.Truncate(name, 0); err != nil {
			return nil, flags, err
		}
	}
	file := &hopFile{app: v.app, path: name, deleteOnClose: flags&vfs.OPEN_DELETEONCLOSE != 0}
	file.writes = newWriteCoalescer(applib.MaxIOChunk, func(offset int64, data []byte) error {
		_, err := v.app.WriteAt(name, uint64(offset), data)
		return err
	})
	return file, flags, nil
}

func (v *hopVFS) Delete(name string, _ bool) error {
	name, err := cleanHopPath(name)
	if err != nil {
		return err
	}
	return v.app.Remove(name)
}

func (v *hopVFS) Access(name string, _ vfs.AccessFlag) (bool, error) {
	name, err := cleanHopPath(name)
	if err != nil {
		return false, err
	}
	_, err = v.app.Stat(name)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	return err == nil, err
}

func (*hopVFS) FullPathname(name string) (string, error) { return cleanHopPath(name) }

// hopFile is one SQLite file on the HopOS volume. Page writes are coalesced
// into runs of up to one ABI chunk and reach storage at Sync, at unlock, before
// a read or size query that would see them, and at Close; the volume ABI moves
// a megabyte per call at full speed, one page per call did not.
type hopFile struct {
	app           *applib.App
	path          string
	deleteOnClose bool
	lock          vfs.LockLevel
	writes        *writeCoalescer
}

func (f *hopFile) Close() error {
	flushErr := f.writes.Flush()
	if f.deleteOnClose {
		return errors.Join(flushErr, f.app.Remove(f.path))
	}
	return flushErr
}

func (f *hopFile) ReadAt(target []byte, offset int64) (int, error) {
	if offset < 0 {
		return 0, errors.New("negative SQLite read offset")
	}
	if f.writes.Overlaps(offset, int64(len(target))) {
		if err := f.writes.Flush(); err != nil {
			return 0, err
		}
	}
	total := 0
	for total < len(target) {
		count := len(target) - total
		if count > applib.MaxIOChunk {
			count = applib.MaxIOChunk
		}
		read, err := f.app.ReadInto(f.path, uint64(offset)+uint64(total), target[total:total+count])
		total += read
		if err != nil {
			return total, err
		}
		if read < count {
			return total, io.EOF
		}
	}
	return total, nil
}

func (f *hopFile) WriteAt(source []byte, offset int64) (int, error) {
	if offset < 0 {
		return 0, errors.New("negative SQLite write offset")
	}
	if err := f.writes.Write(offset, source); err != nil {
		return 0, err
	}
	return len(source), nil
}

func (f *hopFile) Truncate(size int64) error {
	if size < 0 {
		return errors.New("negative SQLite truncate size")
	}
	f.writes.Discard(size)
	if err := f.writes.Flush(); err != nil {
		return err
	}
	return f.app.Truncate(f.path, uint64(size))
}

// Sync is the durability boundary SQLite relies on: pending writes reach the
// volume here. The ABI itself has no flush; a completed write is on the volume.
func (f *hopFile) Sync(vfs.SyncFlag) error { return f.writes.Flush() }

func (f *hopFile) Size() (int64, error) {
	if err := f.writes.Flush(); err != nil {
		return 0, err
	}
	size, err := f.app.Stat(f.path)
	return int64(size), err
}

func (f *hopFile) Lock(lock vfs.LockLevel) error {
	f.lock = lock
	return nil
}

// Unlock hands the file to whoever locks next; nothing may still be pending.
func (f *hopFile) Unlock(lock vfs.LockLevel) error {
	if err := f.writes.Flush(); err != nil {
		return err
	}
	f.lock = lock
	return nil
}

func (f *hopFile) CheckReservedLock() (bool, error) {
	return f.lock >= vfs.LOCK_RESERVED, nil
}

func (*hopFile) SectorSize() int { return 4096 }

func (*hopFile) DeviceCharacteristics() vfs.DeviceCharacteristic { return 0 }

func cleanHopPath(name string) (string, error) {
	name = path.Clean("/" + strings.TrimSpace(name))
	if name != "/data" && !strings.HasPrefix(name, "/data/") {
		return "", errors.New("SQLite path must stay inside /data")
	}
	return name, nil
}

func decimal(value uint64) string {
	if value == 0 {
		return "0"
	}
	var buffer [20]byte
	position := len(buffer)
	for value > 0 {
		position--
		buffer[position] = byte('0' + value%10)
		value /= 10
	}
	return string(buffer[position:])
}
