package server

import (
	"context"
	"testing"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
)

// Tests build layers the way the UI does, with typed requests; these
// helpers keep that short. A layer is recorded, gets one install command
// and is saved.
type layerSpec struct {
	Kind    domain.ArtifactKind
	Name    string
	Scope   domain.ArtifactScope
	From    string // kind:name of the parent's current version
	Enables []domain.Enablement
	Install string // one shell command run in the recording
}

func toolLayer(name string) layerSpec {
	return layerSpec{Kind: domain.ArtifactTool, Name: name, Scope: domain.ScopeGlobal, Install: "install " + name}
}

func gitLayer() layerSpec {
	spec := toolLayer("git")
	spec.Enables = []domain.Enablement{{Name: "git"}}
	return spec
}

// agentLayer is an ACP agent built on tool:git.
func agentLayer(name, command string) layerSpec {
	spec := toolLayer(name)
	spec.From = "tool:git"
	spec.Enables = []domain.Enablement{{Name: "acp", Command: command}}
	return spec
}

func (spec layerSpec) request(t *testing.T, srv *Server, actor string) domain.CreateRecordingRequest {
	t.Helper()
	req := domain.CreateRecordingRequest{Actor: actor, Kind: spec.Kind, Name: spec.Name, Scope: spec.Scope, Enables: spec.Enables}
	if spec.From != "" {
		parent := latestLayer(t, srv, actor, spec.From)
		req.ParentArtifactIDs = []string{parent.ID}
	}
	return req
}

func latestLayer(t *testing.T, srv *Server, actor, selector string) domain.Artifact {
	t.Helper()
	kind, name, _ := splitSelector(selector)
	artifact, err := srv.store.LatestArtifact(kind, name, actor, "default")
	if err != nil {
		t.Fatalf("latest %s: %v", selector, err)
	}
	return artifact
}

func splitSelector(selector string) (domain.ArtifactKind, string, bool) {
	for index := 0; index < len(selector); index++ {
		if selector[index] == ':' {
			return domain.ArtifactKind(selector[:index]), selector[index+1:], true
		}
	}
	return "", selector, false
}

// startLayer begins a recording and returns what the API returns: the
// recording, and the start job when the capsule is not up yet.
func startLayer(t *testing.T, srv *Server, actor string, spec layerSpec) (domain.Recording, *domain.StartStatus) {
	t.Helper()
	recording, start, err := srv.createCapsuleRecording(spec.request(t, srv, actor))
	if err != nil {
		t.Fatalf("record %s:%s: %v", spec.Kind, spec.Name, err)
	}
	return recording, start
}

// recordLayer begins a recording whose capsule is up right away.
func recordLayer(t *testing.T, srv *Server, actor string, spec layerSpec) domain.Recording {
	t.Helper()
	recording, start := startLayer(t, srv, actor, spec)
	if start != nil {
		t.Fatalf("record %s:%s is still starting: %+v", spec.Kind, spec.Name, start)
	}
	return recording
}

// runInRecording runs one command in the operator's open recording.
func runInRecording(t *testing.T, srv *Server, actor, input string) (domain.Recording, capsule.Execution) {
	t.Helper()
	open, err := srv.store.OpenRecording(actor)
	if err != nil {
		t.Fatalf("no open recording for %s: %v", actor, err)
	}
	recording, execution, err := srv.executeRecordingCommand(context.Background(), open.ID, domain.ExecuteRecordingCommandRequest{Actor: actor, Input: input})
	if err != nil {
		t.Fatalf("%s: %v", input, err)
	}
	return recording, execution
}

// endLayer ends the operator's open recording and returns what the API
// returns: the artifact, or the seal job when saving takes long.
func endLayer(srv *Server, actor string) (domain.Artifact, *domain.SealStatus, error) {
	open, err := srv.store.OpenRecording(actor)
	if err != nil {
		return domain.Artifact{}, nil, err
	}
	return srv.endCapsuleRecording(open.ID, domain.EndRecordingRequest{Actor: actor})
}

// saveLayer ends the open recording and expects the artifact at once.
func saveLayer(t *testing.T, srv *Server, actor string) domain.Artifact {
	t.Helper()
	artifact, seal, err := endLayer(srv, actor)
	if err != nil {
		t.Fatalf("end recording: %v", err)
	}
	if seal != nil {
		t.Fatalf("end recording is still sealing: %+v", seal)
	}
	return artifact
}

func cancelLayer(srv *Server, actor string) (domain.Recording, error) {
	open, err := srv.store.OpenRecording(actor)
	if err != nil {
		return domain.Recording{}, err
	}
	return srv.cancelCapsuleRecording(context.Background(), open.ID, domain.CancelRecordingRequest{Actor: actor})
}

// buildLayers records, installs and saves every spec in order.
func buildLayers(t *testing.T, srv *Server, actor string, specs ...layerSpec) []domain.Artifact {
	t.Helper()
	artifacts := make([]domain.Artifact, 0, len(specs))
	for _, spec := range specs {
		recordLayer(t, srv, actor, spec)
		if spec.Install != "" {
			runInRecording(t, srv, actor, spec.Install)
		}
		artifacts = append(artifacts, saveLayer(t, srv, actor))
	}
	return artifacts
}

// useLayers starts a composition of a selector with extra layers.
func useLayers(t *testing.T, srv *Server, actor, selector string, with ...string) domain.Composition {
	t.Helper()
	composition, err := srv.useCapsule(context.Background(), domain.UseRequest{Operator: actor, Selector: selector, WithSelectors: with, Profile: "default"})
	if err != nil {
		t.Fatalf("use %s: %v", selector, err)
	}
	return composition
}

// editLayer starts an EDIT of the current version of a selector.
func editLayer(t *testing.T, srv *Server, actor, selector string) domain.Recording {
	t.Helper()
	current := latestLayer(t, srv, actor, selector)
	recording, start, err := srv.editCapsuleArtifact(actor, current.ID)
	if err != nil {
		t.Fatalf("edit %s: %v", selector, err)
	}
	if start != nil {
		t.Fatalf("edit %s is still starting: %+v", selector, start)
	}
	return recording
}
