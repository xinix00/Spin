package server

import (
	"bytes"
	"embed"
)

// frontendAssetVersion is part of every browser-asset URL. Increment it for
// every frontend change; immutable CDN caches may retain older asset paths.
const frontendAssetVersion = "5"

//go:embed ui.html
var dashboardHTML []byte

var dashboardDocument = bytes.ReplaceAll(dashboardHTML, []byte("__SPIN_UI_VERSION__"), []byte(frontendAssetVersion))

// dashboardAssets contains Spin's stylesheet, application script and pinned
// browser dependencies. They are served from Spin itself so the UI keeps
// working in offline workspaces.
//
//go:embed assets/spin.css assets/spin.js assets/vendor/*
var dashboardAssets embed.FS
