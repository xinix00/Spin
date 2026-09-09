package replica

import (
	"context"
	"errors"
	"io"
	"time"
)

type spoolPart struct {
	offset int64
	length int
}
type capture struct {
	pages []uint32
	// shipped lists the pages that were read, in segment order, with
	// their hashes for the index.
	shipped  []uint32
	hashes   []pageHash
	parts    []spoolPart
	path     string
	size     int64
	revision uint64
	pageSize int
	snapshot bool
	at       time.Time
}

func (c capture) close(files Storage) {
	if c.path != "" {
		_ = files.Remove(c.path)
	}
}

// capture spools the entire dirty set under one database read transaction:
// nothing else touches the database until the spool is complete, which is
// why a full snapshot runs while a Spin opens, not while it serves. Network
// I/O starts only after the transaction releases the writer. The scratch
// name is reused, so a process crash cannot leak a spool per attempt.
func (r *Replica) capture(ctx context.Context, db Database, all bool) (capture, error) {
	var c capture
	err := db.WithReadTransaction(ctx, func() error {
		pageSize := r.tracker.currentPageSize()
		if pageSize == 0 {
			return nil
		}
		size, err := r.fileSize()
		if err != nil {
			return err
		}
		if size%int64(pageSize) != 0 {
			return errors.New("database size is not page aligned")
		}
		c.pageSize = pageSize
		if all || r.getMarker().PageSize != pageSize {
			all = true
			c.snapshot = true
			r.tracker.markAll(size)
		}
		c.pages = r.tracker.take(int(^uint(0) >> 1))
		c.revision = r.tracker.version()
		c.size = size
		if len(c.pages) == 0 && !all {
			return nil
		}
		c.path = r.path + ".replica-spool"
		spool, err := r.files.Open(c.path, true)
		if err != nil {
			return err
		}
		defer spool.Close()
		if err := spool.Truncate(0); err != nil {
			return err
		}
		limit := max(1, r.config.SegmentBytes/pageSize)
		var position int64
		for offset := 0; offset < len(c.pages) || offset == 0; offset += limit {
			if err := ctx.Err(); err != nil {
				return err
			}
			seg, err := r.readPages(pageSize, c.pages[offset:min(offset+limit, len(c.pages))])
			if err != nil {
				return err
			}
			c.shipped = append(c.shipped, seg.Pages...)
			c.hashes = append(c.hashes, seg.hashes()...)
			data := encodeSegment(seg)
			if err := writeAt(spool, data, position); err != nil {
				return err
			}
			c.parts = append(c.parts, spoolPart{offset: position, length: len(data)})
			position += int64(len(data))
			if len(c.pages) == 0 {
				break
			}
		}
		if err := spool.Sync(); err != nil {
			return err
		}
		if err := spool.Close(); err != nil {
			return err
		}
		c.at = r.now().UTC()
		return nil
	})
	if err != nil {
		r.tracker.putBack(c.pages)
		c.close(r.files)
	}
	return c, err
}

func (r *Replica) readSpool(path string, part spoolPart) ([]byte, error) {
	file, err := r.files.Open(path, false)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	data := make([]byte, part.length)
	n, err := file.ReadAt(data, part.offset)
	if err == nil && n != len(data) {
		err = io.ErrUnexpectedEOF
	}
	return data, err
}
