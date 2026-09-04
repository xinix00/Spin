package persistence

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/url"
	"sort"
	"strings"
	"sync"

	sqliteDriver "github.com/ncruces/go-sqlite3/driver"
)

const (
	backupFormatKey = "backup/format"
	backupKeyKey    = "backup/master_key"
	backupFormat    = "spin-sqlite-backup-v1"
)

var ErrBackupUploadOffset = errors.New("backup upload offset mismatch")

type StagedBackup struct {
	Path      string
	Database  *SQLite
	MasterKey string
	remove    func(string) error
}

// backupUploadWindow bounds how far ahead of the committed prefix a chunk may
// land. Parallel uploaders keep a handful of chunks in flight; anything further
// ahead is a client bug rather than reordering, and the bound keeps the sparse
// gap in the staging file small.
const backupUploadWindow int64 = 64 << 20

// BackupUpload incrementally assembles one portable database without ever
// requiring a request body larger than the surrounding HTTP transport allows.
// Chunks may arrive concurrently and out of order: every write lands at its own
// offset while the upload tracks the contiguous committed prefix, the first
// byte that did not reach physical storage yet, so an interrupted client can
// resume there.
type BackupUpload struct {
	mu       sync.Mutex
	path     string
	vfs      string
	maxBytes int64
	offset   int64        // contiguous committed prefix
	ahead    []uploadSpan // completed chunks beyond offset, sorted by start
	pending  int          // writes in flight
}

type uploadSpan struct{ start, end int64 }

func (s *SQLite) BeginBackupUpload(maxBytes int64) (*BackupUpload, error) {
	if maxBytes <= 0 {
		return nil, errors.New("backup upload limit must be positive")
	}
	path := s.temporaryPath("restore-upload")
	if err := createPhysicalFile(path); err != nil {
		return nil, err
	}
	return &BackupUpload{path: path, vfs: s.vfs, maxBytes: maxBytes}, nil
}

// Offset reports the committed prefix.
func (u *BackupUpload) Offset() int64 {
	u.mu.Lock()
	defer u.mu.Unlock()
	return u.offset
}

// WriteAt stores one chunk at offset and reports the committed prefix. A chunk
// that lies entirely inside the prefix is acknowledged without rewriting it, so
// retries stay idempotent. A chunk that straddles the prefix boundary or lands
// beyond the reorder window reports ErrBackupUploadOffset with the prefix. The
// physical write runs outside the lock so several chunks can stream at once.
func (u *BackupUpload) WriteAt(ctx context.Context, offset, length int64, source io.Reader) (int64, error) {
	u.mu.Lock()
	if u.path == "" {
		u.mu.Unlock()
		return u.offset, errors.New("backup upload is closed")
	}
	if length <= 0 {
		u.mu.Unlock()
		return u.offset, errors.New("backup upload chunk is empty")
	}
	if offset < 0 || length > u.maxBytes-offset {
		u.mu.Unlock()
		return u.offset, errors.New("backup exceeds upload limit")
	}
	if offset+length <= u.offset {
		committed := u.offset
		u.mu.Unlock()
		return committed, nil
	}
	if offset < u.offset || offset > u.offset+backupUploadWindow {
		committed := u.offset
		u.mu.Unlock()
		return committed, fmt.Errorf("%w: received %d, committed %d", ErrBackupUploadOffset, offset, committed)
	}
	path := u.path
	u.pending++
	u.mu.Unlock()

	written, err := appendPhysicalFile(ctx, path, io.LimitReader(source, length), offset, length)
	if err == nil && written != length {
		err = io.ErrUnexpectedEOF
	}

	u.mu.Lock()
	defer u.mu.Unlock()
	u.pending--
	if u.path == "" {
		return u.offset, errors.New("backup upload is closed")
	}
	if err != nil {
		return u.offset, err
	}
	u.commit(uploadSpan{start: offset, end: offset + length})
	return u.offset, nil
}

func (u *BackupUpload) commit(span uploadSpan) {
	index := sort.Search(len(u.ahead), func(i int) bool { return u.ahead[i].start >= span.start })
	u.ahead = append(u.ahead, uploadSpan{})
	copy(u.ahead[index+1:], u.ahead[index:])
	u.ahead[index] = span
	consumed := 0
	for consumed < len(u.ahead) && u.ahead[consumed].start <= u.offset {
		if u.ahead[consumed].end > u.offset {
			u.offset = u.ahead[consumed].end
		}
		consumed++
	}
	u.ahead = append(u.ahead[:0], u.ahead[consumed:]...)
}

func (u *BackupUpload) Stage(expectedSize int64) (*StagedBackup, error) {
	u.mu.Lock()
	defer u.mu.Unlock()
	if u.path == "" {
		return nil, errors.New("backup upload is closed")
	}
	if u.pending > 0 {
		return nil, errors.New("backup upload still has chunks in flight")
	}
	if u.offset != expectedSize {
		return nil, fmt.Errorf("backup upload is incomplete: received %d of %d bytes", u.offset, expectedSize)
	}
	backup, err := openStagedBackup(u.path, u.vfs)
	if err != nil {
		return nil, err
	}
	u.path = ""
	return backup, nil
}

func (u *BackupUpload) Close() error {
	u.mu.Lock()
	defer u.mu.Unlock()
	if u.path == "" {
		return nil
	}
	err := removePhysicalFile(u.path)
	u.path = ""
	return err
}

func (b *StagedBackup) Close() error {
	var closeErr error
	if b.Database != nil {
		closeErr = b.Database.Close()
		b.Database = nil
	}
	if b.remove != nil && b.Path != "" {
		closeErr = errors.Join(closeErr, b.remove(b.Path))
		b.Path = ""
	}
	return closeErr
}

