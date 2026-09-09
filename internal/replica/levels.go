package replica

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"sort"
	"strconv"
	"strings"
	"time"
)

// Restore points thin out with age, the way Litestream compacts: the raw
// segments (level 0, one per sync) are merged into level 1 windows once a
// window has elapsed, level 1 windows into level 2, and so on. A merged
// window holds every page changed in it, once, at its last state. Restore
// to a moment applies the snapshot, then the coarsest windows that end
// before it, then finer ones for the remainder, then raw segments. Finer
// files go once a coarser window covers them and their keep has passed;
// the coarsest level lives as long as its generation.
//
//	generations/<id>/L0/<seq>-<unix>.seg          raw segments
//	generations/<id>/L<k>/<start>-<end>/<part>.seg merged windows
//	generations/<id>/snapshot                       when the snapshot completed

type rawFile struct {
	key  string
	seq  int64
	at   time.Time
	size int64
}

type windowFile struct {
	key  string
	part int
	size int64
}

type window struct {
	level int
	start time.Time
	end   time.Time
	parts []windowFile
	bytes int64
}

type snapshotNote struct {
	Seq int64     `json:"seq"`
	At  time.Time `json:"at"`
}

type layout struct {
	generation string
	raw        []rawFile
	windows    map[int][]window
	snapshot   snapshotNote
}

func (r *Replica) rawKey(generation string, seq int64, at time.Time) string {
	return fmt.Sprintf("%sL0/%012d-%010d.seg", r.generationPrefix(generation), seq, at.Unix())
}

func (r *Replica) windowPrefix(generation string, level int, start, end time.Time) string {
	return fmt.Sprintf("%sL%d/%010d-%010d/", r.generationPrefix(generation), level, start.Unix(), end.Unix())
}

func (r *Replica) snapshotKey(generation string) string {
	return r.generationPrefix(generation) + "snapshot"
}

// loadLayout reads what the bucket holds for a generation.
func (r *Replica) loadLayout(ctx context.Context, generation string) (*layout, error) {
	prefix := r.generationPrefix(generation)
	objects, err := r.s3.List(ctx, prefix)
	if err != nil {
		return nil, err
	}
	result := &layout{generation: generation, windows: map[int][]window{}}
	byWindow := map[string]*window{}
	for _, object := range objects {
		rest := strings.TrimPrefix(object.Key, prefix)
		switch {
		case rest == "snapshot":
			data, err := r.s3.Get(ctx, object.Key)
			if err != nil {
				return nil, err
			}
			_ = json.Unmarshal(data, &result.snapshot)
			result.snapshot.At = result.snapshot.At.UTC().Truncate(time.Second)
		case strings.HasPrefix(rest, "L0/"):
			name := strings.TrimSuffix(strings.TrimPrefix(rest, "L0/"), ".seg")
			seqText, atText, ok := strings.Cut(name, "-")
			if !ok {
				continue
			}
			seq, seqErr := strconv.ParseInt(seqText, 10, 64)
			at, atErr := strconv.ParseInt(atText, 10, 64)
			if seqErr != nil || atErr != nil {
				continue
			}
			result.raw = append(result.raw, rawFile{key: object.Key, seq: seq, at: time.Unix(at, 0).UTC(), size: object.Size})
		case strings.HasPrefix(rest, "L"):
			levelText, remainder, ok := strings.Cut(rest[1:], "/")
			if !ok {
				continue
			}
			level, err := strconv.Atoi(levelText)
			if err != nil || level < 1 {
				continue
			}
			span, partText, ok := strings.Cut(remainder, "/")
			if !ok || !strings.HasSuffix(partText, ".seg") {
				continue
			}
			startText, endText, ok := strings.Cut(span, "-")
			if !ok {
				continue
			}
			start, startErr := strconv.ParseInt(startText, 10, 64)
			end, endErr := strconv.ParseInt(endText, 10, 64)
			part, partErr := strconv.Atoi(strings.TrimSuffix(partText, ".seg"))
			if startErr != nil || endErr != nil || partErr != nil {
				continue
			}
			id := fmt.Sprintf("%d/%s", level, span)
			current := byWindow[id]
			if current == nil {
				current = &window{level: level, start: time.Unix(start, 0).UTC(), end: time.Unix(end, 0).UTC()}
				byWindow[id] = current
			}
			current.parts = append(current.parts, windowFile{key: object.Key, part: part, size: object.Size})
			current.bytes += object.Size
		}
	}
	sort.Slice(result.raw, func(i, j int) bool { return result.raw[i].seq < result.raw[j].seq })
	for _, current := range byWindow {
		sort.Slice(current.parts, func(i, j int) bool { return current.parts[i].part < current.parts[j].part })
		result.windows[current.level] = append(result.windows[current.level], *current)
	}
	for level := range result.windows {
		sort.Slice(result.windows[level], func(i, j int) bool { return result.windows[level][i].start.Before(result.windows[level][j].start) })
	}
	return result, nil
}

