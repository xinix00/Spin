package persistence

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"hash"
	"io"
	"runtime"
	"strings"
	"sync"

	"easyacp/internal/domain"
)

// SnapshotUpload archives one Docker snapshot from chunks that a runner sends
// as separate requests. Every chunk becomes one object row the moment it
// arrives, so nothing is buffered on the control plane and an interrupted
// upload resumes at the committed prefix. The object stays incomplete, and
// therefore invisible to HasSnapshot, until Complete has verified every chunk.
type SnapshotUpload struct {
	database *SQLite
	snapshot domain.CapsuleSnapshot
	// kind and refPrefix describe an object that is not a snapshot: a
	// deliverable bundle is kept under bundle:<digest>.
	kind      string
	refPrefix string
	objectID  int64
	size      int64
	chunks    *chunkAssembler

	mu        sync.Mutex
	completed bool

	// The digest is taken while the chunks arrive, in the order of the
	// committed prefix, so Complete does not have to read gigabytes back.
	// Chunks that land ahead of the prefix wait in ahead (at most the
	// assembler's reorder window). A chunk written twice makes the running
	// digest unreliable; Complete then reads the rows back instead.
	hashMu    sync.Mutex
	hash      hash.Hash
	hashed    int64
	ahead     map[int64][]byte
	rewritten bool
}

type blobSink struct{ upload *SnapshotUpload }

func (s *SQLite) BeginSnapshotUpload(ctx context.Context, snapshot domain.CapsuleSnapshot, size int64) (*SnapshotUpload, error) {
	if strings.TrimSpace(snapshot.Digest) == "" {
		return nil, errors.New("snapshot digest is required")
	}
	if size <= 0 {
		return nil, errors.New("snapshot size must be positive")
	}
	return s.beginObjectUpload(ctx, "docker-snapshot", "", snapshot, size)
}

// BeginBundleUpload receives a deliverable bundle (a zip) the way a
// snapshot arrives: in chunks, published under bundle:<digest> once
// complete.
func (s *SQLite) BeginBundleUpload(ctx context.Context, size int64) (*SnapshotUpload, error) {
	if size <= 0 {
		return nil, errors.New("bundle size must be positive")
	}
	return s.beginObjectUpload(ctx, "deliverable-bundle", "bundle:", domain.CapsuleSnapshot{}, size)
}

func (s *SQLite) beginObjectUpload(ctx context.Context, kind, refPrefix string, snapshot domain.CapsuleSnapshot, size int64) (*SnapshotUpload, error) {
	result, err := s.db.ExecContext(ctx, `INSERT INTO spin_objects(kind) VALUES(?)`, kind)
	if err != nil {
		return nil, err
	}
	objectID, err := result.LastInsertId()
	if err != nil {
		return nil, err
	}
	upload := &SnapshotUpload{database: s, snapshot: snapshot, kind: kind, refPrefix: refPrefix, objectID: objectID, size: size,
		hash: sha256.New(), ahead: map[int64][]byte{}}
	upload.chunks = newChunkAssembler(blobSink{upload}, size)
	return upload, nil
}

// writeChunk stores one aligned chunk as its own row. The alignment rule keeps
// row sequence and byte offset the same thing, which is what makes a retried
// chunk a plain replace.
func (b blobSink) writeChunk(ctx context.Context, offset, length int64, source io.Reader) (int64, error) {
	upload := b.upload
	if offset%blobChunkSize != 0 || (length != blobChunkSize && offset+length != upload.size) {
		return 0, fmt.Errorf("snapshot chunks must be %d bytes and aligned, except the last", blobChunkSize)
	}
	data := make([]byte, length)
	if _, err := io.ReadFull(source, data); err != nil {
		return 0, err
	}
	_, err := upload.database.db.ExecContext(ctx, `INSERT OR REPLACE INTO spin_object_chunks(object_id, sequence, data) VALUES(?, ?, ?)`,
		upload.objectID, offset/blobChunkSize, data)
	if err != nil {
		return 0, err
	}
	upload.digestChunk(offset, data)
	return length, nil
}

// digestChunk feeds a stored chunk to the running digest once every byte
// before it has been fed.
func (u *SnapshotUpload) digestChunk(offset int64, data []byte) {
	u.hashMu.Lock()
	defer u.hashMu.Unlock()
	if _, waiting := u.ahead[offset]; waiting || offset < u.hashed {
		u.rewritten = true
		return
	}
	u.ahead[offset] = data
	for {
		next, ok := u.ahead[u.hashed]
		if !ok {
			return
		}
		delete(u.ahead, u.hashed)
		_, _ = u.hash.Write(next)
		u.hashed += int64(len(next))
	}
}

// runningDigest is the digest taken on arrival, when it covers exactly the
// declared size and no chunk was written twice.
func (u *SnapshotUpload) runningDigest() (string, bool) {
	u.hashMu.Lock()
	defer u.hashMu.Unlock()
	if u.rewritten || len(u.ahead) != 0 || u.hashed != u.size {
		return "", false
	}
	return "sha256:" + hex.EncodeToString(u.hash.Sum(nil)), true
}

// Offset reports the committed prefix.
func (u *SnapshotUpload) Offset() int64 { return u.chunks.Offset() }

