package replica

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"sort"
	"strings"
	"time"
)

type committedManifest struct {
	key string
	manifest
}

type layout struct {
	raw      []committedManifest
	windows  map[int][]committedManifest
	snapshot manifest
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
	// Parts can outnumber manifests by orders of magnitude. They have no
	// bearing on commit visibility and do not belong in a layout listing.
	objects, err := r.s3.List(ctx, prefix+"L")
	if err != nil {
		return nil, err
	}
	l := &layout{windows: map[int][]committedManifest{}}
	keys := []string{r.snapshotKey(generation)}
	for _, object := range objects {
		rest := strings.TrimPrefix(object.Key, prefix)
		if (strings.HasPrefix(rest, "L0/") && strings.HasSuffix(rest, ".json")) || (strings.HasPrefix(rest, "L") && strings.HasSuffix(rest, "/complete")) {
			keys = append(keys, object.Key)
		}
	}
	manifests, err := r.getManifests(ctx, generation, keys)
	if err != nil {
		return nil, err
	}
	seqs := map[int64]bool{}
	for index, key := range keys {
		rest := strings.TrimPrefix(key, prefix)
		m := manifests[index]
		switch {
		case rest == "snapshot":
			if m.At.IsZero() {
				continue // A failed or pruned generation has no snapshot.
			}
			if m.Level != 0 || m.FirstSeq != 1 || m.Seq != 1 {
				return nil, fmt.Errorf("%w: invalid snapshot manifest", errReplicaCorrupt)
			}
			l.snapshot = m
		case strings.HasPrefix(rest, "L0/"):
			if m.Level != 0 || m.FirstSeq != m.Seq || m.Seq <= 1 || seqs[m.Seq] {
				return nil, fmt.Errorf("%w: invalid or duplicate raw commit", errReplicaCorrupt)
			}
			seqs[m.Seq] = true
			l.raw = append(l.raw, committedManifest{key: key, manifest: m})
		default:
			if m.Level < 1 || m.FirstSeq <= 1 {
				return nil, fmt.Errorf("%w: invalid window manifest", errReplicaCorrupt)
			}
			l.windows[m.Level] = append(l.windows[m.Level], committedManifest{key: key, manifest: m})
		}
	}
	sort.Slice(l.raw, func(i, j int) bool { return l.raw[i].Seq < l.raw[j].Seq })
	for level := range l.windows {
		sort.Slice(l.windows[level], func(i, j int) bool { return l.windows[level][i].Start.Before(l.windows[level][j].Start) })
	}
	return l, nil
}

// errCommitGap: a window's commits do not follow each other. Nothing merges
// across it, so the generation cannot thin out any more; it ends.
var errCommitGap = errors.New("cannot merge a gap in commit sequence")

// manifestFetchers is how many manifests are fetched at once. A generation
// whose compaction was stuck for a day holds thousands of them, and one GET
// after another made a start wait minutes for its layout.
const manifestFetchers = 8

// getManifests fetches manifests in parallel, in the order of keys. A missing
// snapshot manifest stays zero: a failed or pruned generation has none.
func (r *Replica) getManifests(ctx context.Context, generation string, keys []string) ([]manifest, error) {
	out := make([]manifest, len(keys))
	err := each(ctx, manifestFetchers, len(keys), func(ctx context.Context, index int) error {
		m, err := r.getManifest(ctx, generation, keys[index])
		if errors.Is(err, ErrNotFound) && keys[index] == r.snapshotKey(generation) {
			return nil
		}
		if err != nil {
			return err
		}
		out[index] = m
		return nil
	})
	return out, err
}

func (r *Replica) compact(ctx context.Context, generation string, now time.Time) error {
	r.archiveMu.Lock()
	defer r.archiveMu.Unlock()
	l, err := r.loadLayout(ctx, generation)
	if err != nil {
		return err
	}
	if l.snapshot.At.IsZero() {
		return fmt.Errorf("%w: generation has no committed snapshot", errReplicaCorrupt)
	}
	for level := 1; level <= len(r.config.Schedule); level++ {
		for _, span := range r.elapsedWindows(l, level, now) {
			inputs := l.inputs(level, span[0], span[1])
			if len(inputs) == 0 {
				continue
			}
			m, err := r.mergeWindow(ctx, generation, level, span[0], span[1], inputs)
			if err != nil {
				return err
			}
			key := r.windowPrefix(generation, level, span[0], span[1]) + "complete"
			l.windows[level] = append(l.windows[level], committedManifest{key: key, manifest: m})
		}
		// A retry may fill a hole before an already committed window.
		sort.Slice(l.windows[level], func(i, j int) bool { return l.windows[level][i].Start.Before(l.windows[level][j].Start) })
	}
	return r.expire(ctx, l, now)
}

