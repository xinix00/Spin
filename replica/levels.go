package replica

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"slices"
	"sort"
	"strings"
	"time"
)

type rawFile struct {
	key string
	seq int64
	at  time.Time
	manifest
}

type window struct {
	key        string
	level      int
	start, end time.Time
	parts      []partRef
	manifest
}

type layout struct {
	generation string
	raw        []rawFile
	windows    map[int][]window
	snapshot   manifest
}

func (r *Replica) rawKey(generation string, seq int64, at time.Time) string {
	return fmt.Sprintf("%sL0/%012d-%020d.json", r.generationPrefix(generation), seq, at.UnixNano())
}
func (r *Replica) windowPrefix(generation string, level int, start, end time.Time) string {
	return fmt.Sprintf("%sL%d/%010d-%010d/", r.generationPrefix(generation), level, start.Unix(), end.Unix())
}
func (r *Replica) snapshotKey(generation string) string {
	return r.generationPrefix(generation) + "snapshot"
}

// Listing ignores uncommitted parts, including leftovers from failed attempts.
func (r *Replica) loadLayout(ctx context.Context, generation string) (*layout, error) {
	prefix := r.generationPrefix(generation)
	objects, err := r.s3.List(ctx, prefix)
	if err != nil {
		return nil, err
	}
	l := &layout{generation: generation, windows: map[int][]window{}}
	seqs := map[int64]bool{}
	for _, object := range objects {
		rest := strings.TrimPrefix(object.Key, prefix)
		if rest != "snapshot" && !(strings.HasPrefix(rest, "L0/") && strings.HasSuffix(rest, ".json")) && !(strings.HasPrefix(rest, "L") && strings.HasSuffix(rest, "/complete")) {
			continue
		}
		m, err := r.getManifest(ctx, generation, object.Key)
		if err != nil {
			return nil, err
		}
		switch {
		case rest == "snapshot":
			if m.Level != 0 || m.FirstSeq != 1 || m.Seq != 1 {
				return nil, errors.New("invalid snapshot manifest")
			}
			l.snapshot = m
		case strings.HasPrefix(rest, "L0/"):
			if m.Level != 0 || m.FirstSeq != m.Seq || m.Seq <= 1 || seqs[m.Seq] {
				return nil, errors.New("invalid or duplicate raw commit")
			}
			seqs[m.Seq] = true
			l.raw = append(l.raw, rawFile{key: object.Key, seq: m.Seq, at: m.At, manifest: m})
		default:
			if m.Level < 1 || m.FirstSeq <= 1 {
				return nil, errors.New("invalid window manifest")
			}
			l.windows[m.Level] = append(l.windows[m.Level], window{key: object.Key, level: m.Level, start: m.Start, end: m.End, parts: m.Parts, manifest: m})
		}
	}
	sort.Slice(l.raw, func(i, j int) bool { return l.raw[i].seq < l.raw[j].seq })
	for level := range l.windows {
		sort.Slice(l.windows[level], func(i, j int) bool { return l.windows[level][i].start.Before(l.windows[level][j].start) })
	}
	return l, nil
}

func (r *Replica) compact(ctx context.Context, generation string) error {
	r.archiveMu.Lock()
	defer r.archiveMu.Unlock()
	now := r.now()
	for level := 1; level <= len(r.config.Schedule); level++ {
		l, err := r.loadLayout(ctx, generation)
		if err != nil {
			return err
		}
		for _, span := range r.elapsedWindows(l, level, now) {
			inputs := l.inputs(level, span[0], span[1])
			if len(inputs) == 0 {
				continue
			}
			if err := r.mergeWindow(ctx, generation, level, span[0], span[1], inputs); err != nil {
				return err
			}
		}
	}
	l, err := r.loadLayout(ctx, generation)
	if err != nil {
		return err
	}
	return r.expire(ctx, l, now)
}

func (r *Replica) elapsedWindows(l *layout, level int, now time.Time) [][2]time.Time {
	size := r.config.Schedule[level-1].Window
	merged := map[int64]bool{}
	for _, w := range l.windows[level] {
		merged[w.start.Unix()] = true
	}
	candidates := map[int64]time.Time{}
	// A window is (start, end]: a commit exactly on a boundary belongs to
	// the window that ends there, the same rule plan uses for a point at
	// that boundary, so the point does not change once the window exists.
	note := func(at time.Time) {
		start := at.Truncate(size)
		if at.Equal(start) {
			start = start.Add(-size)
		}
		if !start.Add(size).After(now) && !merged[start.Unix()] {
			candidates[start.Unix()] = start
		}
	}
	if level == 1 {
		for _, raw := range l.raw {
			note(raw.at)
		}
	} else {
		for _, w := range l.windows[level-1] {
			note(w.start)
		}
	}
	var spans [][2]time.Time
	for _, start := range candidates {
		spans = append(spans, [2]time.Time{start, start.Add(size)})
	}
	sort.Slice(spans, func(i, j int) bool { return spans[i][0].Before(spans[j][0]) })
	return spans
}