func (s *SQLite) WriteBackup(ctx context.Context, destination io.Writer, masterKey string) error {
	backup, err := s.PrepareBackup(ctx, masterKey)
	if err != nil {
		return err
	}
	defer backup.Close()
	return backup.WriteTo(ctx, destination)
}

// PrepareBackup takes one consistent online SQLite snapshot and adds the
// portable key only to that copy. Callers can inspect every referenced object
// in the frozen copy before streaming it to a user.
func (s *SQLite) PrepareBackup(ctx context.Context, masterKey string) (*StagedBackup, error) {
	temporary := s.temporaryPath("backup")
	if err := s.backupTo(ctx, temporary); err != nil {
		_ = removePhysicalFile(temporary)
		return nil, err
	}
	backup, err := Open(temporary, OpenOptions{VFS: s.vfs})
	if err != nil {
		_ = removePhysicalFile(temporary)
		return nil, fmt.Errorf("open backup copy: %w", err)
	}
	if err := backup.WriteFile(backupFormatKey, []byte(backupFormat)); err == nil {
		err = backup.WriteFile(backupKeyKey, []byte(strings.TrimSpace(masterKey)))
	}
	if err != nil {
		_ = backup.Close()
		_ = removePhysicalFile(temporary)
		return nil, err
	}
	return &StagedBackup{Path: temporary, Database: backup, MasterKey: strings.TrimSpace(masterKey), remove: removePhysicalFile}, nil
}

func (b *StagedBackup) WriteTo(ctx context.Context, destination io.Writer) error {
	if b == nil || b.Path == "" {
		return errors.New("staged backup is closed")
	}
	if b.Database != nil {
		if err := b.Database.Close(); err != nil {
			return err
		}
		b.Database = nil
	}
	return readPhysicalFile(ctx, b.Path, destination)
}

func (s *SQLite) StageBackup(ctx context.Context, source io.Reader, maxBytes int64) (*StagedBackup, error) {
	temporary := s.temporaryPath("restore")
	if err := writePhysicalFile(ctx, temporary, source, maxBytes); err != nil {
		_ = removePhysicalFile(temporary)
		return nil, err
	}
	backup, err := openStagedBackup(temporary, s.vfs)
	if err != nil {
		_ = removePhysicalFile(temporary)
		return nil, err
	}
	return backup, nil
}

func openStagedBackup(path, vfs string) (*StagedBackup, error) {
	backup, err := Open(path, OpenOptions{VFS: vfs})
	if err != nil {
		return nil, fmt.Errorf("open uploaded backup: %w", err)
	}
	format, err := backup.ReadFile(backupFormatKey)
	if err != nil || string(format) != backupFormat {
		_ = backup.Close()
		return nil, errors.New("not a supported Spin database backup")
	}
	key, err := backup.ReadFile(backupKeyKey)
	if err != nil || strings.TrimSpace(string(key)) == "" {
		_ = backup.Close()
		return nil, errors.New("Spin backup has no master key")
	}
	return &StagedBackup{Path: path, Database: backup, MasterKey: strings.TrimSpace(string(key)), remove: removePhysicalFile}, nil
}

func (s *SQLite) RestoreFrom(ctx context.Context, backup *StagedBackup) error {
	if backup == nil || backup.Path == "" {
		return errors.New("staged backup is closed")
	}
	// Close the validating connection before SQLite opens the same source via
	// the online-backup API. The staged object remains removable by Close.
	if backup.Database != nil {
		if err := backup.Database.Close(); err != nil {
			return err
		}
		backup.Database = nil
	}
	connection, err := s.db.Conn(ctx)
	if err != nil {
		return err
	}
	err = connection.Raw(func(driverConnection any) error {
		conn, ok := driverConnection.(sqliteDriver.Conn)
		if !ok {
			return errors.New("unexpected SQLite driver connection")
		}
		return conn.Raw().Restore("main", physicalURI(backup.Path, s.vfs))
	})
	closeErr := connection.Close()
	if err != nil {
		return fmt.Errorf("restore SQLite database: %w", errors.Join(err, closeErr))
	}
	if closeErr != nil {
		return closeErr
	}
	if err := s.DeleteFile(backupKeyKey); err != nil {
		return err
	}
	return s.DeleteFile(backupFormatKey)
}

func (s *SQLite) RollbackPoint(ctx context.Context) (*StagedBackup, error) {
	temporary := s.temporaryPath("rollback")
	if err := s.backupTo(ctx, temporary); err != nil {
		_ = removePhysicalFile(temporary)
		return nil, err
	}
	return &StagedBackup{Path: temporary, remove: removePhysicalFile}, nil
}

func (s *SQLite) backupTo(ctx context.Context, destination string) error {
	connection, err := s.db.Conn(ctx)
	if err != nil {
		return err
	}
	defer connection.Close()
	return connection.Raw(func(driverConnection any) error {
		conn, ok := driverConnection.(sqliteDriver.Conn)
		if !ok {
			return errors.New("unexpected SQLite driver connection")
		}
		return conn.Raw().Backup("main", physicalURI(destination, s.vfs))
	})
}

func (s *SQLite) temporaryPath(kind string) string {
	return s.path + "." + kind + "-" + fmt.Sprint(s.nextID.Add(1)) + ".db"
}

func physicalURI(path, vfsName string) string {
	query := url.Values{}
	if vfsName != "" {
		query.Set("vfs", vfsName)
		query.Set("nolock", "1")
	}
	return "file:" + path + "?" + query.Encode()
}
