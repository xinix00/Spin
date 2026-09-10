package capsule

import (
	"archive/tar"
	"bytes"
	"strings"
	"testing"
)

// A diff is read file by file with its hash; the summary lists the files
// largest first.
func TestReadDiffEntriesAndSummary(t *testing.T) {
	var buffer bytes.Buffer
	writer := tar.NewWriter(&buffer)
	add := func(name string, content string) {
		if err := writer.WriteHeader(&tar.Header{Name: name, Size: int64(len(content)), Typeflag: tar.TypeReg, Mode: 0o644}); err != nil {
			t.Fatal(err)
		}
		if _, err := writer.Write([]byte(content)); err != nil {
			t.Fatal(err)
		}
	}
	add("usr/local/bin/tool", "binary-binary-binary")
	add("root/.claude/.credentials.json", "{}")
	add("root/.npm/_cacache/x", "cache")
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	raw := append([]byte(nil), buffer.Bytes()...)
	entries, hashes, err := readDiffEntries(bytes.NewReader(raw))
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 3 || len(hashes) != 3 || hashes["/usr/local/bin/tool"] == "" {
		t.Fatalf("entries = %+v hashes = %d", entries, len(hashes))
	}
	summary := summarize(entries)
	if summary.Files != 3 || summary.Entries[0].Path != "/usr/local/bin/tool" {
		t.Fatalf("summary = %+v", summary)
	}
	var kept bytes.Buffer
	if err := filterTar(bytes.NewReader(raw), &kept, func(name string) bool { return !strings.Contains(name, "_cacache") }); err != nil {
		t.Fatal(err)
	}
	remaining, _, err := readDiffEntries(&kept)
	if err != nil || len(remaining) != 2 {
		t.Fatalf("kept = %+v, %v", remaining, err)
	}
}

// A layer's content hash depends on what is in the tar, not on the order
// its entries come in; the top layer of a save stream comes out raw, and a
// delta note is recognised without consuming the stream.
func TestLayerContentHashAndTopLayer(t *testing.T) {
	entry := func(w *tar.Writer, name, content string) {
		if err := w.WriteHeader(&tar.Header{Name: name, Size: int64(len(content)), Typeflag: tar.TypeReg, Mode: 0o644}); err != nil {
			t.Fatal(err)
		}
		if _, err := w.Write([]byte(content)); err != nil {
			t.Fatal(err)
		}
	}
	var one, two bytes.Buffer
	w := tar.NewWriter(&one)
	entry(w, "root/.codex/auth.json", `{"token":"a"}`)
	entry(w, "usr/local/bin/x", "bin")
	_ = w.Close()
	w = tar.NewWriter(&two)
	entry(w, "usr/local/bin/x", "bin")
	entry(w, "root/.codex/auth.json", `{"token":"a"}`)
	_ = w.Close()
	hashOne, err := layerContentHash(bytes.NewReader(one.Bytes()))
	if err != nil {
		t.Fatal(err)
	}
	hashTwo, err := layerContentHash(bytes.NewReader(two.Bytes()))
	if err != nil || hashOne != hashTwo {
		t.Fatalf("hashes differ by order: %s %s %v", hashOne, hashTwo, err)
	}
	var changed bytes.Buffer
	w = tar.NewWriter(&changed)
	entry(w, "root/.codex/auth.json", `{"token":"b"}`)
	entry(w, "usr/local/bin/x", "bin")
	_ = w.Close()
	if hashChanged, _ := layerContentHash(bytes.NewReader(changed.Bytes())); hashChanged == hashOne {
		t.Fatal("a changed file kept the hash")
	}

	// A save stream: two layers and a manifest naming the top one last.
	var save bytes.Buffer
	w = tar.NewWriter(&save)
	entry(w, "aaa/layer.tar", "lower")
	entry(w, "bbb/layer.tar", string(one.Bytes()))
	entry(w, "manifest.json", `[{"Layers":["aaa/layer.tar","bbb/layer.tar"]}]`)
	_ = w.Close()
	var top bytes.Buffer
	if err := topLayer(bytes.NewReader(save.Bytes()), &top); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(top.Bytes(), one.Bytes()) {
		t.Fatalf("top layer = %d bytes, want %d", top.Len(), one.Len())
	}
	if chainContent("a", "b") == chainContent("b", "a") || !strings.HasPrefix(chainContent("a", "b"), "content:") {
		t.Fatal("chain content is not ordered or not marked")
	}
}
