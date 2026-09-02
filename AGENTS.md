# Spin repository instructions

## Frontend asset version

- `internal/server/ui.go` contains the monotonically increasing `frontendAssetVersion`.
- Every change affecting `internal/server/ui.html`, `internal/server/assets/spin.css`, `internal/server/assets/spin.js`, or a file in `internal/server/assets/vendor/` MUST increment that number in the same commit.
- Never reuse or decrement a frontend asset version. Cloudflare and browsers cache `/assets/v<version>/...` as immutable.
- Keep HTML and API responses non-cacheable; do not remove the CDN cache-control headers from `preventCaching`.
- After a frontend change, run `node --check internal/server/assets/spin.js` and `go test ./internal/server`.
