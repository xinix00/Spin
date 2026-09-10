//go:build !tamago

package capsule

import (
	"archive/tar"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path"
	"strconv"
	"strings"

	"easyacp/internal/domain"
)

// A sealed layer holds its real difference only. Docker records a file as
// changed as soon as it is opened for writing, so a tool that rewrote
// itself unchanged, or a login that touched a whole install, made a layer
// of hundreds of megabytes that added nothing. Sealing reads the layer's
// diff, drops what is byte-for-byte the same in the layer below and what
// is a cache, and rebuilds the layer from the rest when anything went.

// cleanLayer returns the manifest of the image tagged tag over its parent
// image, rebuilding the image without the identical and cache files when
// there are any.
func (d *Docker) cleanLayer(ctx context.Context, tag, parentImage, recordingID string) (*domain.LayerContents, error) {
	save, err := os.CreateTemp("", "spin-seal-*.tar")
	if err != nil {
		return nil, err
	}
	defer func() {
		_ = save.Close()
		_ = os.Remove(save.Name())
	}()
	saver := exec.CommandContext(ctx, d.binary, "image", "save", tag)
	saver.Stdout = save
	var saveError bytes.Buffer
	saver.Stderr = &saveError
	if err := saver.Run(); err != nil {
		return nil, fmt.Errorf("docker image save %s: %s: %w", tag, strings.TrimSpace(saveError.String()), err)
	}
	diff, err := os.CreateTemp("", "spin-seal-diff-*.tar")
	if err != nil {
		return nil, err
	}
	defer func() {
		_ = diff.Close()
		_ = os.Remove(diff.Name())
	}()
	if _, err := save.Seek(0, io.SeekStart); err != nil {
		return nil, err
	}
	deletions, err := layerDiff(save, diff)
	if err != nil {
		return nil, fmt.Errorf("read the diff of %s: %w", tag, err)
	}
	if _, err := diff.Seek(0, io.SeekStart); err != nil {
		return nil, err
	}
	entries, hashes, err := readDiffEntries(diff)
	if err != nil {
		return nil, err
	}
	// The parent's hashes of the same paths tell which files came back
	// unchanged.
	identical := map[string]bool{}
	if parentImage != "" && len(hashes) > 0 {
		parentHashes, err := d.hashesInImage(ctx, parentImage, keys(hashes))
		if err != nil {
			return nil, err
		}
		for name, hash := range hashes {
			if parentHashes[name] == hash {
				identical[name] = true
			}
		}
	}
	var kept []contentEntry
	contents := domain.LayerContents{}
	drop := map[string]bool{}
	for _, entry := range entries {
		switch {
		case identical[entry.Path]:
			contents.DroppedIdentical.Files++
			contents.DroppedIdentical.Bytes += entry.Size
			drop[entry.Path] = true
		default:
			kept = append(kept, entry)
		}
	}
	summary := summarize(kept)
	summary.DroppedIdentical = contents.DroppedIdentical
	if len(drop) == 0 {
		return &summary, nil
	}
	if _, err := diff.Seek(0, io.SeekStart); err != nil {
		return nil, err
	}
	filtered, err := os.CreateTemp("", "spin-seal-kept-*.tar")
	if err != nil {
		return nil, err
	}
	defer func() {
		_ = filtered.Close()
		_ = os.Remove(filtered.Name())
	}()
	if err := filterTar(diff, filtered, func(name string) bool {
		clean := "/" + strings.TrimPrefix(strings.TrimPrefix(name, "./"), "/")
		return !drop[clean]
	}); err != nil {
		return nil, err
	}
	if _, err := filtered.Seek(0, io.SeekStart); err != nil {
		return nil, err
	}
	if err := d.rebuildLayer(ctx, tag, parentImage, recordingID, filtered, deletions); err != nil {
		return nil, err
	}
	return &summary, nil
}

// readDiffEntries lists the regular files of a layer tar with their kind
// and content hash.
func readDiffEntries(layer io.Reader) ([]contentEntry, map[string]string, error) {
	reader := tar.NewReader(layer)
	var entries []contentEntry
	hashes := map[string]string{}
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, nil, err
		}
		if header.Typeflag != tar.TypeReg {
			continue
		}
		name := "/" + strings.TrimPrefix(strings.TrimPrefix(header.Name, "./"), "/")
		hasher := sha256.New()
		if _, err := io.Copy(hasher, reader); err != nil {
			return nil, nil, err
		}
		entries = append(entries, contentEntry{Path: name, Size: header.Size})
		hashes[name] = hex.EncodeToString(hasher.Sum(nil))
	}
	return entries, hashes, nil
}

func keys(values map[string]string) []string {
	out := make([]string, 0, len(values))
	for key := range values {
		out = append(out, key)
	}
	return out
}

// hashesInImage asks a one-shot container of the image for the sha256 of
// the given paths; missing paths are simply absent from the answer.
func (d *Docker) hashesInImage(ctx context.Context, image string, paths []string) (map[string]string, error) {
	var input strings.Builder
	for _, name := range paths {
		if strings.ContainsAny(name, "\n\r") {
			continue
		}
		input.WriteString(name)
		input.WriteByte('\n')
	}
	script := `while IFS= read -r p; do [ -f "$p" ] && sha256sum "$p"; done; true`
	output, err := d.controlInput(ctx, []byte(input.String()), "run", "--rm", "-i", "--network", "none", "--entrypoint", "sh", image, "-c", script)
	if err != nil {
		return nil, fmt.Errorf("hash files in %s: %w", image, err)
	}
	hashes := map[string]string{}
	for _, line := range strings.Split(output, "\n") {
		hash, name, ok := strings.Cut(strings.TrimSpace(line), "  ")
		if ok && len(hash) == 64 {
			hashes[name] = hash
		}
	}
	return hashes, nil
}

