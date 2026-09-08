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
	"time"

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

	migrations []string
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

// sqliteDSN builds the connection string. On HopOS the order of the pragmas
// matters and url.Values would sort them, so the query is written out by hand.
//
// locking_mode comes first, and it is the setting that matters most. This
// process is the only one that can reach the file, which is what a slot is, so
// holding the lock costs nothing and lets SQLite keep its page cache between
// statements instead of dropping it every time it releases the lock. It also
// has to be the first pragma: anything before it touches the database and the
// mode can no longer be raised. cache_size and synchronous follow because
// neither is stored in the file, so a second connection would silently fall
// back to a 2 MiB cache and to whatever durability the build defaults to.
func sqliteDSN(path, vfsName string) string {
	dsn := "file:" + filepath.ToSlash(path)
	if vfsName == "" {
		return dsn + "?_pragma=foreign_keys(1)"
	}
	return dsn + "?vfs=" + url.QueryEscape(vfsName) +
		"&_pragma=locking_mode(exclusive)" +
		"&_pragma=cache_size(-65536)" +
		"&_pragma=synchronous(full)" +
		"&_pragma=foreign_keys(1)"
}

// The two tables that carry large rows are rowid tables on purpose. In a
// WITHOUT ROWID table the whole row is the b-tree key, so every descent
// compares against neighbouring rows and SQLite fetches their complete
// records, overflow pages included: on HopOS that was ~10 MiB of volume reads
// for every 1 MiB chunk inserted, and it grew with the table. With the blob in
// the row and only (object_id, sequence) in the index, an insert reads nothing.
const (
	createKV = `CREATE TABLE IF NOT EXISTS spin_kv (
			key TEXT PRIMARY KEY,
			value BLOB NOT NULL
		)`
	createChunks = `CREATE TABLE IF NOT EXISTS spin_object_chunks (
			id INTEGER PRIMARY KEY,
			object_id INTEGER NOT NULL REFERENCES spin_objects(id) ON DELETE CASCADE,
			sequence INTEGER NOT NULL,
			data BLOB NOT NULL,
			UNIQUE(object_id, sequence)
		)`
)