// compact merges elapsed windows level by level and drops what coarser
// windows cover once its keep has passed.
func (r *Replica) compact(ctx context.Context, generation string) error {
	now := r.now()
	for level := 1; level <= len(r.config.Schedule); level++ {
		current, err := r.loadLayout(ctx, generation)
		if err != nil {
			return err
		}
		size := r.config.Schedule[level-1].Window
		for _, span := range r.elapsedWindows(current, level, now) {
			inputs := current.inputs(level, span[0], span[1])
			if len(inputs) == 0 {
				continue
			}
			if err := r.mergeWindow(ctx, generation, level, span[0], span[1], inputs); err != nil {
				return err
			}
			r.logger.Info("replica: window merged", "domain", r.domain, "generation", generation, "level", level, "start", span[0].Format(time.RFC3339), "window", size.String(), "inputs", len(inputs))
		}
	}
	current, err := r.loadLayout(ctx, generation)
	if err != nil {
		return err
	}
	return r.expire(ctx, current, now)
}

// elapsedWindows lists the windows of a level that have ended, hold input,
// and have no merged file yet.
func (r *Replica) elapsedWindows(current *layout, level int, now time.Time) [][2]time.Time {
	size := r.config.Schedule[level-1].Window
	merged := map[int64]bool{}
	for _, existing := range current.windows[level] {
		merged[existing.start.Unix()] = true
	}
	candidates := map[int64]bool{}
	note := func(at time.Time) {
		start := at.Truncate(size)
		if !start.Add(size).After(now) && !merged[start.Unix()] {
			candidates[start.Unix()] = true
		}
	}
	if level == 1 {
		for _, raw := range current.raw {
			note(raw.at)
		}
	} else {
		for _, finer := range current.windows[level-1] {
			note(finer.start)
		}
	}
	starts := make([]int64, 0, len(candidates))
	for start := range candidates {
		starts = append(starts, start)
	}
	sort.Slice(starts, func(i, j int) bool { return starts[i] < starts[j] })
	spans := make([][2]time.Time, 0, len(starts))
	for _, start := range starts {
		begin := time.Unix(start, 0).UTC()
		spans = append(spans, [2]time.Time{begin, begin.Add(size)})
	}
	return spans
}

// inputs are the level-1 files inside a window, in order.
func (l *layout) inputs(level int, start, end time.Time) []string {
	var keys []string
	if level == 1 {
		for _, raw := range l.raw {
			if !raw.at.Before(start) && raw.at.Before(end) {
				keys = append(keys, raw.key)
			}
		}
		return keys
	}
	for _, finer := range l.windows[level-1] {
		if !finer.start.Before(start) && !finer.end.After(end) {
			for _, part := range finer.parts {
				keys = append(keys, part.key)
			}
		}
	}
	return keys
}

// mergeWindow unions the inputs into one window: pass one learns which
// input holds the last state of every page, pass two copies exactly those
// pages out, so memory stays at one input and one output part.
func (r *Replica) mergeWindow(ctx context.Context, generation string, level int, start, end time.Time, inputs []string) error {
	winner := map[uint32]int{}
	pageSize, dbSize := 0, int64(0)
	for index, key := range inputs {
		data, err := r.s3.Get(ctx, key)
		if err != nil {
			return err
		}
		seg, err := decodeSegment(data)
		if err != nil {
			return fmt.Errorf("%s: %w", key, err)
		}
		for _, page := range seg.Pages {
			winner[page] = index
		}
		pageSize, dbSize = seg.PageSize, seg.DBSize
	}
	if pageSize == 0 {
		return nil
	}
	prefix := r.windowPrefix(generation, level, start, end)
	part := 0
	out := segment{PageSize: pageSize, DBSize: dbSize}
	outBytes := 0
	flush := func() error {
		if len(out.Pages) == 0 {
			return nil
		}
		part++
		if err := r.s3.Put(ctx, fmt.Sprintf("%s%06d.seg", prefix, part), encodeSegment(out)); err != nil {
			return err
		}
		out = segment{PageSize: pageSize, DBSize: dbSize}
		outBytes = 0
		return nil
	}
	for index, key := range inputs {
		data, err := r.s3.Get(ctx, key)
		if err != nil {
			return err
		}
		seg, err := decodeSegment(data)
		if err != nil {
			return fmt.Errorf("%s: %w", key, err)
		}
		for position, page := range seg.Pages {
			if winner[page] != index {
				continue
			}
			out.Pages = append(out.Pages, page)
			out.Data = append(out.Data, seg.Data[position])
			outBytes += pageSize
			if outBytes >= r.config.SegmentBytes {
				if err := flush(); err != nil {
					return err
				}
			}
		}
	}
	if err := flush(); err != nil {
		return err
	}
	if part == 0 {
		// A window with inputs but no pages (the database only shrank)
		// still needs a file, so restore points know its size.
		return r.s3.Put(ctx, prefix+"000001.seg", encodeSegment(segment{PageSize: pageSize, DBSize: dbSize}))
	}
	return nil
}

