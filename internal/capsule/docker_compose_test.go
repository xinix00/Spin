//go:build !tamago

package capsule

import (
	"archive/tar"
	"bytes"
	"encoding/json"
	"io"
	"slices"
	"testing"

	"easyacp/internal/domain"
)

func tarWith(entries map[string]string) []byte {
	var buffer bytes.Buffer
	writer := tar.NewWriter(&buffer)
	for name, content := range entries {
		_ = writer.WriteHeader(&tar.Header{Name: name, Mode: 0o644, Size: int64(len(content))})
		_, _ = writer.Write([]byte(content))
	}
	_ = writer.Close()
	return buffer.Bytes()
}

// The top layer of a docker save is the recording's own change; whiteouts
// become deletions and the rest is copied as a tar.
func TestLayerDiffTakesTheTopLayerAndItsWhiteouts(t *testing.T) {
	lower := tarWith(map[string]string{"usr/local/lib/node_modules/old/index.js": "old"})
	top := tarWith(map[string]string{
		"root/.codex/auth.json":                         "secret",
		"usr/local/lib/node_modules/.wh.old":            "",
		"usr/local/lib/node_modules/cache/.wh..wh..opq": "",
	})
	manifest, _ := json.Marshal([]map[string]any{{"Config": "cfg.json", "Layers": []string{"aaa/layer.tar", "bbb/layer.tar"}}})
	save := tarWith(map[string]string{"aaa/layer.tar": string(lower), "bbb/layer.tar": string(top), "manifest.json": string(manifest)})

	var diff bytes.Buffer
	deletions, err := layerDiff(bytes.NewReader(save), &diff)
	if err != nil {
		t.Fatal(err)
	}
	slices.Sort(deletions)
	if !slices.Equal(deletions, []string{"/usr/local/lib/node_modules/cache/*", "/usr/local/lib/node_modules/old"}) {
		t.Fatalf("deletions = %v", deletions)
	}
	var names []string
	reader := tar.NewReader(&diff)
	for {
		header, err := reader.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			t.Fatal(err)
		}
		names = append(names, header.Name)
	}
	if !slices.Equal(names, []string{"root/.codex/auth.json"}) {
		t.Fatalf("diff entries = %v", names)
	}
}

// The layer with the widest closure is the base; a layer recorded on a
// version inside that closure is applied as a diff, an unrelated one fully.
func TestCompositionBasePrefersTheWidestClosure(t *testing.T) {
	git := domain.Artifact{ID: "git"}
	codexV1 := domain.Artifact{ID: "codex1", ParentArtifactIDs: []string{"git"}, SupersededBy: "codex2"}
	credential := domain.Artifact{ID: "cred", ParentArtifactIDs: []string{"codex1"}}
	codexV2 := domain.Artifact{ID: "codex2", ParentArtifactIDs: []string{"codex1"}}
	other := domain.Artifact{ID: "other"}
	byID := map[string]domain.Artifact{"git": git, "codex1": codexV1, "cred": credential, "codex2": codexV2, "other": other}
	base, plan := compositionBase([]domain.Artifact{git, credential, codexV2, other}, byID)
	if base != 2 {
		t.Fatalf("base = %d, want the newest codex", base)
	}
	if plan["cred"] != layerDiffOnly || plan["other"] != layerFullCopy || plan["git"] != layerContained {
		t.Fatalf("plan = %v", plan)
	}
}

// Paths a running container owns are never copied into it.
func TestFilterExportDropsDockerManagedPaths(t *testing.T) {
	export := tarWith(map[string]string{"proc/1/status": "x", "sys/kernel": "x", "dev/null": "", "etc/hosts": "x", "etc/passwd": "root", "usr/bin/tool": "bin"})
	var out bytes.Buffer
	if err := filterExport(bytes.NewReader(export), &out); err != nil {
		t.Fatal(err)
	}
	var names []string
	reader := tar.NewReader(&out)
	for {
		header, err := reader.Next()
		if err == io.EOF {
			break
		}
		names = append(names, header.Name)
	}
	slices.Sort(names)
	if !slices.Equal(names, []string{"etc/passwd", "usr/bin/tool"}) {
		t.Fatalf("kept = %v", names)
	}
}

func TestValidGitRefAcceptsTicketBranches(t *testing.T) {
	for _, ref := range []string{"jobs/#290488/main", "jobs/#1234/vervolg-ab12cd/main", "develop", "release/1.2.3"} {
		if !validGitRef(ref) {
			t.Fatalf("%q was rejected", ref)
		}
	}
	for _, ref := range []string{"", "/jobs/main", "jobs/main/", "a..b", "jobs/ #1/main", "jobs/$x/main"} {
		if validGitRef(ref) {
			t.Fatalf("%q was accepted", ref)
		}
	}
}

// An imported image is accepted on its layers, which every image store
// reports the same; the image ID differs between the classic and the
// containerd store, so it decides nothing.
func TestImportedImageVerifiesOnLayersNotID(t *testing.T) {
	layers := `["sha256:aaa","sha256:bbb"]`
	rootFS := rootFSDigest(layers)
	if rootFS == "" || rootFS == rootFSDigest(`["sha256:aaa"]`) || rootFSDigest("not json") != "" {
		t.Fatalf("rootFS digest = %q", rootFS)
	}
	recorded := domain.CapsuleSnapshot{Driver: "docker", Ref: "spin/artifact:rec_1", Digest: "sha256:classic-id", RootFS: rootFS}
	if err := verifyImportedImage(recorded, "sha256:containerd-id", rootFS); err != nil {
		t.Fatalf("same layers, other ID: %v", err)
	}
	if err := verifyImportedImage(recorded, "sha256:classic-id", rootFSDigest(`["sha256:zzz"]`)); err == nil {
		t.Fatal("other layers were accepted")
	}
	legacy := domain.CapsuleSnapshot{Driver: "docker", Ref: "spin/artifact:rec_0", Digest: "sha256:classic-id"}
	if err := verifyImportedImage(legacy, "sha256:containerd-id", rootFS); err != nil {
		t.Fatalf("legacy snapshot without layers: %v", err)
	}
}
