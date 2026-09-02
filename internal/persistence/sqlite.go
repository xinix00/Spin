package persistence

import (
	"bytes"
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"

	"easyacp/internal/domain"

	_ "github.com/ncruces/go-sqlite3/driver"
)

const blobChunkSize = 1 << 20

// SQLite is Spin's durable control-plane database and binary object store.
// Large objects are split into rows so Docker snapshots can travel from a
// runner into the database without ever being collected in server memory.
type SQLite struct {
	db     *sql.DB
	dsn    string
	path   string
	vfs    string
	fsPath string
	nextID atomic.Uint64
}

type OpenOptions struct {
	// VFS selects an already registered ncruces SQLite VFS. It is empty on
	// normal operating systems and set by the HopOS entrypoint.
	VFS string
}

type BlobInfo struct {
	Ref    string `json:"ref"`
	Digest string `json:"digest"`
	Kind   string `json:"kind"`
	Size   int64  `json:"size"`
}

func Open(path string, options OpenOptions) (*SQLite, error) {
	path = strings.TrimSpace(path)
	if path == "" {
		return nil, errors.New("SQLite path is required")
	}
	dsn := sqliteDSN(path, options.VFS)
	db, err := sql.Open("sqlite3", dsn)
	if err != nil {
		return nil, fmt.Errorf("open SQLite: %w", err)
	}
	// Spin serializes its state machine already. One database connection also
	// keeps the nolock HopOS VFS honest and prevents accidental lock variants.
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)
	store := &SQLite{db: db, dsn: dsn, path: path, vfs: options.VFS}
	if options.VFS == "" {
		store.fsPath = path
	}
	if err := store.initialize(context.Background()); err != nil {
		_ = db.Close()
		return nil, err
	}
	return store, nil
}

func sqliteDSN(path, vfsName string) string {
	query := url.Values{}
	query.Set("_pragma", "foreign_keys(1)")
	if vfsName != "" {
		query.Set("vfs", vfsName)
		query.Set("nolock", "1")
	}
	return "file:" + filepath.ToSlash(path) + "?" + query.Encode()
}

func (s *SQLite) initialize(ctx context.Context) error {
	statements := []string{
		`PRAGMA journal_mode=DELETE`,
		`PRAGMA synchronous=FULL`,
		`CREATE TABLE IF NOT EXISTS spin_kv (
			key TEXT PRIMARY KEY,
			value BLOB NOT NULL
		) WITHOUT ROWID`,
		`CREATE TABLE IF NOT EXISTS spin_objects (
			id INTEGER PRIMARY KEY,
			digest TEXT,
			kind TEXT NOT NULL,
			size INTEGER NOT NULL DEFAULT 0,
			complete INTEGER NOT NULL DEFAULT 0
		)`,
		`CREATE UNIQUE INDEX IF NOT EXISTS spin_objects_digest
			ON spin_objects(digest) WHERE complete = 1`,
		`CREATE TABLE IF NOT EXISTS spin_object_chunks (
			object_id INTEGER NOT NULL REFERENCES spin_objects(id) ON DELETE CASCADE,
			sequence INTEGER NOT NULL,
			data BLOB NOT NULL,
			PRIMARY KEY(object_id, sequence)
		) WITHOUT ROWID`,
		`CREATE TABLE IF NOT EXISTS spin_object_refs (
			ref TEXT PRIMARY KEY,
			object_id INTEGER NOT NULL REFERENCES spin_objects(id),
			FOREIGN KEY(object_id) REFERENCES spin_objects(id)
		) WITHOUT ROWID`,
		`DELETE FROM spin_objects WHERE complete = 0`,
	}
	for _, statement := range statements {
		if _, err := s.db.ExecContext(ctx, statement); err != nil {
			return fmt.Errorf("initialize SQLite: %w", err)
		}
	}
	return nil
}

func (s *SQLite) Close() error { return s.db.Close() }

func (s *SQLite) Path() string { return s.fsPath }

// ReadFile and WriteFile implement store.StateBackend. The path is a logical
// key, not a host filename.
func (s *SQLite) ReadFile(path string) ([]byte, error) {
	var value []byte
	err := s.db.QueryRow(`SELECT value FROM spin_kv WHERE key = ?`, path).Scan(&value)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, fs.ErrNotExist
	}
	if err != nil {
		return nil, err
	}
	return append([]byte(nil), value...), nil
}

func (s *SQLite) WriteFile(path string, data []byte) error {
	_, err := s.db.Exec(`INSERT INTO spin_kv(key, value) VALUES(?, ?)
		ON CONFLICT(key) DO UPDATE SET value = excluded.value`, path, data)
	return err
}

