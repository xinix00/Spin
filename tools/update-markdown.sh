#!/usr/bin/env bash
# Refresh the vendored copy of xinix00/markdown to its newest release. The
# editor is Derek's own library and follows along without pinning; every other
# vendored dependency stays pinned.
set -euo pipefail
cd "$(dirname "$0")/.."
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
gh repo clone xinix00/markdown "$tmp/markdown" -- --depth 1 --quiet
for file in markdown.js markdown.css; do
  cp "$tmp/markdown/$file" "internal/server/assets/vendor/$file"
done
cp "$tmp/markdown/LICENSE" internal/server/assets/vendor/LICENSE.markdown
version="$(sed -n "s/.*version: '\([0-9.]*\)'.*/\1/p" internal/server/assets/vendor/markdown.js | tail -n 1)"
echo "vendored xinix00/markdown $version; bump frontendAssetVersion in internal/server/ui.go"
