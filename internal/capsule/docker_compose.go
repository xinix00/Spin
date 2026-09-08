//go:build !tamago

package capsule

import (
	"archive/tar"
	"bufio"
	"bytes"
	"compress/gzip"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path"
	"strings"

	"easyacp/internal/domain"
)

// A composition unions layers. Copying every layer's whole filesystem over
// the previous one is wrong as soon as a layer was recorded on an older
// version of another: the older files it carries shadow what the EDIT
// removed. So the layer with the widest closure is the base, and every
// layer whose parent that base already covers is applied as its own Docker
// diff: only the files that recording added or changed, and its deletions
// (whiteouts). A layer from an unrelated chain still gets the full copy.

// compositionBase picks the base: the layer that the most other layers are
// built on, counting EDIT versions as one lineage (a credential recorded on
// an older tool:codex is built on tool:codex). It reports for each other
// layer whether its parents are inside that base, which makes it a diff.
func compositionBase(layers []domain.Artifact, byID map[string]domain.Artifact) (int, map[string]layerAction) {
	tip := func(id string) string {
		for depth := 0; depth < 64; depth++ {
			artifact, ok := byID[id]
			if !ok || artifact.SupersededBy == "" {
				return id
			}
			id = artifact.SupersededBy
		}
		return id
	}
	closures := make([]map[string]bool, len(layers))
	for index, layer := range layers {
		closure := map[string]bool{}
		collectParents(layer.ID, byID, closure)
		closures[index] = closure
	}
	// inBase reports whether an artifact is the base or one of its ancestors,
	// across versions of the same layer.
	inBase := func(base int, id string) bool {
		if id == layers[base].ID || closures[base][id] || tip(id) == tip(layers[base].ID) {
			return true
		}
		for ancestor := range closures[base] {
			if tip(ancestor) == tip(id) {
				return true
			}
		}
		return false
	}
	best, bestScore, bestSize := 0, -1, -1
	for index := range layers {
		score := 0
		for other := range layers {
			if other == index {
				continue
			}
			for _, parentID := range layers[other].ParentArtifactIDs {
				// A parent that is merely an older version of the same layer
				// (an EDIT chain) is not a dependency on somebody else.
				if tip(parentID) == tip(layers[other].ID) {
					continue
				}
				if inBase(index, parentID) {
					score++
					break
				}
			}
		}
		if score > bestScore || (score == bestScore && len(closures[index]) > bestSize) {
			best, bestScore, bestSize = index, score, len(closures[index])
		}
	}
	plan := map[string]layerAction{}
	for index, layer := range layers {
		if index == best {
			continue
		}
		if layer.ID == layers[best].ID || closures[best][layer.ID] {
			// Already part of the base, this exact version: copying it
			// again would put older files over newer.
			plan[layer.ID] = layerContained
			continue
		}
		// A newer version of one of the base's ancestors is not inside the
		// base: the base was recorded on the older one. Its own top layer,
		// the EDIT, goes over the base like any layer built on it.
		parentsCovered := len(layer.ParentArtifactIDs) > 0
		for _, parentID := range layer.ParentArtifactIDs {
			parentsCovered = parentsCovered && inBase(best, parentID)
		}
		if parentsCovered {
			plan[layer.ID] = layerDiffOnly
		} else {
			plan[layer.ID] = layerFullCopy
		}
	}
	return best, plan
}

// layerAction is how a non-base layer joins a composition.
type layerAction int

const (
	layerFullCopy  layerAction = iota // unrelated chain: whole filesystem
	layerDiffOnly                     // built on the base: its own top layer
	layerContained                    // already inside the base: nothing
)

// dockerManagedPath reports paths a running container owns itself: kernel
// filesystems and the files Docker mounts. Copying an export over them fails.
func dockerManagedPath(name string) bool {
	name = strings.TrimPrefix(strings.TrimPrefix(name, "./"), "/")
	for _, prefix := range []string{"proc/", "sys/", "dev/"} {
		if name == strings.TrimSuffix(prefix, "/") || strings.HasPrefix(name, prefix) {
			return true
		}
	}
	switch name {
	case "etc/hosts", "etc/hostname", "etc/resolv.conf", ".dockerenv":
		return true
	}
	return false
}

// filterExport copies a container export to out without the paths a running
// container manages itself.
func filterExport(export io.Reader, out io.Writer) error {
	reader := tar.NewReader(export)
	writer := tar.NewWriter(out)
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		if dockerManagedPath(header.Name) {
			continue
		}
		if err := writer.WriteHeader(header); err != nil {
			return err
		}
		if _, err := io.Copy(writer, reader); err != nil {
			return err
		}
	}
	return writer.Close()
}

