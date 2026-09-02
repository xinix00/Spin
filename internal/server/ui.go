package server

import "embed"

//go:embed ui.html
var dashboardHTML []byte

// dashboardAssets contains pinned browser dependencies. They are served from
// Spin itself so Markdown and diagrams keep working in offline workspaces.
//
//go:embed assets/vendor/*
var dashboardAssets embed.FS
