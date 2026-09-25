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
	minSize  int64
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

// capture spools the pages to ship. The pages go in segments, each read under its
// own short read transaction so queries run in between, and the processor
// is given up after every segment (on HopOS nothing preempts a goroutine
// that never blocks). Pages written while the copy ran are read again at
// the end, in bounded segments under one final transaction with the check
// that finds them. That transaction is the capture boundary: writes after it
// stay pending for the next sync. Waiting for a later empty round would never
// finish on a busy database. The final transaction may pause writers while it
// reconciles pages changed during the copy, but memory stays segment-bounded.
//
// Network I/O starts only after the spool is complete. The scratch name is
// reused, so a process crash cannot leak a spool per attempt.
func (r *Replica) capture(ctx context.Context, db Database, all bool) (capture, error) {
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
	c.minSize = size
	return c, all, nil
}

// spoolWriter appends encoded segments to the spool.
type spoolWriter struct {
	file     File
	position int64
}

func (w *spoolWriter) write(c *capture, seg segment) error {
	c.size = seg.DBSize
	c.minSize = min(c.minSize, seg.DBSize)
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
				if r.tracker.currentPageSize() != c.pageSize {
					return errors.New("replica: page size changed during capture; retry with a fresh snapshot")
				}
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
			r.rememberSource(seg) // the witness the next sync checks (guard.go)
			if err := spool.write(&c, seg); err != nil {
				spool.file.Close()
				return err
			}
			if c.snapshot {
				r.reportCopy("read", int64(min(offset+limit, len(c.pages)))*int64(c.pageSize), int64(len(c.pages))*int64(c.pageSize))
			}
			runtime.Gosched()
			if len(c.pages) == 0 {
				break
			}
		}
		// Reconcile every page changed during the copy at one consistent
		// point, without materializing the entire dirty database in memory.
		if err := db.WithReadTransaction(ctx, func() error {
			if r.tracker.currentPageSize() != c.pageSize {
				return errors.New("replica: page size changed during capture; retry with a fresh snapshot")
			}
			again := r.tracker.take(int(^uint(0) >> 1))
			c.revision = r.tracker.version()
			// Include even unread pages so an I/O error retries the whole set.
			c.pages = append(c.pages, again...)
			for offset := 0; offset < len(again); offset += limit {
				if err := ctx.Err(); err != nil {
					return err
				}
				seg, err := r.readPages(c.pageSize, again[offset:min(offset+limit, len(again))])
				if err != nil {
					return err
				}
				r.rememberSource(seg)
				if err := spool.write(&c, seg); err != nil {
					return err
				}
			}
			c.at = r.now().UTC()
			return nil
		}); err != nil {
			spool.file.Close()
			return err
		}
		if err := spool.finish(); err != nil {
			return err
		}
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
