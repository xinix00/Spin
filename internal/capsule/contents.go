package capsule

import (
	"path"
	"sort"
	"strings"

	"easyacp/internal/domain"
)

// What a layer wrote is what it owns, and every path falls into a kind:
// the tool a layer installs, the login an agent keeps under HOME, the
// configuration Spin reads, the workspace, or a cache that never belongs in
// a layer. Sealing uses the kinds to keep a layer to its real difference;
// the end of a Session uses them to say what the agent changed outside the
// workspace.

const (
	KindTool      = "tool"
	KindLogin     = "login"
	KindHome      = "home"
	KindConfig    = "config"
	KindWorkspace = "workspace"
	KindCache     = "cache"
	KindData      = "data"
	KindSystem    = "system"
)

var loginDirs = []string{".claude", ".codex", ".gemini", ".config/gh", ".config/opencode", ".local/share/opencode", ".ssh", ".netrc", ".git-credentials", ".config/gcloud", ".aws", ".docker"}

var cacheDirs = []string{".cache", ".npm", ".yarn/cache", ".pnpm-store", ".cargo/registry", ".cargo/git", ".gradle/caches", ".m2/repository", ".nuget/packages", "go/pkg/mod/cache", ".bun/install/cache", ".local/share/pnpm/store", ".config/Code/Cache"}

// ClassifyPath names the kind of an absolute path in a capsule.
func ClassifyPath(name string) string {
	clean := path.Clean("/" + strings.TrimPrefix(strings.TrimPrefix(name, "./"), "/"))
	home, ok := homeRelative(clean)
	if ok {
		for _, dir := range cacheDirs {
			if home == dir || strings.HasPrefix(home, dir+"/") {
				return KindCache
			}
		}
		for _, dir := range loginDirs {
			if home == dir || strings.HasPrefix(home, dir+"/") {
				return KindLogin
			}
		}
		if home == ".claude.json" {
			return KindLogin
		}
		return KindHome
	}
	switch {
	case clean == "/workspace" || strings.HasPrefix(clean, "/workspace/"):
		return KindWorkspace
	case strings.HasPrefix(clean, "/tmp/"), strings.HasPrefix(clean, "/var/tmp/"), strings.HasPrefix(clean, "/var/cache/"), strings.HasPrefix(clean, "/var/log/"), strings.HasPrefix(clean, "/run/"):
		return KindCache
	case strings.HasPrefix(clean, "/etc/spin/"):
		return KindConfig
	case strings.HasPrefix(clean, "/etc/"):
		return KindSystem
	case strings.HasPrefix(clean, "/usr/"), strings.HasPrefix(clean, "/opt/"), strings.HasPrefix(clean, "/bin/"), strings.HasPrefix(clean, "/sbin/"), strings.HasPrefix(clean, "/lib/"), strings.HasPrefix(clean, "/lib64/"), strings.HasPrefix(clean, "/spin/"):
		return KindTool
	case strings.HasPrefix(clean, "/var/lib/"), strings.HasPrefix(clean, "/srv/"), strings.HasPrefix(clean, "/data/"):
		return KindData
	}
	return KindSystem
}

// homeRelative strips /root or /home/<user> off a path.
func homeRelative(clean string) (string, bool) {
	if clean == "/root" {
		return "", true
	}
	if strings.HasPrefix(clean, "/root/") {
		return strings.TrimPrefix(clean, "/root/"), true
	}
	if strings.HasPrefix(clean, "/home/") {
		rest := strings.TrimPrefix(clean, "/home/")
		_, relative, ok := strings.Cut(rest, "/")
		if !ok {
			return "", true
		}
		return relative, true
	}
	return "", false
}

// contentEntry is one file of a diff after classification.
type contentEntry struct {
	Path string
	Size int64
	Kind string
}

// summarize turns entries into the manifest a layer or a Session carries.
func summarize(entries []contentEntry) domain.LayerContents {
	contents := domain.LayerContents{}
	byKind := map[string]*domain.ContentKind{}
	for _, entry := range entries {
		contents.Files++
		contents.Bytes += entry.Size
		kind := byKind[entry.Kind]
		if kind == nil {
			kind = &domain.ContentKind{Kind: entry.Kind}
			byKind[entry.Kind] = kind
		}
		kind.Files++
		kind.Bytes += entry.Size
	}
	sorted := append([]contentEntry(nil), entries...)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i].Size > sorted[j].Size })
	for _, entry := range sorted {
		kind := byKind[entry.Kind]
		if len(kind.Largest) < 5 {
			kind.Largest = append(kind.Largest, domain.ContentPath{Path: entry.Path, Bytes: entry.Size})
		}
	}
	for _, kind := range byKind {
		contents.Kinds = append(contents.Kinds, *kind)
	}
	sort.Slice(contents.Kinds, func(i, j int) bool { return contents.Kinds[i].Bytes > contents.Kinds[j].Bytes })
	for index, entry := range sorted {
		if index >= ManifestEntryLimit {
			break
		}
		contents.Entries = append(contents.Entries, domain.ContentEntry{Path: entry.Path, Bytes: entry.Size, Kind: entry.Kind})
	}
	return contents
}

// ManifestEntryLimit bounds the listing a manifest carries.
const ManifestEntryLimit = 50000