func (s *SQLite) initialize(ctx context.Context) error {
	pragmas := []string{
		// 64 KiB pages: a 1 MiB chunk is 17 pages instead of 257, so a cold
		// read or a cascade delete costs 17 volume calls per MiB. Only a new
		// database picks this up; an existing file keeps its page size.
		`PRAGMA page_size=65536`,
		// A rollback journal, not WAL. The win everyone reaches for in WAL is
		// really the exclusive lock (see sqliteDSN): a connection that keeps
		// its lock keeps its page cache between statements, and that is what
		// takes point lookups from 8423 to 68222 per second on a HopOS slot.
		// WAL adds nothing on top of that here and costs bulk: every byte of a
		// megabyte chunk reaches the database a second time at checkpoint, so
		// uploads dropped from 429 to 190 MB/s with stalls of 200 ms. Measured
		// 05-09; `vitals` test `sqlite` prints all four variants side by side.
		`PRAGMA journal_mode=DELETE`,
	}
	for _, statement := range pragmas {
		if _, err := s.db.ExecContext(ctx, statement); err != nil {
			return fmt.Errorf("initialize SQLite: %w", err)
		}
	}
	if err := s.rebuildWithoutRowid(ctx); err != nil {
		return fmt.Errorf("initialize SQLite: %w", err)
	}
	statements := []string{
		createKV,
		`CREATE TABLE IF NOT EXISTS spin_objects (
			id INTEGER PRIMARY KEY,
			digest TEXT,
			kind TEXT NOT NULL,
			size INTEGER NOT NULL DEFAULT 0,
			complete INTEGER NOT NULL DEFAULT 0
		)`,
		`CREATE UNIQUE INDEX IF NOT EXISTS spin_objects_digest
			ON spin_objects(digest) WHERE complete = 1`,
		createChunks,
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

// rebuildWithoutRowid converts spin_kv and spin_object_chunks from the earlier
// WITHOUT ROWID form to rowid tables, in place and in one transaction per
// table. Foreign-key bookkeeping is off for the duration: DROP TABLE would
// otherwise run an implicit DELETE that walks every row first.
func (s *SQLite) rebuildWithoutRowid(ctx context.Context) error {
	rebuilds := []struct{ table, create, columns string }{
		{"spin_kv", createKV, "key, value"},
		{"spin_object_chunks", createChunks, "object_id, sequence, data"},
	}
	conn, err := s.db.Conn(ctx)
	if err != nil {
		return err
	}
	defer conn.Close()
	for _, r := range rebuilds {
		var definition string
		err := conn.QueryRowContext(ctx, `SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?`, r.table).Scan(&definition)
		if errors.Is(err, sql.ErrNoRows) {
			continue
		}
		if err != nil {
			return err
		}
		if !strings.Contains(strings.ToUpper(definition), "WITHOUT ROWID") {
			continue
		}
		started := time.Now()
		if _, err := conn.ExecContext(ctx, `PRAGMA foreign_keys=OFF`); err != nil {
			return err
		}
		err = func() error {
			tx, err := conn.BeginTx(ctx, nil)
			if err != nil {
				return err
			}
			defer tx.Rollback()
			scratch := r.table + "_rowid"
			steps := []string{
				strings.Replace(r.create, "IF NOT EXISTS "+r.table, scratch, 1),
				fmt.Sprintf(`INSERT INTO %s(%s) SELECT %s FROM %s ORDER BY %s`, scratch, r.columns, r.columns, r.table, r.columns),
				`DROP TABLE ` + r.table,
				fmt.Sprintf(`ALTER TABLE %s RENAME TO %s`, scratch, r.table),
			}
			for _, step := range steps {
				if _, err := tx.ExecContext(ctx, step); err != nil {
					return fmt.Errorf("%s: %w", r.table, err)
				}
			}
			return tx.Commit()
		}()
		if _, fkErr := conn.ExecContext(ctx, `PRAGMA foreign_keys=ON`); err == nil {
			err = fkErr
		}
		if err != nil {
			return fmt.Errorf("rebuild %s as rowid table: %w", r.table, err)
		}
		s.migrations = append(s.migrations, fmt.Sprintf("rebuilt %s as a rowid table in %s", r.table, time.Since(started).Round(time.Millisecond)))
	}
	return nil
}

// Migrations reports what Open changed about an existing database, for the
// startup log.
func (s *SQLite) Migrations() []string { return s.migrations }

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

// ReadBlobChunk returns the stored chunk that starts at offset, which must
// be a multiple of the chunk size: blobs are stored as aligned 1 MiB rows, so
// a runner can pull a snapshot in resumable pieces without the server
// reading anything it does not send.
func (s *SQLite) ReadBlobChunk(ctx context.Context, ref string, offset int64) ([]byte, BlobInfo, error) {
	info, err := s.BlobInfo(ctx, ref)
	if err != nil {
		return nil, BlobInfo{}, err
	}
	if offset < 0 {
		return nil, info, fmt.Errorf("blob chunk offset %d is negative", offset)
	}
	if offset >= info.Size {
		return nil, info, nil
	}
	if offset%blobChunkSize != 0 {
		return nil, info, fmt.Errorf("blob chunk offset %d is not aligned to %d bytes", offset, blobChunkSize)
	}
	var chunk []byte
	err = s.db.QueryRowContext(ctx, `SELECT c.data
		FROM spin_object_refs r
		JOIN spin_objects o ON o.id = r.object_id
		JOIN spin_object_chunks c ON c.object_id = o.id
		WHERE r.ref = ? AND o.complete = 1 AND c.sequence = ?`, ref, offset/blobChunkSize).Scan(&chunk)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, info, fmt.Errorf("blob %s has no chunk at %d: %w", ref, offset, fs.ErrNotExist)
	}
	if err != nil {
		return nil, info, err
	}
	return chunk, info, nil
}

// ReadSnapshotChunk is ReadBlobChunk for an archived snapshot.
func (s *SQLite) ReadSnapshotChunk(ctx context.Context, snapshot domain.CapsuleSnapshot, offset int64) ([]byte, BlobInfo, error) {
	return s.ReadBlobChunk(ctx, snapshotRef(snapshot), offset)
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

// StorageUsage is what the database occupies: the file, and the objects
// (snapshots, attachments) inside it.
type StorageUsage struct {
	DatabaseBytes int64 `json:"database_bytes"`
	ObjectBytes   int64 `json:"object_bytes"`
	Objects       int   `json:"objects"`
}

// Usage reports the database file size from SQLite's own page count, so it
// works on every VFS, and the bytes held by complete objects.
func (s *SQLite) Usage(ctx context.Context) (StorageUsage, error) {
	var usage StorageUsage
	var pageCount, pageSize int64
	if err := s.db.QueryRowContext(ctx, `PRAGMA page_count`).Scan(&pageCount); err != nil {
		return usage, err
	}
	if err := s.db.QueryRowContext(ctx, `PRAGMA page_size`).Scan(&pageSize); err != nil {
		return usage, err
	}
	usage.DatabaseBytes = pageCount * pageSize
	if err := s.db.QueryRowContext(ctx, `SELECT COUNT(*), COALESCE(SUM(size), 0) FROM spin_objects WHERE complete = 1`).Scan(&usage.Objects, &usage.ObjectBytes); err != nil {
		return usage, err
	}
	return usage, nil
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