// expire removes files a coarser window covers once their keep has passed.
func (r *Replica) expire(ctx context.Context, current *layout, now time.Time) error {
	covered := func(level int, at time.Time) bool {
		for _, coarser := range current.windows[level+1] {
			if !at.Before(coarser.start) && at.Before(coarser.end) {
				return true
			}
		}
		return false
	}
	var keys []string
	if len(r.config.Schedule) > 0 {
		keep := r.config.Schedule[0].Window
		for _, raw := range current.raw {
			if raw.at.Before(now.Add(-keep)) && covered(0, raw.at) {
				keys = append(keys, raw.key)
			}
		}
	}
	for level := 1; level < len(r.config.Schedule); level++ {
		keep := r.config.Schedule[level-1].Keep
		for _, finer := range current.windows[level] {
			if finer.end.Before(now.Add(-keep)) && covered(level, finer.start) {
				for _, part := range finer.parts {
					keys = append(keys, part.key)
				}
			}
		}
	}
	for _, key := range keys {
		if err := r.s3.Delete(ctx, key); err != nil {
			return err
		}
	}
	if len(keys) > 0 {
		r.logger.Info("replica: covered files expired", "domain", r.domain, "generation", current.generation, "files", len(keys))
	}
	return nil
}

// A Point is a moment the database can be put back to.
type Point struct {
	Generation string    `json:"generation"`
	At         time.Time `json:"at"`
	Level      int       `json:"level"`
	Current    bool      `json:"current"`
}

// Points lists every restore point in the bucket, newest first: the
// snapshot of each generation, every merged window's end, every raw
// segment still there.
func (r *Replica) Points(ctx context.Context) ([]Point, error) {
	generations, err := r.generationIDs(ctx)
	if err != nil {
		return nil, err
	}
	current := r.getMarker()
	var points []Point
	for _, generation := range generations {
		current := generation == current.Generation && current.Complete
		lay, err := r.loadLayout(ctx, generation)
		if err != nil {
			return nil, err
		}
		if lay.snapshot.At.IsZero() {
			continue
		}
		seen := map[int64]bool{}
		add := func(at time.Time, level int) {
			at = at.Truncate(time.Second)
			if at.Before(lay.snapshot.At) || seen[at.Unix()] {
				return
			}
			seen[at.Unix()] = true
			points = append(points, Point{Generation: generation, At: at, Level: level, Current: current})
		}
		add(lay.snapshot.At, -1)
		for level, windows := range lay.windows {
			for _, span := range windows {
				add(span.end, level)
			}
		}
		for _, raw := range lay.raw {
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
		if ok && id != "" && !seen[id] {
			seen[id] = true
			ids = append(ids, id)
		}
	}
	sort.Strings(ids)
	return ids, nil
}

// Fetch writes the database as it was at a point to another file. A zero
// moment means the latest state of the generation.
func (r *Replica) Fetch(ctx context.Context, generation string, at time.Time, destination string) error {
	if !validGeneration(generation) {
		return fmt.Errorf("invalid generation %q", generation)
	}
	_, err := r.restoreInto(ctx, generation, at, destination)
	return err
}

// restoreInto rebuilds the database at a moment: snapshot and windows,
// coarsest first, then raw segments up to the moment.
func (r *Replica) restoreInto(ctx context.Context, generation string, at time.Time, path string) (marker, error) {
	lay, err := r.loadLayout(ctx, generation)
	if err != nil {
		return marker{}, err
	}
	if at.IsZero() {
		at = time.Unix(1<<40, 0)
	}
	var keys []string
	cursor := time.Time{}
	for level := len(r.config.Schedule); level >= 1; level-- {
		for _, span := range lay.windows[level] {
			if span.start.Before(cursor) || span.end.After(at) {
				continue
			}
			for _, part := range span.parts {
				keys = append(keys, part.key)
			}
			cursor = span.end
		}
	}
	var lastSeq int64
	for _, raw := range lay.raw {
		if raw.at.Before(cursor) || raw.at.After(at) {
			continue
		}
		keys = append(keys, raw.key)
		lastSeq = raw.seq
	}
	if len(keys) == 0 {
		return marker{}, errors.New("nothing to restore at that moment")
	}
	file, err := r.files.Open(path, true)
	if err != nil {
		return marker{}, err
	}
	result := marker{Generation: generation, Complete: true, Clean: true, StartedAt: r.now()}
	for _, key := range keys {
		data, err := r.s3.Get(ctx, key)
		if err != nil {
			_ = file.Close()
			return marker{}, err
		}
		seg, err := decodeSegment(data)
		if err != nil {
			_ = file.Close()
			return marker{}, fmt.Errorf("%s: %w", key, err)
		}
		for index, page := range seg.Pages {
			if _, err := file.WriteAt(seg.Data[index], int64(page-1)*int64(seg.PageSize)); err != nil {
				_ = file.Close()
				return marker{}, err
			}
		}
		if err := file.Truncate(seg.DBSize); err != nil {
			_ = file.Close()
			return marker{}, err
		}
		result.Size = seg.DBSize
		result.Bytes += int64(len(data))
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return marker{}, err
	}
	if err := file.Close(); err != nil {
		return marker{}, err
	}
	result.Seq = lastSeq
	return result, nil
}
