package capsule

import (
	"sort"

	"easyacp/internal/domain"
)

// What a layer wrote is what it owns. Sealing keeps a layer to its real
// difference from the layer under it; the end of a Session lists what the
// agent changed outside the workspace. Nothing is sorted into kinds and
// nothing is chosen: which files travel between Sessions is what the
// person ticks in the list.

// contentEntry is one file of a diff.
type contentEntry struct {
	Path string
	Size int64
}

// summarize turns entries into the manifest a layer or a Session carries.
func summarize(entries []contentEntry) domain.LayerContents {
	contents := domain.LayerContents{}
	for _, entry := range entries {
		contents.Files++
		contents.Bytes += entry.Size
	}
	sorted := append([]contentEntry(nil), entries...)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i].Size > sorted[j].Size })
	for index, entry := range sorted {
		if index >= ManifestEntryLimit {
			break
		}
		contents.Entries = append(contents.Entries, domain.ContentEntry{Path: entry.Path, Bytes: entry.Size})
	}
	return contents
}

// ManifestEntryLimit bounds the listing a manifest carries.
const ManifestEntryLimit = 50000

// insideWorkspace reports whether a path lies in the workspace.
func insideWorkspace(name string) bool {
	return name == "/workspace" || len(name) > len("/workspace/") && name[:len("/workspace/")] == "/workspace/"
}
