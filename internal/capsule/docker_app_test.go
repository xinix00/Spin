package capsule

import (
	"os"
	"path/filepath"
	"testing"
)

// The host's hosts file reaches the app as --add-host entries, minus the
// loopback and special names that mean something else inside a container.
func TestHostEntriesSkipLoopbackAndComments(t *testing.T) {
	path := filepath.Join(t.TempDir(), "hosts")
	if err := os.WriteFile(path, []byte(`##
127.0.0.1	localhost
255.255.255.255	broadcasthost
::1             localhost
192.168.1.40	sqlserver.easyflor.local sqlserver # our database
10.0.0.7 	tools
fe80::1%lo0	localhost
`), 0o644); err != nil {
		t.Fatal(err)
	}
	entries := hostEntries(path)
	want := []string{"sqlserver.easyflor.local:192.168.1.40", "sqlserver:192.168.1.40", "tools:10.0.0.7"}
	if len(entries) != len(want) {
		t.Fatalf("entries = %v, want %v", entries, want)
	}
	for index := range want {
		if entries[index] != want[index] {
			t.Fatalf("entries = %v, want %v", entries, want)
		}
	}
	if entries := hostEntries(filepath.Join(t.TempDir(), "missing")); entries != nil {
		t.Fatalf("missing hosts file produced %v", entries)
	}
}

func TestAppNamesAreStableAndSafe(t *testing.T) {
	if got := appContainerName("ses_abc", "Web API"); got != "spin-app-ses_abc-web-api" {
		t.Fatalf("container name = %q", got)
	}
	if got := appNetworkName("ses_abc"); got != "spin-app-ses_abc" {
		t.Fatalf("network name = %q", got)
	}
}