// WriteAt stores one chunk; see chunkAssembler.WriteAt for the contract.
func (u *SnapshotUpload) WriteAt(ctx context.Context, offset, length int64, source io.Reader) (int64, error) {
	return u.chunks.WriteAt(ctx, offset, length, source)
}

// Complete verifies the assembled object against its declared size, records
// its content digest and publishes it under the snapshot reference. A snapshot
// that already exists with the same content is reused rather than duplicated.
//
// A snapshot runs to gigabytes, and the server has one database connection
// and, on HopOS, nothing that preempts a goroutine. Reading it back under one
// transaction to hash it held every request in the server until the last
// chunk was done: the browser following End & save gave up, and the runner's
// complete ran into the edge's 100 second limit. So the digest is taken on
// arrival, and only the publish is a transaction. Nothing else writes these
// rows (the assembler is closed, Close waits for u.mu).
func (u *SnapshotUpload) Complete(ctx context.Context) (BlobInfo, error) {
	if err := u.chunks.finish(u.size); err != nil {
		return BlobInfo{}, fmt.Errorf("snapshot %w", err)
	}
	u.mu.Lock()
	defer u.mu.Unlock()
	chunks := (u.size + blobChunkSize - 1) / blobChunkSize
	var stored, size int64
	if err := u.database.db.QueryRowContext(ctx, `SELECT COUNT(*), COALESCE(SUM(length(data)), 0) FROM spin_object_chunks WHERE object_id = ?`, u.objectID).Scan(&stored, &size); err != nil {
		return BlobInfo{}, err
	}
	if stored != chunks || size != u.size {
		return BlobInfo{}, fmt.Errorf("snapshot has %d chunks and %d bytes, declared %d chunks and %d bytes", stored, size, chunks, u.size)
	}
	digest, ok := u.runningDigest()
	if !ok {
		var err error
		if digest, err = u.readDigest(ctx, chunks); err != nil {
			return BlobInfo{}, err
		}
	}
	tx, err := u.database.db.BeginTx(ctx, nil)
	if err != nil {
		return BlobInfo{}, err
	}
	objectID := u.objectID
	var existingID int64
	err = tx.QueryRowContext(ctx, `SELECT id FROM spin_objects WHERE digest = ? AND complete = 1`, digest).Scan(&existingID)
	switch {
	case err == nil:
		if _, err := tx.ExecContext(ctx, `DELETE FROM spin_objects WHERE id = ?`, objectID); err != nil {
			_ = tx.Rollback()
			return BlobInfo{}, err
		}
		objectID = existingID
	case errors.Is(err, sql.ErrNoRows):
		if _, err := tx.ExecContext(ctx, `UPDATE spin_objects SET digest = ?, size = ?, complete = 1 WHERE id = ?`, digest, size, objectID); err != nil {
			_ = tx.Rollback()
			return BlobInfo{}, err
		}
	default:
		_ = tx.Rollback()
		return BlobInfo{}, err
	}
	ref := snapshotRef(u.snapshot)
	if u.refPrefix != "" {
		ref = u.refPrefix + strings.TrimPrefix(digest, "sha256:")
	}
	if _, err := tx.ExecContext(ctx, `INSERT INTO spin_object_refs(ref, object_id) VALUES(?, ?)
		ON CONFLICT(ref) DO UPDATE SET object_id = excluded.object_id`, ref, objectID); err != nil {
		_ = tx.Rollback()
		return BlobInfo{}, err
	}
	if err := tx.Commit(); err != nil {
		return BlobInfo{}, err
	}
	u.completed = true
	return BlobInfo{Ref: ref, Digest: digest, Kind: u.kind, Size: size}, nil
}

// readDigest reads the rows back to hash them: one chunk per statement, and
// the processor given up after each, so the server keeps serving meanwhile.
func (u *SnapshotUpload) readDigest(ctx context.Context, chunks int64) (string, error) {
	hash := sha256.New()
	for sequence := int64(0); sequence < chunks; sequence++ {
		if err := ctx.Err(); err != nil {
			return "", err
		}
		var data []byte
		err := u.database.db.QueryRowContext(ctx, `SELECT data FROM spin_object_chunks WHERE object_id = ? AND sequence = ?`, u.objectID, sequence).Scan(&data)
		if errors.Is(err, sql.ErrNoRows) {
			return "", fmt.Errorf("snapshot chunk %d is missing", sequence)
		}
		if err != nil {
			return "", err
		}
		_, _ = hash.Write(data)
		runtime.Gosched()
	}
	return "sha256:" + hex.EncodeToString(hash.Sum(nil)), nil
}

// Close abandons an upload that did not complete and drops its rows.
func (u *SnapshotUpload) Close() error {
	u.chunks.close()
	u.mu.Lock()
	defer u.mu.Unlock()
	if u.completed {
		return nil
	}
	u.completed = true
	_, err := u.database.db.ExecContext(context.Background(), `DELETE FROM spin_objects WHERE id = ?`, u.objectID)
	return err
}

// SnapshotInfo describes an archived snapshot: its size and content digest.
func (s *SQLite) SnapshotInfo(ctx context.Context, snapshot domain.CapsuleSnapshot) (BlobInfo, error) {
	return s.BlobInfo(ctx, snapshotRef(snapshot))
}
