package persistence

import (
	"archive/zip"
	"context"
	"errors"
	"fmt"
	"io"
	"net/url"
	"path"
	"path/filepath"
	"strings"
	"time"

	sqliteDriver "github.com/ncruces/go-sqlite3/driver"
)

const (
	backupFormatKey = "backup/format"
	backupKeyKey    = "backup/master_key"
	backupFormat    = "spin-sqlite-backup-v1"
)

type StagedBackup struct {
	Path      string
	Database  *SQLite
	MasterKey string
	remove    func(string) error
}

// BackupUpload assembles one portable database from chunks without ever
// requiring a request body larger than the surrounding HTTP transport allows.
// The ordering rules live in the shared chunk assembler; this type only owns
// the staging file the chunks land in.
type BackupUpload struct {
	chunks *chunkAssembler
	path   string
	vfs    string
}

type fileSink struct{ path string }

func (f fileSink) writeChunk(ctx context.Context, offset, length int64, source io.Reader) (int64, error) {
	return appendPhysicalFile(ctx, f.path, source, offset, length)
}

func (s *SQLite) BeginBackupUpload(maxBytes int64) (*BackupUpload, error) {
	if maxBytes <= 0 {
		return nil, errors.New("backup upload limit must be positive")
	}
	path := s.temporaryPath("restore-upload")
	if err := createPhysicalFile(path); err != nil {
		return nil, err
	}
	return &BackupUpload{chunks: newChunkAssembler(fileSink{path}, maxBytes), path: path, vfs: s.vfs}, nil
}

// Offset reports the committed prefix.
func (u *BackupUpload) Offset() int64 { return u.chunks.Offset() }

// WriteAt stores one chunk; see chunkAssembler.WriteAt for the contract.
func (u *BackupUpload) WriteAt(ctx context.Context, offset, length int64, source io.Reader) (int64, error) {
	return u.chunks.WriteAt(ctx, offset, length, source)
}

func (u *BackupUpload) Stage(expectedSize int64) (*StagedBackup, error) {
	if err := u.chunks.finish(expectedSize); err != nil {
		return nil, fmt.Errorf("backup %w", err)
	}
	backup, err := openStagedBackup(u.path, u.vfs)
	if err != nil {
		_ = removePhysicalFile(u.path)
		return nil, err
	}
	return backup, nil
}

