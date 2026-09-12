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

// capture spools the pages to ship. Two ways, one result.
//
// Blocking, while a Spin opens: the whole dirty set under one read
// transaction. Nothing else touches the database yet, so holding it costs
// nothing, and the copy is consistent by construction.
//
// Live, while a Spin serves: the pages go in segments, each read under its
// own short read transaction so queries run in between, and the processor
// is given up after every segment (on HopOS nothing preempts a goroutine
// that never blocks). Pages written while the copy ran are read again at
// the end, under one transaction with the check that finds them, until a
// round finds none: the segments together then equal the database as it
// was in that last transaction. This is how a generation renews itself in
// the background without the database going away for minutes.
//
// Network I/O starts only after the spool is complete. The scratch name is
// reused, so a process crash cannot leak a spool per attempt.
func (r *Replica) capture(ctx context.Context, db Database, all, blocking bool) (capture, error) {
	if blocking {
		return r.captureBlocking(ctx, db, all)
	}
	return r.captureLive(ctx, db, all)
}

// captureBegin takes the dirty set and the shape of the database.
func (r *Replica) captureBegin(all bool) (capture, bool, error) {
	var c capture
	pageSize := r.tracker.currentPageSize()
	if pageSize == 0 {
		return c, false, nil
	}
	size, err := r.fileSize()
	if err != nil {
		return c, false, err
	}
	if size%int64(pageSize) != 0 {
		return c, false, errors.New("database size is not page aligned")
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
	return c, all, nil
}

// spoolWriter appends encoded segments to the spool.
type spoolWriter struct {
	file     File
	position int64
}

func (w *spoolWriter) write(c *capture, seg segment) error {
	if seg.DBSize < c.size {
		c.size = seg.DBSize
	}
	data := encodeSegment(seg)
	if err := writeAt(w.file, data, w.position); err != nil {
		return err
	}
	c.parts = append(c.parts, spoolPart{offset: w.position, length: len(data)})
	w.position += int64(len(data))
	return nil
}

func (r *Replica) openSpool(c *capture) (*spoolWriter, error) {
	c.path = r.path + ".replica-spool"
	file, err := r.files.Open(c.path, true)
	if err != nil {
		return nil, err
	}
	if err := file.Truncate(0); err != nil {
		file.Close()
		return nil, err
	}
	return &spoolWriter{file: file}, nil
}

func (w *spoolWriter) finish() error {
	if err := w.file.Sync(); err != nil {
		w.file.Close()
		return err
	}
	return w.file.Close()
}

func (r *Replica) captureBlocking(ctx context.Context, db Database, all bool) (capture, error) {
	var c capture
	err := db.WithReadTransaction(ctx, func() error {
		var err error
		c, all, err = r.captureBegin(all)
		if err != nil || c.pageSize == 0 || (len(c.pages) == 0 && !all) {
			return err
		}
		spool, err := r.openSpool(&c)
		if err != nil {
			return err
		}
		limit := max(1, r.config.SegmentBytes/c.pageSize)
		for offset := 0; offset < len(c.pages) || offset == 0; offset += limit {
			if err := ctx.Err(); err != nil {
				spool.file.Close()
				return err
			}
			seg, err := r.readPages(c.pageSize, c.pages[offset:min(offset+limit, len(c.pages))])
			if err != nil {
				spool.file.Close()
				return err
			}
			if err := spool.write(&c, seg); err != nil {
				spool.file.Close()
				return err
			}
			if len(c.pages) == 0 {
				break
			}
		}
		if err := spool.finish(); err != nil {
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

func (r *Replica) captureLive(ctx context.Context, db Database, all bool) (capture, error) {
	var c capture
	err := db.WithReadTransaction(ctx, func() error {
		var err error
		c, all, err = r.captureBegin(all)
		return err
	})
	if err != nil {
		r.tracker.putBack(c.pages)
		return c, err
	}
	if c.pageSize == 0 || (len(c.pages) == 0 && !all) {
		return c, nil
	}
	err = func() error {
		spool, err := r.openSpool(&c)
		if err != nil {
			return err
		}
		limit := max(1, r.config.SegmentBytes/c.pageSize)
		readSegment := func(pages []uint32) (segment, error) {
			var seg segment
			err := db.WithReadTransaction(ctx, func() error {
				var err error
				seg, err = r.readPages(c.pageSize, pages)
				return err
			})
			return seg, err
		}
		for offset := 0; offset < len(c.pages) || offset == 0; offset += limit {
			if err := ctx.Err(); err != nil {
				spool.file.Close()
				return err
			}
			seg, err := readSegment(c.pages[offset:min(offset+limit, len(c.pages))])
			if err != nil {
				spool.file.Close()
				return err
			}
			if err := spool.write(&c, seg); err != nil {
				spool.file.Close()
				return err
			}
			runtime.Gosched()
			if len(c.pages) == 0 {
				break
			}
		}
		// Pages written while the copy ran were read too early: read them
		// again, until a round finds none. The take and the read share one
		// transaction, so nothing slips between them.
		for {
			if err := ctx.Err(); err != nil {
				spool.file.Close()
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
				spool.file.Close()
				return err
			}
			if len(again) == 0 {
				break
			}
			c.pages = append(c.pages, again...)
			if err := spool.write(&c, seg); err != nil {
				spool.file.Close()
				return err
			}
			runtime.Gosched()
		}
		if err := spool.finish(); err != nil {
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