// layerDiff writes the top layer of a docker save stream to out, minus its
// whiteout entries, and returns the paths those whiteouts delete. The top
// layer is the recording's own change: one commit per saved recording.
func layerDiff(save io.ReadSeeker, out io.Writer) ([]string, error) {
	var manifest []struct {
		Layers []string `json:"Layers"`
	}
	reader := tar.NewReader(save)
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, err
		}
		if path.Clean(header.Name) == "manifest.json" {
			if err := json.NewDecoder(reader).Decode(&manifest); err != nil {
				return nil, fmt.Errorf("decode save manifest: %w", err)
			}
		}
	}
	if len(manifest) == 0 || len(manifest[0].Layers) == 0 {
		return nil, errors.New("docker save stream has no layers in its manifest")
	}
	top := path.Clean(manifest[0].Layers[len(manifest[0].Layers)-1])
	if _, err := save.Seek(0, io.SeekStart); err != nil {
		return nil, err
	}
	reader = tar.NewReader(save)
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			return nil, fmt.Errorf("layer %s is missing from the save stream", top)
		}
		if err != nil {
			return nil, err
		}
		if path.Clean(header.Name) != top {
			continue
		}
		return filterWhiteouts(reader, out)
	}
}

// filterWhiteouts copies a layer tar to out without its whiteout entries and
// returns what those entries delete: "dir/name" for .wh.name, and "dir/*"
// for an opaque directory marker.
func filterWhiteouts(layer io.Reader, out io.Writer) ([]string, error) {
	buffered := bufio.NewReader(layer)
	if magic, err := buffered.Peek(2); err == nil && magic[0] == 0x1f && magic[1] == 0x8b {
		unzipped, err := gzip.NewReader(buffered)
		if err != nil {
			return nil, err
		}
		layer = unzipped
	} else {
		layer = buffered
	}
	reader := tar.NewReader(layer)
	writer := tar.NewWriter(out)
	var deletions []string
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, err
		}
		if dockerManagedPath(header.Name) {
			continue
		}
		dir, name := path.Split(strings.TrimPrefix(header.Name, "./"))
		switch {
		case name == ".wh..wh..opq":
			deletions = append(deletions, path.Join("/", dir, "*"))
			continue
		case strings.HasPrefix(name, ".wh."):
			deletions = append(deletions, path.Join("/", dir, strings.TrimPrefix(name, ".wh.")))
			continue
		}
		if err := writer.WriteHeader(header); err != nil {
			return nil, err
		}
		if _, err := io.Copy(writer, reader); err != nil {
			return nil, err
		}
	}
	return deletions, writer.Close()
}

// applyLayerDiff puts one layer's own change onto the running target
// container: deletions first, then the files.
func (d *Docker) applyLayerDiff(ctx context.Context, targetName string, layer domain.Artifact) error {
	save, err := os.CreateTemp("", "spin-layer-*.tar")
	if err != nil {
		return err
	}
	defer func() {
		_ = save.Close()
		_ = os.Remove(save.Name())
	}()
	saver := exec.CommandContext(ctx, d.binary, "image", "save", layer.Snapshot.Ref)
	saver.Stdout = save
	var saveError bytes.Buffer
	saver.Stderr = &saveError
	if err := saver.Run(); err != nil {
		return fmt.Errorf("docker image save %s: %s: %w", layer.Snapshot.Ref, strings.TrimSpace(saveError.String()), err)
	}
	diff, err := os.CreateTemp("", "spin-layer-diff-*.tar")
	if err != nil {
		return err
	}
	defer func() {
		_ = diff.Close()
		_ = os.Remove(diff.Name())
	}()
	if _, err := save.Seek(0, io.SeekStart); err != nil {
		return err
	}
	deletions, err := layerDiff(save, diff)
	if err != nil {
		return fmt.Errorf("read the diff of %s: %w", layer.ID, err)
	}
	if len(deletions) > 0 {
		script := "rm -rf"
		for _, deletion := range deletions {
			// An opaque directory marker empties the directory: the glob
			// must stay outside the quotes to expand.
			if strings.HasSuffix(deletion, "/*") {
				script += " " + shellQuote(strings.TrimSuffix(deletion, "/*")) + "/*"
				continue
			}
			script += " " + shellQuote(deletion)
		}
		if _, err := d.control(ctx, "exec", targetName, "sh", "-c", script); err != nil {
			return fmt.Errorf("apply deletions of %s: %w", layer.ID, err)
		}
	}
	if _, err := diff.Seek(0, io.SeekStart); err != nil {
		return err
	}
	copier := exec.CommandContext(ctx, d.binary, "cp", "-", targetName+":/")
	copier.Stdin = diff
	var copyError bytes.Buffer
	copier.Stderr = &copyError
	if err := copier.Run(); err != nil {
		return fmt.Errorf("docker cp of %s: %s: %w", layer.ID, strings.TrimSpace(copyError.String()), err)
	}
	return nil
}

func shellQuote(value string) string {
	return "'" + strings.ReplaceAll(value, "'", `'\''`) + "'"
}
