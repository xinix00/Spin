package capsule

import (
	"archive/tar"
	"bytes"
	"testing"
)

func TestClassifyPathNamesTheKinds(t *testing.T) {
	cases := map[string]string{
		"/usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.js": KindTool,
		"/root/.claude/.credentials.json":                              KindLogin,
		"/home/dev/.codex/auth.json":                                   KindLogin,
		"/root/.claude.json":                                           KindLogin,
		"/root/.npm/_cacache/index-v5/x":                               KindCache,
		"/root/.cache/pip/wheels/a.whl":                                KindCache,
		"/tmp/build.log":                                               KindCache,
		"/root/notes.md":                                               KindHome,
		"/etc/spin/enabled/acp.env":                                    KindConfig,
		"/etc/hosts":                                                   KindSystem,
		"/workspace/main.go":                                           KindWorkspace,
		"/var/lib/apt/lists/x":                                         KindData,
	}
	for name, want := range cases {
		if got := ClassifyPath(name); got != want {
			t.Fatalf("ClassifyPath(%q) = %s, want %s", name, got, want)
		}
	}
}

// A diff is read file by file with its kind and hash; the summary orders
// kinds by size and names the largest files of each.
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
	if summary.Files != 3 || summary.Kinds[0].Kind != KindTool || summary.Kinds[0].Largest[0].Path != "/usr/local/bin/tool" {
		t.Fatalf("summary = %+v", summary)
	}
	var kept bytes.Buffer
	if err := filterTar(bytes.NewReader(raw), &kept, func(name string) bool { return ClassifyPath(name) != KindCache }); err != nil {
		t.Fatal(err)
	}
	remaining, _, err := readDiffEntries(&kept)
	if err != nil || len(remaining) != 2 {
		t.Fatalf("kept = %+v, %v", remaining, err)
	}
}
