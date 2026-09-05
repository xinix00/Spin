package persistence

// SQLite writes a database one page at a time, 4 KiB by default. On HopOS every
// write is one synchronous system call over the volume ABI, so a 1 MiB blob
// insert cost about five hundred round trips: the pages into the journal, the
// same pages into the database, plus the journal's own bookkeeping. The disk
// and the ABI move a megabyte per call at full speed; the granularity was the
// problem. writeCoalescer joins consecutive writes into one run of up to limit
// bytes and lets the file decide when that run must reach storage: at Sync,
// when a lock is released, before a read or size query that would see it, and
// at Close.

type chunkWriter func(offset int64, data []byte) error

type writeCoalescer struct {
	limit   int
	write   chunkWriter
	pending []byte
	offset  int64
}

func newWriteCoalescer(limit int, write chunkWriter) *writeCoalescer {
	if limit <= 0 {
		limit = 1
	}
	return &writeCoalescer{limit: limit, write: write}
}

// Write records p at offset. A write that continues the pending run is
// appended; anything else flushes the run first. Writes at least as large as
// the limit go straight through, in limit-sized pieces.
func (c *writeCoalescer) Write(offset int64, p []byte) error {
	if len(p) == 0 {
		return nil
	}
	if len(c.pending) > 0 && (offset != c.offset+int64(len(c.pending)) || len(c.pending)+len(p) > c.limit) {
		if err := c.Flush(); err != nil {
			return err
		}
	}
	if len(c.pending) == 0 && len(p) >= c.limit {
		for len(p) > 0 {
			count := min(len(p), c.limit)
			if err := c.write(offset, p[:count]); err != nil {
				return err
			}
			offset += int64(count)
			p = p[count:]
		}
		return nil
	}
	if len(c.pending) == 0 {
		c.offset = offset
		if cap(c.pending) < c.limit {
			c.pending = make([]byte, 0, c.limit)
		}
	}
	c.pending = append(c.pending, p...)
	return nil
}

// Flush sends the pending run to storage.
func (c *writeCoalescer) Flush() error {
	if len(c.pending) == 0 {
		return nil
	}
	err := c.write(c.offset, c.pending)
	c.pending = c.pending[:0]
	return err
}

// Overlaps reports whether a read of length at offset would touch pending bytes.
func (c *writeCoalescer) Overlaps(offset, length int64) bool {
	if len(c.pending) == 0 || length <= 0 {
		return false
	}
	return offset < c.offset+int64(len(c.pending)) && offset+length > c.offset
}

// End reports the byte just past the pending run, or 0 when nothing is pending.
func (c *writeCoalescer) End() int64 {
	if len(c.pending) == 0 {
		return 0
	}
	return c.offset + int64(len(c.pending))
}

// Discard drops pending bytes at or beyond size; a truncate makes them moot.
func (c *writeCoalescer) Discard(size int64) {
	if len(c.pending) == 0 {
		return
	}
	if size <= c.offset {
		c.pending = c.pending[:0]
		return
	}
	if keep := size - c.offset; keep < int64(len(c.pending)) {
		c.pending = c.pending[:keep]
	}
}