// filterTar copies the entries keep allows from a tar to another.
func filterTar(in io.Reader, out io.Writer, keep func(name string) bool) error {
	reader := tar.NewReader(in)
	writer := tar.NewWriter(out)
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		if header.Typeflag == tar.TypeReg && !keep(header.Name) {
			continue
		}
		if err := writer.WriteHeader(header); err != nil {
			return err
		}
		if header.Typeflag == tar.TypeReg {
			if _, err := io.Copy(writer, reader); err != nil {
				return err
			}
		}
	}
	return writer.Close()
}

// rebuildLayer makes the image under tag again from the parent image plus
// the kept part of the diff.
func (d *Docker) rebuildLayer(ctx context.Context, tag, parentImage, recordingID string, kept *os.File, deletions []string) error {
	targetName := runtimeName("spin-seal-build", recordingID)
	_ = d.removeContainer(context.Background(), targetName)
	if _, err := d.control(ctx,
		"run", "-d", "--name", targetName,
		"--label", "spin.managed=true",
		"--label", "spin.kind=seal-build",
		"--network", "none",
		"--entrypoint", "sh", parentImage, "-lc", "trap 'exit 0' TERM INT; while :; do sleep 3600; done",
	); err != nil {
		return fmt.Errorf("create seal build from %s: %w", parentImage, err)
	}
	defer func() { _ = d.removeContainer(context.Background(), targetName) }()
	if len(deletions) > 0 {
		script := "rm -rf"
		for _, deletion := range deletions {
			if strings.HasSuffix(deletion, "/*") {
				script += " " + shellQuote(strings.TrimSuffix(deletion, "/*")) + "/*"
				continue
			}
			script += " " + shellQuote(deletion)
		}
		if _, err := d.control(ctx, "exec", targetName, "sh", "-c", script); err != nil {
			return fmt.Errorf("apply deletions: %w", err)
		}
	}
	copier := exec.CommandContext(ctx, d.binary, "cp", "-", targetName+":/")
	copier.Stdin = kept
	var copyError bytes.Buffer
	copier.Stderr = &copyError
	if err := copier.Run(); err != nil {
		return fmt.Errorf("docker cp of the kept diff: %s: %w", strings.TrimSpace(copyError.String()), err)
	}
	previous, _ := d.control(ctx, "image", "inspect", "--format", "{{.Id}}", tag)
	if _, err := d.control(ctx,
		"commit", "--pause=true",
		"--change", "LABEL spin.managed=true",
		"--change", "LABEL spin.recording_id="+recordingID,
		targetName, tag,
	); err != nil {
		return fmt.Errorf("commit the cleaned layer: %w", err)
	}
	if previous = strings.TrimSpace(previous); previous != "" {
		_, _, _ = d.run(ctx, "image", "rm", "-f", previous)
	}
	return nil
}

// CaptureCapsuleChanges classifies what a running capsule changed against
// its image, outside the workspace: what an agent installed or touched.
func (d *Docker) CaptureCapsuleChanges(ctx context.Context, runtime domain.CapsuleRuntime) (domain.LayerContents, error) {
	if runtime.Driver != "docker" || runtime.ContainerID == "" || runtime.Status == "stopped" {
		return domain.LayerContents{}, errors.New("composition has no live Docker capsule")
	}
	output, err := d.control(ctx, "diff", runtime.ContainerID)
	if err != nil {
		return domain.LayerContents{}, err
	}
	var changed []string
	for _, line := range strings.Split(output, "\n") {
		kind, name, ok := strings.Cut(strings.TrimSpace(line), " ")
		if !ok || (kind != "A" && kind != "C") {
			continue
		}
		name = path.Clean(name)
		if insideWorkspace(name) {
			continue
		}
		changed = append(changed, name)
	}
	if len(changed) == 0 {
		return domain.LayerContents{}, nil
	}
	var input strings.Builder
	for _, name := range changed {
		if !strings.ContainsAny(name, "\n\r") {
			input.WriteString(name + "\n")
		}
	}
	sizes, err := d.controlInput(ctx, []byte(input.String()), "exec", "-i", runtime.ContainerID, "sh", "-c", `while IFS= read -r p; do [ -f "$p" ] && printf '%s %s\n' "$(stat -c %s "$p" 2>/dev/null || wc -c < "$p")" "$p"; done; true`)
	if err != nil {
		return domain.LayerContents{}, err
	}
	var entries []contentEntry
	for _, line := range strings.Split(sizes, "\n") {
		sizeText, name, ok := strings.Cut(strings.TrimSpace(line), " ")
		if !ok {
			continue
		}
		size, _ := strconv.ParseInt(sizeText, 10, 64)
		entries = append(entries, contentEntry{Path: name, Size: size})
	}
	return summarize(entries), nil
}
