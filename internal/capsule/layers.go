package capsule

import (
	"fmt"

	"easyacp/internal/domain"
)

// A composition is a stack of layer versions, bottom to top. Its filesystem
// is every layer's own diff (what that recording changed) applied in that
// order. The runner does not rebuild that from the bottom: the deepest layer
// whose image is exactly the stack up to itself is the base, and only the
// layers above it are applied, each as its own diff. A layer whose diff is
// gone (a pruned older version) is carried by the first later version of it
// in the stack, copied whole; a layer whose parents are not under it (the
// root of another chain) is copied whole as well.

// LayerStep is one layer applied over the base.
type LayerStep struct {
	Artifact domain.Artifact
	// Full copies the layer's whole filesystem instead of its own diff.
	Full bool
}

// LayerPlan is how a runner builds a composition's image.
type LayerPlan struct {
	Base  domain.Artifact
	Steps []LayerStep
}

// Needed lists the layers whose images the runner must hold for this plan.
func (p LayerPlan) Needed() []domain.Artifact {
	needed := []domain.Artifact{p.Base}
	for _, step := range p.Steps {
		needed = append(needed, step.Artifact)
	}
	return needed
}

// CompositionLayers is the stack order of a composition: its Layers, or for
// a composition from before stacks were recorded, the order its layers were
// resolved in (each layer after its parents, newer versions last).
func CompositionLayers(composition domain.Composition) []string {
	if len(composition.Layers) > 0 {
		return composition.Layers
	}
	order := make([]string, 0, len(composition.ResolvedArtifacts))
	for _, resolved := range composition.ResolvedArtifacts {
		order = append(order, resolved.ArtifactID)
	}
	return order
}

func layerRestorable(artifact domain.Artifact) bool {
	return artifact.Snapshot.Driver == "docker" && artifact.Snapshot.Restorable && artifact.Snapshot.Ref != "" && artifact.SnapshotPrunedAt == nil
}

// PlanLayers turns a composition's stack into a base image and the steps
// over it.
func PlanLayers(composition domain.Composition, artifacts []domain.Artifact) (LayerPlan, error) {
	byID := make(map[string]domain.Artifact, len(artifacts))
	for _, artifact := range artifacts {
		byID[artifact.ID] = artifact
	}
	order := make([]string, 0, len(artifacts))
	position := map[string]int{}
	for _, id := range CompositionLayers(composition) {
		if _, ok := byID[id]; !ok {
			continue
		}
		if _, seen := position[id]; seen {
			continue
		}
		position[id] = len(order)
		order = append(order, id)
	}
	if len(order) == 0 {
		return LayerPlan{}, fmt.Errorf("composition %s has no layers", composition.ID)
	}
	closure := func(id string) map[string]bool {
		found := map[string]bool{}
		var walk func(string)
		walk = func(id string) {
			if found[id] {
				return
			}
			artifact, ok := byID[id]
			if !ok {
				return
			}
			found[id] = true
			for _, parentID := range artifact.ParentArtifactIDs {
				walk(parentID)
			}
		}
		walk(id)
		return found
	}
	// The base: the deepest layer whose image is exactly the stack up to it.
	baseIndex := -1
	for index, id := range order {
		if !layerRestorable(byID[id]) {
			continue
		}
		members := closure(id)
		if len(members) != index+1 {
			continue
		}
		exact := true
		for _, below := range order[:index+1] {
			if !members[below] {
				exact = false
				break
			}
		}
		if exact {
			baseIndex = index
		}
	}
	if baseIndex < 0 {
		return LayerPlan{}, fmt.Errorf("layer %s at the bottom of the stack has no restorable image", order[0])
	}
	plan := LayerPlan{Base: byID[order[baseIndex]]}
	carried := map[string]bool{}
	for index := baseIndex + 1; index < len(order); index++ {
		artifact := byID[order[index]]
		if !layerRestorable(artifact) {
			// Its diff is gone; the first later version of it in the stack
			// carries its content and is copied whole.
			next, found := artifact.SupersededBy, false
			for depth := 0; next != "" && depth < 64; depth++ {
				newer, ok := byID[next]
				if !ok {
					break
				}
				if at, inStack := position[next]; inStack && at > index && layerRestorable(newer) {
					carried[next], found = true, true
					break
				}
				next = newer.SupersededBy
			}
			if !found {
				return LayerPlan{}, fmt.Errorf("layer %s has no restorable image and no newer version above it in the stack", artifact.ID)
			}
			continue
		}
		full := carried[artifact.ID] || len(artifact.ParentArtifactIDs) == 0
		for _, parentID := range artifact.ParentArtifactIDs {
			if at, inStack := position[parentID]; !inStack || at >= index {
				full = true
			}
		}
		plan.Steps = append(plan.Steps, LayerStep{Artifact: artifact, Full: full})
	}
	return plan, nil
}
