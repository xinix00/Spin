//go:build !tamago

package capsule

import (
	"archive/tar"
	"bufio"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"sort"
	"strings"

	"easyacp/internal/domain"
)

// A layer recorded on another Spin layer is archived as a delta: only its
// own difference, with the name of its parent. A runner that needs it puts
// the parent in place first and rebuilds the image from the two. Docker
// gives such a rebuilt layer another diff ID, so a layer's identity is its
// content: a chain of the parent's identity and the hash of its difference.

const deltaNote = "spin-delta.json"
const contentLabel = "spin.content"

type deltaHeader struct {
	Version   int    `json:"version"`
	ParentRef string `json:"parent_ref"`
	Content   string `json:"content"`
	LayerHash string `json:"layer_hash"`
}

// layerIdentity computes a sealed image's content identity over its parent.
func (d *Docker) layerIdentity(ctx context.Context, tag, parentImage string) (content, layerHash string, err error) {
	save, err := d.saveImage(ctx, tag)
	if err != nil {
		return "", "", err
	}
	defer func() {
		_ = save.Close()
		_ = os.Remove(save.Name())
	}()
	top, err := os.CreateTemp("", "spin-top-*.tar")
	if err != nil {
		return "", "", err
	}
	defer func() {
		_ = top.Close()
		_ = os.Remove(top.Name())
	}()
	if err := topLayer(save, top); err != nil {
		return "", "", err
	}
	if _, err := top.Seek(0, io.SeekStart); err != nil {
		return "", "", err
	}
	layerHash, err = layerContentHash(top)
	if err != nil {
		return "", "", err
	}
	parent, err := d.imageIdentity(ctx, parentImage)
	if err != nil {
		return "", "", err
	}
	return chainContent(parent, layerHash), layerHash, nil
}

// imageIdentity is an image's content: the label a rebuild left, else its
// layers (the same on every runner for an image that was never rebuilt).
func (d *Docker) imageIdentity(ctx context.Context, ref string) (string, error) {
	if ref == "" {
		return "", nil
	}
	if label := d.imageLabel(ctx, ref, contentLabel); label != "" {
		return label, nil
	}
	return d.imageRootFS(ctx, ref)
}

func (d *Docker) imageLabel(ctx context.Context, ref, label string) string {
	value, _, err := d.run(ctx, "image", "inspect", "--format", "{{index .Config.Labels \""+label+"\"}}", ref)
	if err != nil {
		return ""
	}
	return strings.TrimSpace(value)
}

func chainContent(parent, layerHash string) string {
	sum := sha256.Sum256([]byte("spin-layer\n" + parent + "\n" + layerHash))
	return "content:" + hex.EncodeToString(sum[:])
}

func (d *Docker) saveImage(ctx context.Context, ref string) (*os.File, error) {
	save, err := os.CreateTemp("", "spin-save-*.tar")
	if err != nil {
		return nil, err
	}
	saver := exec.CommandContext(ctx, d.binary, "image", "save", ref)
	saver.Stdout = save
	var saveError bytes.Buffer
	saver.Stderr = &saveError
	if err := saver.Run(); err != nil {
		_ = save.Close()
		_ = os.Remove(save.Name())
		return nil, fmt.Errorf("docker image save %s: %s: %w", ref, strings.TrimSpace(saveError.String()), err)
	}
	if _, err := save.Seek(0, io.SeekStart); err != nil {
		_ = save.Close()
		_ = os.Remove(save.Name())
		return nil, err
	}
	return save, nil
}

