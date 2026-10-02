# Spin repository instructions

## Frontend asset version

- `crates/runtime/ui/VERSION` contains the monotonically increasing frontend asset version.
- Every change affecting `crates/runtime/ui/ui.html` or a file under `crates/runtime/ui/assets/` MUST increment that number in the same commit.
- Never reuse or decrement a frontend asset version. Cloudflare and browsers cache `/assets/v<version>/...` as immutable.
- Keep HTML and API responses non-cacheable; do not remove the CDN cache-control headers.
- After a frontend change, run `node --check crates/runtime/ui/assets/spin.js` and `cargo test --offline -p spin-host --test http`.
