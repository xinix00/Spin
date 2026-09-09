package replica

import (
	"context"
	"errors"
	"fmt"
	"io"
	"runtime"
	"time"
)

// restoreInto downloads and validates into a scratch file first. Publication
// uses a durable intent because a generic SQLite VFS has no atomic rename.
// Prepare retries publication whenever this intent survives a process crash.
func (r *Replica) restoreInto(ctx context.Context, generation string, at time.Time, path string) (marker, error) {
	r.restoreMu.Lock()
	defer r.restoreMu.Unlock()
	l, err := r.loadLayout(ctx, generation)
	if err != nil {
		return marker{}, err
	}
	plan, err := l.plan(at)
	if err != nil {
		return marker{}, err
	}
	temporary := path + ".replica-restore-data"
	file, err := r.files.Open(temporary, true)
	if err != nil {
		return marker{}, err
	}
	defer func() { file.Close(); r.files.Remove(temporary) }()
	if err := file.Truncate(0); err != nil {
		return marker{}, err
	}
	result := marker{Version: formatVersion, Generation: generation, Complete: true, Clean: true, StartedAt: generationTime(generation)}
	pageSize := 0
	for _, m := range plan {
		// A merged shrink-then-grow must clear truncated pages from the baseline.
		if err := file.Truncate(m.MinSize); err != nil {
			return marker{}, err
		}
		for _, ref := range m.Parts {
			seg, err := r.readPart(ctx, ref)
			if err != nil {
				return marker{}, err
			}
			if m.MinSize > seg.DBSize || m.MinSize%int64(seg.PageSize) != 0 {
				return marker{}, errors.New("invalid manifest truncation size")
			}
			if pageSize != 0 && pageSize != seg.PageSize {
				return marker{}, errors.New("page size changed during restore")
			}
			pageSize = seg.PageSize
			for index, page := range seg.Pages {
				if err := writeAt(file, seg.Data[index], int64(page-1)*int64(pageSize)); err != nil {
					return marker{}, err
				}
			}
			if err := file.Truncate(seg.DBSize); err != nil {
				return marker{}, err
			}
			result.Size = seg.DBSize
			result.PageSize = seg.PageSize
			result.Bytes += ref.Size
		}
		result.Seq = m.Seq
		result.At = m.At
	}
	// Preserve the compaction frontier when continuing an idle generation.
	for _, windows := range l.windows {
		for _, w := range windows {
			if w.end.After(result.SealedAt) {
				result.SealedAt = w.end
			}
		}
	}
	if err := file.Sync(); err != nil {
		return marker{}, err
	}
	intent := path + ".replica-restoring"
	if err := r.writeLocal(intent, []byte(generation)); err != nil {
		return marker{}, err
	}
	destination, err := r.files.Open(path, true)
	if err != nil {
		return marker{}, err
	}
	defer destination.Close()
	buf := make([]byte, 1<<20)
	for offset := int64(0); offset < result.Size; {
		if err := ctx.Err(); err != nil {
			return marker{}, err
		}
		count := int(min(int64(len(buf)), result.Size-offset))
		n, err := file.ReadAt(buf[:count], offset)
		if err != nil {
			return marker{}, err
		}
		if n != count {
			return marker{}, io.ErrUnexpectedEOF
		}
		if err := writeAt(destination, buf[:count], offset); err != nil {
			return marker{}, err
		}
		offset += int64(count)
		runtime.Gosched()
	}
	if err := destination.Truncate(result.Size); err != nil {
		return marker{}, err
	}
	if err := destination.Sync(); err != nil {
		return marker{}, err
	}
	if err := destination.Close(); err != nil {
		return marker{}, err
	}
	if err := r.files.Remove(intent); err != nil {
		return marker{}, fmt.Errorf("finish restore: %w", err)
	}
	return result, nil
}

func writeAt(file File, data []byte, offset int64) error {
	n, err := file.WriteAt(data, offset)
	if err == nil && n != len(data) {
		err = io.ErrShortWrite
	}
	return err
}

func (r *Replica) writeLocal(path string, data []byte) error {
	file, err := r.files.Open(path, true)
	if err != nil {
		return err
	}
	defer file.Close()
	if err := writeAt(file, data, 0); err != nil {
		return err
	}
	if err := file.Truncate(int64(len(data))); err != nil {
		return err
	}
	if err := file.Sync(); err != nil {
		return err
	}
	return file.Close()
}