// topLayer copies the top layer of a docker save stream raw, whiteouts
// included, to out.
func topLayer(save io.ReadSeeker, out io.Writer) error {
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
			return err
		}
		if strings.TrimPrefix(header.Name, "./") == "manifest.json" {
			if err := json.NewDecoder(reader).Decode(&manifest); err != nil {
				return fmt.Errorf("decode save manifest: %w", err)
			}
		}
	}
	if len(manifest) == 0 || len(manifest[0].Layers) == 0 {
		return errors.New("docker save stream has no layers in its manifest")
	}
	top := strings.TrimPrefix(manifest[0].Layers[len(manifest[0].Layers)-1], "./")
	if _, err := save.Seek(0, io.SeekStart); err != nil {
		return err
	}
	reader = tar.NewReader(save)
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			return fmt.Errorf("layer %s is missing from the save stream", top)
		}
		if err != nil {
			return err
		}
		if strings.TrimPrefix(header.Name, "./") != top {
			continue
		}
		buffered := bufio.NewReader(reader)
		if magic, err := buffered.Peek(2); err == nil && magic[0] == 0x1f && magic[1] == 0x8b {
			unzipped, err := gzip.NewReader(buffered)
			if err != nil {
				return err
			}
			_, err = io.Copy(out, unzipped)
			return err
		}
		_, err = io.Copy(out, buffered)
		return err
	}
}

// layerContentHash hashes a layer tar by what is in it: every entry's
// name, type, mode, size, link target and content, in name order.
func layerContentHash(layer io.Reader) (string, error) {
	reader := tar.NewReader(layer)
	var lines []string
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return "", err
		}
		hasher := sha256.New()
		if header.Typeflag == tar.TypeReg {
			if _, err := io.Copy(hasher, reader); err != nil {
				return "", err
			}
		}
		lines = append(lines, fmt.Sprintf("%s\x00%c\x00%o\x00%d\x00%s\x00%x", strings.TrimPrefix(header.Name, "./"), header.Typeflag, header.Mode, header.Size, header.Linkname, hasher.Sum(nil)))
	}
	sort.Strings(lines)
	sum := sha256.Sum256([]byte(strings.Join(lines, "\n")))
	return hex.EncodeToString(sum[:]), nil
}

// exportDelta writes the archive form of a delta layer: the note and the
// raw top layer, gzip over both.
func (d *Docker) exportDelta(ctx context.Context, snapshot domain.CapsuleSnapshot, destination io.Writer) error {
	save, err := d.saveImage(ctx, snapshot.Ref)
	if err != nil {
		return err
	}
	defer func() {
		_ = save.Close()
		_ = os.Remove(save.Name())
	}()
	top, err := os.CreateTemp("", "spin-delta-*.tar")
	if err != nil {
		return err
	}
	defer func() {
		_ = top.Close()
		_ = os.Remove(top.Name())
	}()
	if err := topLayer(save, top); err != nil {
		return err
	}
	if _, err := top.Seek(0, io.SeekStart); err != nil {
		return err
	}
	layerHash, err := layerContentHash(top)
	if err != nil {
		return err
	}
	size, err := top.Seek(0, io.SeekEnd)
	if err != nil {
		return err
	}
	if _, err := top.Seek(0, io.SeekStart); err != nil {
		return err
	}
	compressor, err := gzip.NewWriterLevel(destination, gzip.BestSpeed)
	if err != nil {
		return err
	}
	writer := tar.NewWriter(compressor)
	note, _ := json.Marshal(deltaHeader{Version: 1, ParentRef: snapshot.ParentRef, Content: snapshot.Content, LayerHash: layerHash})
	if err := writer.WriteHeader(&tar.Header{Name: deltaNote, Mode: 0o600, Size: int64(len(note)), Typeflag: tar.TypeReg}); err != nil {
		return err
	}
	if _, err := writer.Write(note); err != nil {
		return err
	}
	if err := writer.WriteHeader(&tar.Header{Name: "layer.tar", Mode: 0o600, Size: size, Typeflag: tar.TypeReg}); err != nil {
		return err
	}
	if _, err := io.Copy(writer, top); err != nil {
		return err
	}
	if err := writer.Close(); err != nil {
		return err
	}
	return compressor.Close()
}

// readDeltaNote tells whether an archive stream is a delta, without
// consuming it: the first entry of a delta is its note.
func readDeltaNote(source io.ReadSeeker) (*deltaHeader, error) {
	defer source.Seek(0, io.SeekStart)
	unzipped, err := gzip.NewReader(source)
	if err != nil {
		return nil, nil
	}
	reader := tar.NewReader(unzipped)
	header, err := reader.Next()
	if err != nil || strings.TrimPrefix(header.Name, "./") != deltaNote {
		return nil, nil
	}
	var note deltaHeader
	if err := json.NewDecoder(io.LimitReader(reader, 1<<16)).Decode(&note); err != nil {
		return nil, fmt.Errorf("read delta note: %w", err)
	}
	return &note, nil
}

