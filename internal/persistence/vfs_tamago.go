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
	"github.com/xinix00/HopOS/metal/abi/hopabi"
	"github.com/xinix00/HopOS/metal/app/applib"
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
	return &hopFile{app: v.app, path: name, deleteOnClose: flags&vfs.OPEN_DELETEONCLOSE != 0}, flags, nil
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

type hopFile struct {
	app           *applib.App
	path          string
	deleteOnClose bool
	lock          vfs.LockLevel
}

func (f *hopFile) Close() error {
	if f.deleteOnClose {
		return f.app.Remove(f.path)
	}
	return nil
}

func (f *hopFile) ReadAt(target []byte, offset int64) (int, error) {
	if offset < 0 {
		return 0, errors.New("negative SQLite read offset")
	}
	total := 0
	for total < len(target) {
		count := len(target) - total
		if count > hopabi.MaxChunk {
			count = hopabi.MaxChunk
		}
		chunk, err := f.app.ReadAt(f.path, uint64(offset)+uint64(total), count)
		copy(target[total:], chunk)
		total += len(chunk)
		if err != nil {
			return total, err
		}
		if len(chunk) < count {
			return total, io.EOF
		}
	}
	return total, nil
}

func (f *hopFile) WriteAt(source []byte, offset int64) (int, error) {
	if offset < 0 {
		return 0, errors.New("negative SQLite write offset")
	}
	total := 0
	for total < len(source) {
		count := len(source) - total
		if count > hopabi.MaxChunk {
			count = hopabi.MaxChunk
		}
		written, err := f.app.WriteAt(f.path, uint64(offset)+uint64(total), source[total:total+count])
		total += written
		if err != nil {
			return total, err
		}
		if written != count {
			return total, io.ErrShortWrite
		}
	}
	return total, nil
}

func (f *hopFile) Truncate(size int64) error {
	if size < 0 {
		return errors.New("negative SQLite truncate size")
	}
	return f.app.Truncate(f.path, uint64(size))
}

func (*hopFile) Sync(vfs.SyncFlag) error { return nil }

func (f *hopFile) Size() (int64, error) {
	size, err := f.app.Stat(f.path)
	return int64(size), err
}

func (f *hopFile) Lock(lock vfs.LockLevel) error {
	f.lock = lock
	return nil
}

func (f *hopFile) Unlock(lock vfs.LockLevel) error {
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
