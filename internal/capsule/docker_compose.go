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