func (s *SQLite) DeleteFile(path string) error {
	_, err := s.db.Exec(`DELETE FROM spin_kv WHERE key = ?`, path)
	return err
}

func (s *SQLite) ImportFileIfMissing(key, source string) (bool, error) {
	if _, err := s.ReadFile(key); err == nil {
		return false, nil
	} else if !errors.Is(err, fs.ErrNotExist) {
		return false, err
	}
	data, err := os.ReadFile(source)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	if err := s.WriteFile(key, data); err != nil {
		return false, err
	}
	return true, nil
}

func (s *SQLite) PutBlob(ctx context.Context, ref, kind string, source io.Reader) (BlobInfo, error) {
	ref = strings.TrimSpace(ref)
	if ref == "" {
		return BlobInfo{}, errors.New("blob reference is required")
	}
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return BlobInfo{}, err
	}
	result, err := tx.ExecContext(ctx, `INSERT INTO spin_objects(kind) VALUES(?)`, strings.TrimSpace(kind))
	if err != nil {
		_ = tx.Rollback()
		return BlobInfo{}, err
	}
	objectID, err := result.LastInsertId()
	if err != nil {
		_ = tx.Rollback()
		return BlobInfo{}, err
	}
	hash := sha256.New()
	buffer := make([]byte, blobChunkSize)
	var size int64
	for sequence := 0; ; sequence++ {
		count, readErr := io.ReadFull(source, buffer)
		if readErr != nil && !errors.Is(readErr, io.EOF) && !errors.Is(readErr, io.ErrUnexpectedEOF) {
			_ = tx.Rollback()
			return BlobInfo{}, readErr
		}
		if count > 0 {
			chunk := buffer[:count]
			if _, err := hash.Write(chunk); err != nil {
				_ = tx.Rollback()
				return BlobInfo{}, err
			}
			if _, err := tx.ExecContext(ctx, `INSERT INTO spin_object_chunks(object_id, sequence, data) VALUES(?, ?, ?)`, objectID, sequence, chunk); err != nil {
				_ = tx.Rollback()
				return BlobInfo{}, err
			}
			size += int64(count)
		}
		if readErr != nil {
			break
		}
	}
	digest := "sha256:" + hex.EncodeToString(hash.Sum(nil))
	var existingID int64
	err = tx.QueryRowContext(ctx, `SELECT id FROM spin_objects WHERE digest = ? AND complete = 1`, digest).Scan(&existingID)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		_ = tx.Rollback()
		return BlobInfo{}, err
	}
	if err == nil {
		if _, err := tx.ExecContext(ctx, `DELETE FROM spin_objects WHERE id = ?`, objectID); err != nil {
			_ = tx.Rollback()
			return BlobInfo{}, err
		}
		objectID = existingID
	} else if _, err := tx.ExecContext(ctx, `UPDATE spin_objects SET digest = ?, size = ?, complete = 1 WHERE id = ?`, digest, size, objectID); err != nil {
		_ = tx.Rollback()
		return BlobInfo{}, err
	}
	if _, err := tx.ExecContext(ctx, `INSERT INTO spin_object_refs(ref, object_id) VALUES(?, ?)
		ON CONFLICT(ref) DO UPDATE SET object_id = excluded.object_id`, ref, objectID); err != nil {
		_ = tx.Rollback()
		return BlobInfo{}, err
	}
	if err := tx.Commit(); err != nil {
		return BlobInfo{}, err
	}
	return BlobInfo{Ref: ref, Digest: digest, Kind: strings.TrimSpace(kind), Size: size}, nil
}

func (s *SQLite) BlobInfo(ctx context.Context, ref string) (BlobInfo, error) {
	var info BlobInfo
	info.Ref = ref
	err := s.db.QueryRowContext(ctx, `SELECT o.digest, o.kind, o.size
		FROM spin_object_refs r JOIN spin_objects o ON o.id = r.object_id
		WHERE r.ref = ? AND o.complete = 1`, ref).Scan(&info.Digest, &info.Kind, &info.Size)
	if errors.Is(err, sql.ErrNoRows) {
		return BlobInfo{}, fs.ErrNotExist
	}
	return info, err
}

