//go:build !tamago

package capsule

import (
	"archive/tar"
	"bytes"
	"encoding/json"
	"io"
	"slices"
	"strings"
	"testing"
	"time"

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
func restorable(id string, parents ...string) domain.Artifact {
	return domain.Artifact{ID: id, ParentArtifactIDs: parents, Snapshot: domain.CapsuleSnapshot{Driver: "docker", Ref: "image:" + id, Restorable: true}}
}

func planIDs(plan LayerPlan) []string {
	ids := []string{plan.Base.ID + "=base"}
	for _, step := range plan.Steps {
		action := "diff"
		if step.Full {
			action = "full"
		}
		ids = append(ids, step.Artifact.ID+"="+action)
	}
	return ids
}

// The base is the deepest layer whose image is exactly the stack up to it;
// what lies above is applied as its own diff, an unrelated root whole.
func TestPlanLayersTakesTheDeepestExactPrefixAsBase(t *testing.T) {
	git, node := restorable("git"), restorable("node", "git")
	codex := restorable("codex", "node")
	credential := restorable("cred", "codex")
	dotnet := restorable("dotnet")
	composition := domain.Composition{Layers: []string{"git", "node", "codex", "cred", "dotnet"}}
	plan, err := PlanLayers(composition, []domain.Artifact{git, node, codex, credential, dotnet})
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(planIDs(plan), " "); got != "cred=base dotnet=full" {
		t.Fatalf("plan = %s", got)
	}
}

// A credential recorded on an older tool: the tool's EDIT sits above the
// old version in the stack, below the credential, and the credential's image
// is no longer an exact prefix. The newest tool is the base and the
// credential goes over it as its diff.
func TestPlanLayersAppliesTheCredentialOverAnEditedTool(t *testing.T) {
	git := restorable("git")
	claudeV1 := restorable("claude1", "git")
	claudeV1.SupersededBy = "claude2"
	claudeV2 := restorable("claude2", "claude1")
	credential := restorable("cred", "claude1")
	composition := domain.Composition{Layers: []string{"git", "claude1", "claude2", "cred"}}
	plan, err := PlanLayers(composition, []domain.Artifact{git, claudeV1, claudeV2, credential})
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(planIDs(plan), " "); got != "claude2=base cred=diff" {
		t.Fatalf("plan = %s", got)
	}
}

// A pruned older version above the base has no diff any more; the first
// later version of it in the stack is copied whole instead.
func TestPlanLayersCarriesAPrunedVersionByItsSuccessor(t *testing.T) {
	dotnet := restorable("dotnet")
	git := restorable("git")
	claudeV1 := restorable("claude1", "git")
	claudeV1.SupersededBy = "claude2"
	pruned := time.Now()
	claudeV1.SnapshotPrunedAt = &pruned
	claudeV2 := restorable("claude2", "claude1")
	credential := restorable("cred", "claude1")
	composition := domain.Composition{Layers: []string{"dotnet", "git", "claude1", "claude2", "cred"}}
	plan, err := PlanLayers(composition, []domain.Artifact{dotnet, git, claudeV1, claudeV2, credential})
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(planIDs(plan), " "); got != "dotnet=base git=full claude2=full cred=diff" {
		t.Fatalf("plan = %s", got)
	}
}

// A composition from before stacks were recorded is planned in the order
// its layers were resolved.
func TestPlanLayersFallsBackToResolvedOrder(t *testing.T) {
	tool, credential := restorable("tool"), restorable("credential", "tool")
	composition := domain.Composition{ResolvedArtifacts: []domain.ResolvedArtifact{{ArtifactID: "tool"}, {ArtifactID: "credential"}}}
	plan, err := PlanLayers(composition, []domain.Artifact{tool, credential})
	if err != nil {
		t.Fatal(err)
	}
	if plan.Base.ID != "credential" || len(plan.Steps) != 0 {
		t.Fatalf("plan = %v", planIDs(plan))
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
