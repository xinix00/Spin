package replica

import (
	"errors"
	"io"
	"io/fs"
	"os"

	"github.com/ncruces/go-sqlite3/vfs"
)

// storage is how the replica itself reaches files next to SQLite: the
// database to read pages from or rebuild, and its marker. On an ordinary
// system that is the OS; on HopOS it is the volume VFS.
type storage interface {
	Open(name string, create bool) (storageFile, error)
	Exists(name string) (bool, error)
	Remove(name string) error
}

type storageFile interface {
	io.ReaderAt
	io.WriterAt
	Truncate(size int64) error
	Sync() error
	Size() (int64, error)
	Close() error
}

// storageFor picks the storage behind a VFS: the OS VFS opens files only
// through SQLite's filename type, so files are reached through os there.
func storageFor(inner vfs.VFS) storage {
	if _, byFilename := inner.(vfs.VFSFilename); byFilename {
		return osStorage{}
	}
	return vfsStorage{inner: inner}
}

type osStorage struct{}

type osFile struct{ *os.File }

func (f osFile) Sync() error { return f.File.Sync() }
func (f osFile) Size() (int64, error) {
	info, err := f.File.Stat()
	if err != nil {
		return 0, err
	}
	return info.Size(), nil
}

func (osStorage) Open(name string, create bool) (storageFile, error) {
	flags := os.O_RDWR
	if create {
		flags |= os.O_CREATE
	}
	file, err := os.OpenFile(name, flags, 0o600)
	if err != nil {
		return nil, err
	}
	return osFile{file}, nil
}

func (osStorage) Exists(name string) (bool, error) {
	_, err := os.Stat(name)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	return err == nil, err
}

func (osStorage) Remove(name string) error { return os.Remove(name) }

type vfsStorage struct{ inner vfs.VFS }

type vfsFile struct{ vfs.File }

func (f vfsFile) Sync() error { return f.File.Sync(vfs.SYNC_FULL) }

func (s vfsStorage) Open(name string, create bool) (storageFile, error) {
	flags := vfs.OPEN_READWRITE | vfs.OPEN_MAIN_DB
	if create {
		flags |= vfs.OPEN_CREATE
	}
	file, _, err := s.inner.Open(name, flags)
	if err != nil {
		return nil, err
	}
	return vfsFile{file}, nil
}

func (s vfsStorage) Exists(name string) (bool, error) {
	return s.inner.Access(name, vfs.ACCESS_EXISTS)
}

func (s vfsStorage) Remove(name string) error { return s.inner.Delete(name, false) }
