package buildinfo

import (
	"fmt"
	"runtime"
	"strings"
)

// These values are deliberately plain variables so release.sh can stamp both
// binaries with the same provenance through -ldflags -X.
var (
	Version = "dev"
	Commit  = "unknown"
	BuiltAt = "unknown"
)

func String(name string) string {
	parts := []string{name, Version, runtime.GOOS + "/" + runtime.GOARCH}
	if Commit != "" && Commit != "unknown" {
		parts = append(parts, "commit "+Commit)
	}
	if BuiltAt != "" && BuiltAt != "unknown" {
		parts = append(parts, "built "+BuiltAt)
	}
	return strings.Join(parts, " · ")
}

func Print(name string) {
	fmt.Println(String(name))
}