func (l *layout) inputs(level int, start, end time.Time) []manifest {
	var inputs []manifest
	if level == 1 {
		for _, raw := range l.raw {
			if raw.at.After(start) && !raw.at.After(end) {
				inputs = append(inputs, raw.manifest)
			}
		}
	} else {
		for _, w := range l.windows[level-1] {
			if !w.start.Before(start) && !w.end.After(end) {
				inputs = append(inputs, w.manifest)
			}
		}
		// A lower window that ends exactly at this window's start belongs
		// to the window before this one.
		inputs = slices.DeleteFunc(inputs, func(m manifest) bool { return !m.End.After(start) })
	}
	return inputs
}

// Merge retains the last page state and last database size. Every attempt uses
// new part keys; the single manifest PUT publishes all parts atomically.
func (r *Replica) mergeWindow(ctx context.Context, generation string, level int, start, end time.Time, inputs []manifest) error {
	if len(inputs) == 0 {
		return nil
	}
	var refs []partRef
	seq := inputs[0].FirstSeq - 1
	for _, m := range inputs {
		if m.FirstSeq != seq+1 {
			return errors.New("cannot merge a gap in commit sequence")
		}
		seq = m.Seq
		refs = append(refs, m.Parts...)
	}
	winner := map[uint32]int{}
	pageSize, dbSize := 0, int64(0)
	index := 0
	for _, input := range inputs {
		// Honor truncation history at every tier, including a finer window that
		// already merged a shrink followed by growth.
		if pageSize > 0 && input.MinSize < dbSize {
			for page := range winner {
				if int64(page)*int64(pageSize) > input.MinSize {
					delete(winner, page)
				}
			}
		}
		var inputSize int64 = -1
		for _, ref := range input.Parts {
			seg, err := r.readPart(ctx, ref)
			if err != nil {
				return err
			}
			if pageSize != 0 && pageSize != seg.PageSize {
				return errors.New("page size changed within generation")
			}
			if input.MinSize > seg.DBSize || input.MinSize%int64(seg.PageSize) != 0 || (inputSize >= 0 && inputSize != seg.DBSize) {
				return errors.New("inconsistent database size in manifest")
			}
			for _, page := range seg.Pages {
				winner[page] = index
			}
			pageSize, dbSize = seg.PageSize, seg.DBSize
			inputSize = seg.DBSize
			index++
		}
	}
	m := manifest{MinSize: inputs[0].MinSize, Version: formatVersion, FirstSeq: inputs[0].FirstSeq, Seq: seq, At: inputs[len(inputs)-1].At, Level: level, Start: start, End: end}
	for _, input := range inputs {
		m.MinSize = min(m.MinSize, input.MinSize)
	}
	prefix := r.generationPrefix(generation) + "data/" + newGenerationID(r.now()) + "/"
	out := segment{PageSize: pageSize, DBSize: dbSize}
	flush := func() error {
		ref, err := r.putPart(ctx, fmt.Sprintf("%s%06d.seg", prefix, len(m.Parts)+1), out)
		if err != nil {
			return err
		}
		m.Parts = append(m.Parts, ref)
		out = segment{PageSize: pageSize, DBSize: dbSize}
		return nil
	}
	for index, ref := range refs {
		seg, err := r.readPart(ctx, ref)
		if err != nil {
			return err
		}
		for position, page := range seg.Pages {
			win, ok := winner[page]
			if !ok || win != index {
				continue
			}
			out.Pages = append(out.Pages, page)
			out.Data = append(out.Data, bytes.Clone(seg.Data[position]))
			if len(out.Pages)*pageSize >= r.config.SegmentBytes {
				if err := flush(); err != nil {
					return err
				}
			}
		}
	}
	if len(out.Pages) > 0 || len(m.Parts) == 0 {
		if err := flush(); err != nil {
			return err
		}
	}
	return r.putManifest(ctx, r.windowPrefix(generation, level, start, end)+"complete", m)
}

func (r *Replica) expire(ctx context.Context, l *layout, now time.Time) error {
	covered := func(level int, m manifest) bool {
		for _, w := range l.windows[level+1] {
			if w.FirstSeq <= m.FirstSeq && w.Seq >= m.Seq {
				return true
			}
		}
		return false
	}
	remove := func(key string, m manifest) error {
		// Remove visibility first. An interrupted deletion leaves only orphan data;
		// a reader can always use the already committed coarser window.
		if err := r.s3.Delete(ctx, key); err != nil {
			return err
		}
		for _, part := range m.Parts {
			if err := r.s3.Delete(ctx, part.Key); err != nil {
				return err
			}
		}
		return nil
	}
	for _, raw := range l.raw {
		if raw.at.Before(now.Add(-r.config.Schedule[0].Window)) && covered(0, raw.manifest) {
			if err := remove(raw.key, raw.manifest); err != nil {
				return err
			}
		}
	}
	for level := 1; level < len(r.config.Schedule); level++ {
		for _, w := range l.windows[level] {
			if w.end.Before(now.Add(-r.config.Schedule[level-1].Keep)) && covered(level, w.manifest) {
				if err := remove(w.key, w.manifest); err != nil {
					return err
				}
			}
		}
	}
	return nil
}

