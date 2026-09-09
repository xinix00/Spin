package replica

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"
)

const formatVersion = 2

// ObjectStore must provide atomic object replacement and strongly consistent
// GET/LIST after PUT/DELETE. A namespace has exactly one writer.
type ObjectStore interface {
	Put(context.Context, string, []byte) error
	Get(context.Context, string) ([]byte, error)
	Delete(context.Context, string) error
	List(context.Context, string) ([]Object, error)
}

type partRef struct {
	Key  string `json:"key"`
	Size int64  `json:"size"`
	Hash string `json:"sha256"`
}

// A manifest is the commit record. Parts alone are never restore points.
// FirstSeq..Seq describes a contiguous run of complete read transactions.
type manifest struct {
	MinSize  int64     `json:"min_size"`
	Version  int       `json:"version"`
	FirstSeq int64     `json:"first_seq"`
	Seq      int64     `json:"seq"`
	At       time.Time `json:"at"`
	Level    int       `json:"level"`
	Start    time.Time `json:"start,omitempty"`
	End      time.Time `json:"end,omitempty"`
	Parts    []partRef `json:"parts"`
}

var ErrLegacyFormat = errors.New("legacy replica has no atomic commit manifests; create a new generation from the original database")

func (r *Replica) putManifest(ctx context.Context, key string, m manifest) error {
	data, err := json.Marshal(m)
	if err != nil {
		return err
	}
	return r.s3.Put(ctx, key, data)
}

func (r *Replica) getManifest(ctx context.Context, generation, key string) (manifest, error) {
	data, err := r.s3.Get(ctx, key)
	if err != nil {
		return manifest{}, err
	}
	var m manifest
	if err := json.Unmarshal(data, &m); err != nil {
		return m, fmt.Errorf("manifest %s: %w", key, err)
	}
	if m.Version != formatVersion {
		return m, ErrLegacyFormat
	}
	if m.MinSize < 0 || m.FirstSeq < 1 || m.Seq < m.FirstSeq || m.At.IsZero() || len(m.Parts) == 0 {
		return m, fmt.Errorf("invalid manifest %s", key)
	}
	if m.Level > 0 && (m.Start.IsZero() || !m.End.After(m.Start) || !m.At.Before(m.End)) {
		return m, fmt.Errorf("invalid window %s", key)
	}
	seen := map[string]bool{}
	for _, part := range m.Parts {
		if !strings.HasPrefix(part.Key, r.generationPrefix(generation)+"data/") || strings.Contains(part.Key, "..") || part.Size < 56 || len(part.Hash) != 64 || seen[part.Key] {
			return m, fmt.Errorf("invalid part in %s", key)
		}
		seen[part.Key] = true
	}
	return m, nil
}

func (r *Replica) readPart(ctx context.Context, part partRef) (segment, error) {
	data, err := r.s3.Get(ctx, part.Key)
	if err != nil {
		return segment{}, err
	}
	if int64(len(data)) != part.Size || sha256hex(data) != part.Hash {
		return segment{}, fmt.Errorf("part checksum mismatch: %s", part.Key)
	}
	return decodeSegment(data)
}

func (r *Replica) putPart(ctx context.Context, key string, seg segment) (partRef, error) {
	data := encodeSegment(seg)
	ref := partRef{Key: key, Size: int64(len(data)), Hash: sha256hex(data)}
	return ref, r.s3.Put(ctx, key, data)
}