func (s *SQLite) WriteBlobTo(ctx context.Context, ref string, destination io.Writer) (BlobInfo, error) {
	info, err := s.BlobInfo(ctx, ref)
	if err != nil {
		return BlobInfo{}, err
	}
	rows, err := s.db.QueryContext(ctx, `SELECT c.data
		FROM spin_object_refs r
		JOIN spin_objects o ON o.id = r.object_id
		JOIN spin_object_chunks c ON c.object_id = o.id
		WHERE r.ref = ? AND o.complete = 1 ORDER BY c.sequence`, ref)
	if err != nil {
		return BlobInfo{}, err
	}
	defer rows.Close()
	hash := sha256.New()
	written := int64(0)
	for rows.Next() {
		var chunk []byte
		if err := rows.Scan(&chunk); err != nil {
			return BlobInfo{}, err
		}
		count, err := destination.Write(chunk)
		if err != nil {
			return BlobInfo{}, err
		}
		if count != len(chunk) {
			return BlobInfo{}, io.ErrShortWrite
		}
		_, _ = hash.Write(chunk)
		written += int64(count)
	}
	if err := rows.Err(); err != nil {
		return BlobInfo{}, err
	}
	digest := "sha256:" + hex.EncodeToString(hash.Sum(nil))
	if written != info.Size || digest != info.Digest {
		return BlobInfo{}, fmt.Errorf("blob %s is corrupt: got %s/%d, expected %s/%d", ref, digest, written, info.Digest, info.Size)
	}
	return info, nil
}

func (s *SQLite) ReadBlob(ctx context.Context, ref string, limit int64) ([]byte, BlobInfo, error) {
	info, err := s.BlobInfo(ctx, ref)
	if err != nil {
		return nil, BlobInfo{}, err
	}
	if limit >= 0 && info.Size > limit {
		return nil, BlobInfo{}, fmt.Errorf("blob %s exceeds limit", ref)
	}
	var builder bytes.Buffer
	if info.Size <= int64(^uint(0)>>1) {
		builder.Grow(int(info.Size))
	}
	if _, err := s.WriteBlobTo(ctx, ref, &builder); err != nil {
		return nil, BlobInfo{}, err
	}
	return builder.Bytes(), info, nil
}

func (s *SQLite) DeleteBlob(ctx context.Context, ref string) error {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	var objectID int64
	err = tx.QueryRowContext(ctx, `SELECT object_id FROM spin_object_refs WHERE ref = ?`, ref).Scan(&objectID)
	if errors.Is(err, sql.ErrNoRows) {
		_ = tx.Rollback()
		return fs.ErrNotExist
	}
	if err != nil {
		_ = tx.Rollback()
		return err
	}
	if _, err := tx.ExecContext(ctx, `DELETE FROM spin_object_refs WHERE ref = ?`, ref); err != nil {
		_ = tx.Rollback()
		return err
	}
	if _, err := tx.ExecContext(ctx, `DELETE FROM spin_objects WHERE id = ? AND NOT EXISTS (SELECT 1 FROM spin_object_refs WHERE object_id = ?)`, objectID, objectID); err != nil {
		_ = tx.Rollback()
		return err
	}
	return tx.Commit()
}

func snapshotRef(snapshot domain.CapsuleSnapshot) string {
	return "snapshot:" + strings.TrimSpace(snapshot.Digest)
}

func (s *SQLite) StoreSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot, source io.Reader) error {
	_, err := s.PutBlob(ctx, snapshotRef(snapshot), "docker-snapshot", source)
	return err
}

func (s *SQLite) RestoreSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot, destination io.Writer) error {
	_, err := s.WriteBlobTo(ctx, snapshotRef(snapshot), destination)
	return err
}

func (s *SQLite) HasSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot) (bool, error) {
	_, err := s.BlobInfo(ctx, snapshotRef(snapshot))
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	return err == nil, err
}

func (s *SQLite) RemoveArchivedSnapshot(ctx context.Context, snapshot domain.CapsuleSnapshot) error {
	err := s.DeleteBlob(ctx, snapshotRef(snapshot))
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	return err
}

// FileStore maps the small file-shaped attachment interface onto database
// object references. The namespace prevents collisions with snapshot blobs.
type FileStore struct {
	database  *SQLite
	namespace string
	kind      string
	limit     int64
}

func (s *SQLite) Files(namespace, kind string, limit int64) *FileStore {
	return &FileStore{database: s, namespace: strings.TrimSpace(namespace), kind: strings.TrimSpace(kind), limit: limit}
}

func (f *FileStore) ref(name string) string { return f.namespace + strings.TrimSpace(name) }

func (f *FileStore) ReadFile(name string) ([]byte, error) {
	data, _, err := f.database.ReadBlob(context.Background(), f.ref(name), f.limit)
	return data, err
}

func (f *FileStore) WriteFile(name string, data []byte) error {
	if f.limit >= 0 && int64(len(data)) > f.limit {
		return errors.New("file exceeds storage limit")
	}
	_, err := f.database.PutBlob(context.Background(), f.ref(name), f.kind, bytes.NewReader(data))
	return err
}

func (f *FileStore) Remove(name string) error {
	err := f.database.DeleteBlob(context.Background(), f.ref(name))
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	return err
}

func (*FileStore) LocalPath(string) string { return "" }