func (r *Replica) elapsedWindows(l *layout, level int, now time.Time) [][2]time.Time {
	size := r.config.Schedule[level-1].Window
	merged := map[int64]bool{}
	for _, w := range l.windows[level] {
		merged[w.Start.Unix()] = true
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
			note(raw.At)
		}
	} else {
		for _, w := range l.windows[level-1] {
			note(w.End)
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
			if raw.At.After(start) && !raw.At.After(end) {
				inputs = append(inputs, raw.manifest)
			}
		}
	} else {
		for _, w := range l.windows[level-1] {
			if !w.Start.Before(start) && !w.End.After(end) {
				inputs = append(inputs, w.manifest)
			}
		}
	}
	return inputs
}

// Merge retains the last page state and last database size. Every attempt uses
// new part keys; the single manifest PUT publishes all parts atomically.
func (r *Replica) mergeWindow(ctx context.Context, generation string, level int, start, end time.Time, inputs []manifest) (manifest, error) {
	if len(inputs) == 0 {
		return manifest{}, nil
	}
	var refs []partRef
	seq := inputs[0].FirstSeq - 1
	for _, m := range inputs {
		if m.FirstSeq != seq+1 {
			return manifest{}, errCommitGap
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
		for _, ref := range input.Parts {
			seg, err := r.readPart(ctx, ref)
			if err != nil {
				return manifest{}, err
			}
			if pageSize != 0 && pageSize != seg.PageSize {
				return manifest{}, fmt.Errorf("%w: page size changed within generation", errReplicaCorrupt)
			}
			if input.MinSize > seg.DBSize || input.MinSize%int64(seg.PageSize) != 0 {
				return manifest{}, fmt.Errorf("%w: inconsistent database size in manifest", errReplicaCorrupt)
			}
			// Live captures can change size between parts. Honor every
			// truncation in order, just as restoreInto does.
			if seg.DBSize < dbSize {
				for page := range winner {
					if int64(page)*int64(seg.PageSize) > seg.DBSize {
						delete(winner, page)
					}
				}
			}
			for _, page := range seg.Pages {
				winner[page] = index
			}
			pageSize, dbSize = seg.PageSize, seg.DBSize
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
			return manifest{}, err
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
					return manifest{}, err
				}
			}
		}
	}
	if len(out.Pages) > 0 || len(m.Parts) == 0 {
		if err := flush(); err != nil {
			return manifest{}, err
		}
	}
	return m, r.putManifest(ctx, r.windowPrefix(generation, level, start, end)+"complete", m)
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
		if raw.At.Before(now.Add(-r.config.Schedule[0].Window)) && covered(0, raw.manifest) {
			if err := remove(raw.key, raw.manifest); err != nil {
				return err
			}
		}
	}
	for level := 1; level < len(r.config.Schedule); level++ {
		for _, w := range l.windows[level] {
			if w.End.Before(now.Add(-r.config.Schedule[level-1].Keep)) && covered(level, w.manifest) {
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
		if errors.Is(err, errReplicaCorrupt) && generation != current.Generation {
			// A repaired current generation must remain discoverable while
			// damaged older metadata is still retained for investigation.
			r.logger.Warn("replica: damaged generation omitted from restore points", "generation", generation, "error", err)
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
				add(w.End, level)
			}
		}
		for _, raw := range l.raw {
			add(raw.At, 0)
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
		return nil, fmt.Errorf("%w: generation has no committed snapshot", errReplicaCorrupt)
	}
	latest := at.IsZero()
	if !latest && at.Before(l.snapshot.At) {
		return nil, errors.New("requested time precedes snapshot")
	}
	target := l.snapshot.Seq
	for _, windows := range l.windows {
		for _, w := range windows {
			if (latest || !w.End.After(at)) && w.Seq > target {
				target = w.Seq
			}
		}
	}
	for _, raw := range l.raw {
		if (latest || !raw.At.After(at)) && raw.Seq > target {
			target = raw.Seq
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
			return nil, fmt.Errorf("%w: missing committed batch before restore point", errReplicaCorrupt)
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