func (u *BackupUpload) Close() error {
	if !u.chunks.close() {
		return nil
	}
	return removePhysicalFile(u.path)
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
	if err := s.backupTo(ctx, temporary, nil); err != nil {
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

// Names inside a streamed backup archive.
const (
	backupArchiveDatabase = "spin.db"
	backupArchiveKey      = "master-key.txt"
)

// StreamBackup writes the live database to destination without a copy on
// the volume: a zip holding the database file and the portable master key.
// The pool has one connection; holding it pauses every write, so the file
// is a committed, consistent snapshot for as long as the stream runs. The
// key travels next to the file because the live database deliberately
// never contains it.
func (s *SQLite) StreamBackup(ctx context.Context, destination io.Writer, masterKey string) error {
	connection, err := s.db.Conn(ctx)
	if err != nil {
		return err
	}
	defer connection.Close()
	// Make sure nothing is left in SQLite's page cache or the VFS write
	// coalescer: a statement on the held connection forces a sync point.
	if _, err := connection.ExecContext(ctx, `PRAGMA user_version`); err != nil {
		return err
	}
	archive := zip.NewWriter(destination)
	database, err := archive.CreateHeader(&zip.FileHeader{Name: backupArchiveDatabase, Method: zip.Store, Modified: time.Now()})
	if err != nil {
		return err
	}
	if err := readPhysicalFile(ctx, s.path, database); err != nil {
		return fmt.Errorf("stream database file: %w", err)
	}
	key, err := archive.CreateHeader(&zip.FileHeader{Name: backupArchiveKey, Method: zip.Store, Modified: time.Now()})
	if err != nil {
		return err
	}
	if _, err := io.WriteString(key, strings.TrimSpace(masterKey)+"\n"); err != nil {
		return err
	}
	return archive.Close()
}

// CleanTemporaryFiles removes staged backup and restore copies a previous
// process left on the volume; each is as large as the database itself.
func (s *SQLite) CleanTemporaryFiles() (int, error) {
	directory := path.Dir(filepath.ToSlash(s.path))
	base := path.Base(filepath.ToSlash(s.path))
	names, err := listPhysicalDir(directory)
	if err != nil {
		return 0, err
	}
	removed := 0
	for _, name := range names {
		if !strings.HasPrefix(name, base+".backup-") && !strings.HasPrefix(name, base+".restore-") {
			continue
		}
		if err := removePhysicalFile(path.Join(directory, name)); err != nil {
			return removed, err
		}
		removed++
	}
	return removed, nil
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

// openStagedBackup accepts both backup forms: a database copy that carries
// its format marker and key inside, and a streamed zip holding the database
// file next to the key. A zip is unpacked to its own staged file first.
func openStagedBackup(path, vfs string) (*StagedBackup, error) {
	if isZipFile(path) {
		return openStagedBackupZip(path, vfs)
	}
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

func isZipFile(path string) bool {
	reader, size, closeReader, err := openPhysicalReaderAt(path)
	if err != nil || size < 4 {
		return false
	}
	defer closeReader()
	var magic [4]byte
	if _, err := reader.ReadAt(magic[:], 0); err != nil {
		return false
	}
	return string(magic[:]) == "PK\x03\x04"
}

func openStagedBackupZip(zipPath, vfs string) (*StagedBackup, error) {
	reader, size, closeReader, err := openPhysicalReaderAt(zipPath)
	if err != nil {
		return nil, fmt.Errorf("open uploaded backup archive: %w", err)
	}
	defer closeReader()
	archive, err := zip.NewReader(reader, size)
	if err != nil {
		return nil, fmt.Errorf("read uploaded backup archive: %w", err)
	}
	var databaseEntry, keyEntry *zip.File
	for _, entry := range archive.File {
		switch entry.Name {
		case backupArchiveDatabase:
			databaseEntry = entry
		case backupArchiveKey:
			keyEntry = entry
		}
	}
	if databaseEntry == nil || keyEntry == nil {
		return nil, errors.New("backup archive must contain spin.db and master-key.txt")
	}
	keyReader, err := keyEntry.Open()
	if err != nil {
		return nil, err
	}
	keyBytes, err := io.ReadAll(io.LimitReader(keyReader, 4096))
	_ = keyReader.Close()
	if err != nil || strings.TrimSpace(string(keyBytes)) == "" {
		return nil, errors.New("backup archive has no master key")
	}
	databaseReader, err := databaseEntry.Open()
	if err != nil {
		return nil, err
	}
	extracted := strings.TrimSuffix(zipPath, ".db") + "-unpacked.db"
	writeErr := writePhysicalFile(context.Background(), extracted, databaseReader, int64(databaseEntry.UncompressedSize64))
	_ = databaseReader.Close()
	_ = removePhysicalFile(zipPath)
	if writeErr != nil {
		_ = removePhysicalFile(extracted)
		return nil, fmt.Errorf("unpack backup archive: %w", writeErr)
	}
	backup, err := Open(extracted, OpenOptions{VFS: vfs})
	if err != nil {
		_ = removePhysicalFile(extracted)
		return nil, fmt.Errorf("open unpacked backup: %w", err)
	}
	if _, err := backup.ReadFile("state"); err != nil {
		_ = backup.Close()
		_ = removePhysicalFile(extracted)
		return nil, errors.New("not a Spin database backup: it has no state")
	}
	return &StagedBackup{Path: extracted, Database: backup, MasterKey: strings.TrimSpace(string(keyBytes)), remove: removePhysicalFile}, nil
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
	if err := s.backupTo(ctx, temporary, nil); err != nil {
		_ = removePhysicalFile(temporary)
		return nil, err
	}
	return &StagedBackup{Path: temporary, remove: removePhysicalFile}, nil
}

// backupTo copies the database with SQLite's online backup API, in steps of
// pages so the copy can be cancelled and its progress reported.
func (s *SQLite) backupTo(ctx context.Context, destination string, progress func(copied, total int)) error {
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
		backup, err := conn.Raw().BackupInit("main", physicalURI(destination, s.vfs))
		if err != nil {
			return err
		}
		for {
			if err := ctx.Err(); err != nil {
				_ = backup.Close()
				return err
			}
			done, err := backup.Step(256)
			if err != nil {
				_ = backup.Close()
				return err
			}
			if progress != nil {
				total := backup.PageCount()
				progress(total-backup.Remaining(), total)
			}
			if done {
				return backup.Close()
			}
		}
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