// Point identifies a fully committed, still reconstructible database state.
type Point struct {
	Generation string    `json:"generation"`
	At         time.Time `json:"at"`
	Level      int       `json:"level"`
	Current    bool      `json:"current"`
}

func (r *Replica) Points(ctx context.Context) ([]Point, error) {
	r.archiveMu.RLock()
	defer r.archiveMu.RUnlock()
	generations, err := r.generationIDs(ctx)
	if err != nil {
		return nil, err
	}
	current := r.getMarker()
	var points []Point
	for _, generation := range generations {
		l, err := r.loadLayout(ctx, generation)
		if errors.Is(err, ErrLegacyFormat) {
			continue
		}
		if err != nil {
			return nil, err
		}
		if l.snapshot.At.IsZero() {
			continue
		}
		seen := map[time.Time]bool{}
		add := func(at time.Time, level int) {
			if seen[at] {
				return
			}
			if _, err := l.plan(at); err != nil {
				return
			}
			seen[at] = true
			points = append(points, Point{Generation: generation, At: at, Level: level, Current: generation == current.Generation && current.Complete})
		}
		add(l.snapshot.At, -1)
		for level, windows := range l.windows {
			for _, w := range windows {
				add(w.end, level)
			}
		}
		for _, raw := range l.raw {
			add(raw.at, 0)
		}
	}
	sort.Slice(points, func(i, j int) bool {
		if !points[i].At.Equal(points[j].At) {
			return points[i].At.After(points[j].At)
		}
		if points[i].Current != points[j].Current {
			return points[i].Current
		}
		return points[i].Generation > points[j].Generation
	})
	return points, nil
}

func (r *Replica) generationIDs(ctx context.Context) ([]string, error) {
	objects, err := r.s3.List(ctx, r.key("generations")+"/")
	if err != nil {
		return nil, err
	}
	seen := map[string]bool{}
	var ids []string
	for _, object := range objects {
		rest := strings.TrimPrefix(object.Key, r.key("generations")+"/")
		id, _, ok := strings.Cut(rest, "/")
		if ok && validGeneration(id) && !seen[id] {
			seen[id] = true
			ids = append(ids, id)
		}
	}
	sort.Strings(ids)
	return ids, nil
}

// plan starts at the permanent snapshot and follows sequence coverage. It
// refuses gaps, including requests for fine points that have already expired.
func (l *layout) plan(at time.Time) ([]manifest, error) {
	if l.snapshot.At.IsZero() {
		return nil, errors.New("generation has no committed snapshot")
	}
	latest := at.IsZero()
	if !latest && at.Before(l.snapshot.At) {
		return nil, errors.New("requested time precedes snapshot")
	}
	target := l.snapshot.Seq
	for _, windows := range l.windows {
		for _, w := range windows {
			if (latest || !w.end.After(at)) && w.Seq > target {
				target = w.Seq
			}
		}
	}
	for _, raw := range l.raw {
		if (latest || !raw.at.After(at)) && raw.seq > target {
			target = raw.seq
		}
	}
	result := []manifest{l.snapshot}
	seq := l.snapshot.Seq
	for seq < target {
		var best *manifest
		for _, windows := range l.windows {
			for _, w := range windows {
				m := w.manifest
				if m.FirstSeq == seq+1 && (latest || !m.End.After(at)) && (best == nil || m.Seq > best.Seq) {
					best = &m
				}
			}
		}
		for _, raw := range l.raw {
			m := raw.manifest
			if m.FirstSeq == seq+1 && (latest || !m.At.After(at)) && (best == nil || m.Seq > best.Seq) {
				best = &m
			}
		}
		if best == nil {
			return nil, errors.New("missing committed batch before restore point")
		}
		result = append(result, *best)
		seq = best.Seq
	}
	return result, nil
}

func (r *Replica) Fetch(ctx context.Context, generation string, at time.Time, destination string) error {
	if !validGeneration(generation) {
		return fmt.Errorf("invalid generation %q", generation)
	}
	if fullPath(r.inner, destination) == r.path {
		return errors.New("restore destination is the live database")
	}
	r.archiveMu.RLock()
	defer r.archiveMu.RUnlock()
	_, err := r.restoreInto(ctx, generation, at, destination)
	return err
}
