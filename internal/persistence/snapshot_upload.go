package persistence

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
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
	objectID int64
	size     int64
	chunks   *chunkAssembler

	mu        sync.Mutex
	completed bool
}

type blobSink struct{ upload *SnapshotUpload }

func (s *SQLite) BeginSnapshotUpload(ctx context.Context, snapshot domain.CapsuleSnapshot, size int64) (*SnapshotUpload, error) {
	if strings.TrimSpace(snapshot.Digest) == "" {
		return nil, errors.New("snapshot digest is required")
	}
	if size <= 0 {
		return nil, errors.New("snapshot size must be positive")
	}
	result, err := s.db.ExecContext(ctx, `INSERT INTO spin_objects(kind) VALUES('docker-snapshot')`)
	if err != nil {
		return nil, err
	}
	objectID, err := result.LastInsertId()
	if err != nil {
		return nil, err
	}
	upload := &SnapshotUpload{database: s, snapshot: snapshot, objectID: objectID, size: size}
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
	return length, nil
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
func (u *SnapshotUpload) Complete(ctx context.Context) (BlobInfo, error) {
	if err := u.chunks.finish(u.size); err != nil {
		return BlobInfo{}, fmt.Errorf("snapshot %w", err)
	}
	u.mu.Lock()
	defer u.mu.Unlock()
	tx, err := u.database.db.BeginTx(ctx, nil)
	if err != nil {
		return BlobInfo{}, err
	}
	rows, err := tx.QueryContext(ctx, `SELECT sequence, data FROM spin_object_chunks WHERE object_id = ? ORDER BY sequence`, u.objectID)
	if err != nil {
		_ = tx.Rollback()
		return BlobInfo{}, err
	}
	hash := sha256.New()
	var size int64
	expected := int64(0)
	for rows.Next() {
		var sequence int64
		var data []byte
		if err := rows.Scan(&sequence, &data); err != nil {
			rows.Close()
			_ = tx.Rollback()
			return BlobInfo{}, err
		}
		if sequence != expected {
			rows.Close()
			_ = tx.Rollback()
			return BlobInfo{}, fmt.Errorf("snapshot chunk %d is missing", expected)
		}
		expected++
		_, _ = hash.Write(data)
		size += int64(len(data))
	}
	if err := rows.Err(); err != nil {
		rows.Close()
		_ = tx.Rollback()
		return BlobInfo{}, err
	}
	rows.Close()
	if size != u.size {
		_ = tx.Rollback()
		return BlobInfo{}, fmt.Errorf("snapshot has %d bytes, declared %d", size, u.size)
	}
	digest := "sha256:" + hex.EncodeToString(hash.Sum(nil))
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
	if _, err := tx.ExecContext(ctx, `INSERT INTO spin_object_refs(ref, object_id) VALUES(?, ?)
		ON CONFLICT(ref) DO UPDATE SET object_id = excluded.object_id`, ref, objectID); err != nil {
		_ = tx.Rollback()
		return BlobInfo{}, err
	}
	if err := tx.Commit(); err != nil {
		return BlobInfo{}, err
	}
	u.completed = true
	return BlobInfo{Ref: ref, Digest: digest, Kind: "docker-snapshot", Size: size}, nil
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
