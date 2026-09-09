package replica

import (
	"context"
	"errors"
	"io"
	"runtime"
	"time"
)

type spoolPart struct {
	offset int64
	length int
}
type capture struct {
	pages    []uint32
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

// capture spools the pages to ship. A snapshot copies the whole database,
// and that must not hold the database or the processor: the pages go in
// segments, each read under its own short read transaction so queries run
// in between, and the processor is given up after every segment (on HopOS
// nothing preempts a goroutine that never blocks). Pages written while the
// copy ran are read again at the end, under one transaction with the
// check that finds them, until a round finds none: the segments together
// then equal the database as it was in that last transaction. Network I/O
// starts only after the spool is complete. The scratch name is reused, so
// a process crash cannot leak a spool per attempt.
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
		return nil
	})
	if err != nil {
		r.tracker.putBack(c.pages)
		return c, err
	}
	if c.pageSize == 0 || (len(c.pages) == 0 && !all) {
		return c, nil
	}
	c.path = r.path + ".replica-spool"
	err = func() error {
		spool, err := r.files.Open(c.path, true)
		if err != nil {
			return err
		}
		defer spool.Close()
		if err := spool.Truncate(0); err != nil {
			return err
		}
		limit := max(1, r.config.SegmentBytes/c.pageSize)
		var position int64
		spoolSegment := func(pages []uint32) error {
			var seg segment
			if err := db.WithReadTransaction(ctx, func() error {
				var err error
				seg, err = r.readPages(c.pageSize, pages)
				return err
			}); err != nil {
				return err
			}
			if seg.DBSize < c.size {
				c.size = seg.DBSize
			}
			data := encodeSegment(seg)
			if err := writeAt(spool, data, position); err != nil {
				return err
			}
			c.parts = append(c.parts, spoolPart{offset: position, length: len(data)})
			position += int64(len(data))
			runtime.Gosched()
			return nil
		}
		for offset := 0; offset < len(c.pages) || offset == 0; offset += limit {
			if err := ctx.Err(); err != nil {
				return err
			}
			if err := spoolSegment(c.pages[offset:min(offset+limit, len(c.pages))]); err != nil {
				return err
			}
			if len(c.pages) == 0 {
				break
			}
		}
		for {
			if err := ctx.Err(); err != nil {
				return err
			}
			var again []uint32
			var seg segment
			if err := db.WithReadTransaction(ctx, func() error {
				again = r.tracker.take(int(^uint(0) >> 1))
				c.revision = r.tracker.version()
				if len(again) == 0 {
					return nil
				}
				var err error
				seg, err = r.readPages(c.pageSize, again)
				return err
			}); err != nil {
				r.tracker.putBack(again)
				return err
			}
			if len(again) == 0 {
				break
			}
			c.pages = append(c.pages, again...)
			if seg.DBSize < c.size {
				c.size = seg.DBSize
			}
			data := encodeSegment(seg)
			if err := writeAt(spool, data, position); err != nil {
				return err
			}
			c.parts = append(c.parts, spoolPart{offset: position, length: len(data)})
			position += int64(len(data))
			runtime.Gosched()
		}
		if err := spool.Sync(); err != nil {
			return err
		}
		if err := spool.Close(); err != nil {
			return err
		}
		c.at = r.now().UTC()
		return nil
	}()
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
