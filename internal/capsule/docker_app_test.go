//go:build !tamago

package capsule

import "testing"

func TestAppNamesAreStableAndSafe(t *testing.T) {
	if got := appContainerName("ses_abc", "Web API"); got != "spin-app-ses_abc-web-api" {
		t.Fatalf("container name = %q", got)
	}
	if got := appNetworkName("ses_abc"); got != "spin-app-ses_abc" {
		t.Fatalf("network name = %q", got)
	}
}
