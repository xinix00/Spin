package replica

import (
	"context"
	"errors"
	"fmt"
	"io"
	"strings"
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
	// Which pages this generation ever shipped, so a chain that never held
	// them is caught here instead of by SQLite (coverage.go).
	shipped := pageSet{}
	var refs []partRef
	var total int64
	for _, m := range plan {
		refs = append(refs, m.Parts...)
		for _, ref := range m.Parts {
			total += ref.Size
		}
	}
	next, stop := r.prefetchParts(ctx, refs, restorePrefetch)
	defer stop()
	progress := restoreProgress{r: r, total: total, started: time.Now()}
	for _, m := range plan {
		// A merged shrink-then-grow must clear truncated pages from the baseline.
		if err := file.Truncate(m.MinSize); err != nil {
			return marker{}, err
		}
		for _, ref := range m.Parts {
			seg, err := next()
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
				shipped.add(page)
			}
			if err := file.Truncate(seg.DBSize); err != nil {
				return marker{}, err
			}
			result.Size = seg.DBSize
			result.PageSize = seg.PageSize
			result.Bytes += ref.Size
			progress.add(ref.Size)
		}
		result.Seq = m.Seq
		result.At = m.At
	}
	// Before anything is published: did this generation ever hold every page
	// the size claims? A page it never carried is a hole, which is what SQLite
	// calls malformed, and by then the cause is invisible. Saying it here
	// keeps the old database in place and names the generation that is beyond
	// repair. A page that was shipped and later truncated away is fine: the
	// source has the same zero there (coverage.go).
	if first, missing, ok := shipped.shortfall(result.Size, pageSize); !ok {
		return marker{}, &errShortReplica{Generation: generation, Size: result.Size, PageSize: pageSize, First: first, Missing: missing}
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
	// And the last gate: does SQLite agree that this is a database? The
	// coverage check above proves every page arrived; this proves they compose
	// into something openable, before it replaces a database that may still be
	// serving (replica.go, Options.Verify).
	if r.verify != nil {
		if err := file.Close(); err != nil {
			return marker{}, err
		}
		if err := r.verify(temporary); err != nil {
			return marker{}, fmt.Errorf("verify generation %s: %w", generation, err)
		}
		if file, err = r.files.Open(temporary, false); err != nil {
			return marker{}, err
		}
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

// restorePrefetch is how many parts a restore fetches ahead of the one it
// applies: the parts go in order, the network does not have to.
const restorePrefetch = 4

// prefetchParts fetches parts ahead, at most ahead of them unapplied, and
// hands them out in order.
func (r *Replica) prefetchParts(ctx context.Context, refs []partRef, ahead int) (next func() (segment, error), stop func()) {
	type fetched struct {
		seg segment
		err error
	}
	ctx, cancel := context.WithCancel(ctx)
	results := make([]chan fetched, len(refs))
	for index := range results {
		results[index] = make(chan fetched, 1)
	}
	slots := make(chan struct{}, ahead)
	go func() {
		for index, ref := range refs {
			select {
			case slots <- struct{}{}:
			case <-ctx.Done():
				return
			}
			go func() {
				seg, err := r.readPart(ctx, ref)
				results[index] <- fetched{seg, err}
			}()
		}
	}()
	position := 0
	next = func() (segment, error) {
		if position >= len(results) {
			return segment{}, errors.New("restore asked for more parts than the plan has")
		}
		select {
		case got := <-results[position]:
			position++
			<-slots
			return got.seg, got.err
		case <-ctx.Done():
			return segment{}, ctx.Err()
		}
	}
	return next, cancel
}

// restoreProgress says how far a restore has come, to the page that waits
// for it (Progress).
type restoreProgress struct {
	r       *Replica
	total   int64
	done    int64
	started time.Time
	told    time.Time
}

func (p *restoreProgress) add(bytes int64) {
	p.done += bytes
	if time.Since(p.told) < 2*time.Second && p.done < p.total {
		return
	}
	p.told = time.Now()
	if p.r.Progress == nil || p.total <= 0 {
		return
	}
	gib := func(value int64) string {
		return strings.Replace(fmt.Sprintf("%.1f GiB", float64(value)/(1<<30)), ".", ",", 1)
	}
	message := fmt.Sprintf("Database uit de replica halen · %s van %s (%d%%)", gib(p.done), gib(p.total), p.done*100/p.total)
	if spent := time.Since(p.started).Seconds(); spent > 1 {
		rate := float64(p.done) / spent
		message += fmt.Sprintf(" · %.0f MiB/s", rate/(1<<20))
		if rate > 0 && p.done < p.total {
			message += fmt.Sprintf(" · nog ~%d min", int(float64(p.total-p.done)/rate/60)+1)
		}
	}
	p.r.Progress(message)
}

func writeAt(file File, data []byte, offset int64) error {
	n, err := file.WriteAt(data, offset)
	if err == nil && n != len(data) {
		err = io.ErrShortWrite
	}
	return err
}

func (r *Replica) writeLocal(path string, data []byte) error {
	return writeLocalFile(r.files, path, data)
}

// writeLocalFile replaces a small local file durably.
func writeLocalFile(files Storage, path string, data []byte) error {
	file, err := files.Open(path, true)
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