// importDelta rebuilds a layer from its parent and its archived difference.
func (d *Docker) importDelta(ctx context.Context, snapshot domain.CapsuleSnapshot, note *deltaHeader, source io.ReadSeeker) error {
	parentRef := note.ParentRef
	if parentRef == "" {
		parentRef = snapshot.ParentRef
	}
	if parentRef == "" {
		return errors.New("delta archive names no parent")
	}
	if id, _, err := d.run(ctx, "image", "inspect", "--format", "{{.Id}}", parentRef); err != nil || strings.TrimSpace(id) == "" {
		return fmt.Errorf("parent %s of %s is not on this runner", parentRef, snapshot.Ref)
	}
	unzipped, err := gzip.NewReader(source)
	if err != nil {
		return err
	}
	reader := tar.NewReader(unzipped)
	var layer *os.File
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		if strings.TrimPrefix(header.Name, "./") != "layer.tar" {
			continue
		}
		layer, err = os.CreateTemp("", "spin-delta-layer-*.tar")
		if err != nil {
			return err
		}
		defer func() {
			_ = layer.Close()
			_ = os.Remove(layer.Name())
		}()
		if _, err := io.Copy(layer, reader); err != nil {
			return err
		}
		break
	}
	if layer == nil {
		return errors.New("delta archive has no layer")
	}
	if _, err := layer.Seek(0, io.SeekStart); err != nil {
		return err
	}
	layerHash, err := layerContentHash(layer)
	if err != nil {
		return err
	}
	if note.LayerHash != "" && layerHash != note.LayerHash {
		return fmt.Errorf("delta of %s is damaged: layer hash %s, expected %s", snapshot.Ref, layerHash, note.LayerHash)
	}
	parentContent, err := d.imageIdentity(ctx, parentRef)
	if err != nil {
		return err
	}
	content := chainContent(parentContent, layerHash)
	if snapshot.Content != "" && content != snapshot.Content {
		return fmt.Errorf("rebuilt %s would have content %s, expected %s: the parent on this runner is another version", snapshot.Ref, content, snapshot.Content)
	}
	if _, err := layer.Seek(0, io.SeekStart); err != nil {
		return err
	}
	clean, err := os.CreateTemp("", "spin-delta-clean-*.tar")
	if err != nil {
		return err
	}
	defer func() {
		_ = clean.Close()
		_ = os.Remove(clean.Name())
	}()
	deletions, err := filterWhiteouts(layer, clean)
	if err != nil {
		return err
	}
	if _, err := clean.Seek(0, io.SeekStart); err != nil {
		return err
	}
	targetName := runtimeName("spin-delta-build", strings.TrimPrefix(snapshot.Ref, "spin/artifact:"))
	_ = d.removeContainer(context.Background(), targetName)
	if _, err := d.control(ctx,
		"run", "-d", "--name", targetName,
		"--label", "spin.managed=true", "--label", "spin.kind=delta-build",
		"--network", "none",
		"--entrypoint", "sh", parentRef, "-lc", "trap 'exit 0' TERM INT; while :; do sleep 3600; done",
	); err != nil {
		return fmt.Errorf("create delta build from %s: %w", parentRef, err)
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
	copier.Stdin = clean
	var copyError bytes.Buffer
	copier.Stderr = &copyError
	if err := copier.Run(); err != nil {
		return fmt.Errorf("docker cp of the delta: %s: %w", strings.TrimSpace(copyError.String()), err)
	}
	if _, err := d.control(ctx,
		"commit", "--pause=true",
		"--change", "LABEL spin.managed=true",
		"--change", "LABEL "+contentLabel+"="+content,
		targetName, snapshot.Ref,
	); err != nil {
		return fmt.Errorf("commit the rebuilt layer: %w", err)
	}
	return nil
}
