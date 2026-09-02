package persistence

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/url"
	"strings"

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
	backup, err := Open(temporary, OpenOptions{VFS: s.vfs})
	if err != nil {
		_ = removePhysicalFile(temporary)
		return nil, fmt.Errorf("open uploaded backup: %w", err)
	}
	format, err := backup.ReadFile(backupFormatKey)
	if err != nil || string(format) != backupFormat {
		_ = backup.Close()
		_ = removePhysicalFile(temporary)
		return nil, errors.New("not a supported Spin database backup")
	}
	key, err := backup.ReadFile(backupKeyKey)
	if err != nil || strings.TrimSpace(string(key)) == "" {
		_ = backup.Close()
		_ = removePhysicalFile(temporary)
		return nil, errors.New("Spin backup has no master key")
	}
	return &StagedBackup{Path: temporary, Database: backup, MasterKey: strings.TrimSpace(string(key)), remove: removePhysicalFile}, nil
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
