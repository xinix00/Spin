#!/usr/bin/env bash
set -euo pipefail

# Build and publish one immutable Spin release plus the mutable rolling channel.
#
#   ./release.sh                 # version from Cargo.toml, build and publish
#   PUBLISH=0 ./release.sh       # compile gate only
#
# Three artifacts: the native HopOS server as an ELF slot image for arm64 and
# riscv64, and the macOS arm64 runner. The version is the workspace version in
# Cargo.toml; bump it there, commit, then release. Runs on an Apple Silicon Mac
# with Homebrew LLVM (clang for the libc-free SQLite, llvm-objcopy for the ELFs).

ROOTDIR="$(cd "$(dirname "$0")" && pwd)"
REPO="${REPO:-xinix00/Spin}"
LLVM="${LLVM:-/opt/homebrew/opt/llvm/bin}"
PUBLISH="${PUBLISH:-1}"

VERSION="v$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOTDIR/Cargo.toml" | head -1)"
if [[ ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]]; then
	echo "FOUT: Cargo.toml heeft geen semver-versie onder [workspace.package]" >&2
	exit 1
fi

OUTDIR="$ROOTDIR/dist/$VERSION"
COMMIT="$(git -C "$ROOTDIR" rev-parse HEAD)"
SHORT_COMMIT="$(git -C "$ROOTDIR" rev-parse --short=12 HEAD)"
BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

if [[ -n "$(git -C "$ROOTDIR" status --porcelain)" && -z "${RELEASE_ALLOW_DIRTY:-}" ]]; then
	echo "FOUT: Spin working tree is niet schoon; commit eerst (of RELEASE_ALLOW_DIRTY=1 voor alleen een bewuste lokale gate)." >&2
	exit 1
fi
[[ "$(uname -sm)" == "Darwin arm64" ]] || { echo "FOUT: de macOS-runner wordt op een Apple Silicon Mac gebouwd" >&2; exit 1; }
[[ -x "$LLVM/clang" && -x "$LLVM/llvm-objcopy" ]] || { echo "FOUT: Homebrew LLVM ontbreekt op $LLVM (brew install llvm, of zet LLVM)" >&2; exit 1; }
command -v gh >/dev/null || [[ "$PUBLISH" != "1" ]] || { echo "FOUT: gh ontbreekt" >&2; exit 1; }

echo "== Spin $VERSION =="
echo "   commit: $SHORT_COMMIT"

rm -rf "$OUTDIR"
mkdir -p "$OUTDIR"
export CARGO_INCREMENTAL=0 SQLITE_CC="$LLVM/clang" SQLITE_AR="$LLVM/llvm-ar"
cd "$ROOTDIR"

# A release is a clean build: cargo does not notice a changed vendored crate
# (third-party/rust, same version) and would ship the previously compiled one.
echo ">> clean release profile"
cargo clean --offline --release
for target in aarch64-unknown-none-softfloat riscv64gc-unknown-none-elf; do
	cargo clean --offline --release --target "$target"
done

ASSETS=()
echo ">> darwin/arm64 (client)"
cargo build --offline --release -p spin-host --bin spin-client
asset="$OUTDIR/spin-client-darwin-arm64"
cp target/release/spin-client "$asset"
/usr/bin/strip -x "$asset"
/usr/bin/codesign --force --sign - "$asset"
"$asset" --version | grep -q "^spin-client ${VERSION#v} " || { echo "FOUT: de runner meldt een andere versie dan $VERSION" >&2; exit 1; }
ASSETS+=("$asset")

for pair in arm64:aarch64-unknown-none-softfloat riscv64:riscv64gc-unknown-none-elf; do
	arch="${pair%%:*}"
	target="${pair#*:}"
	echo ">> HopOS/$arch"
	cargo build --offline --release -p spin-hopos --target "$target"
	asset="$OUTDIR/spin-server-$arch.elf"
	"$LLVM/llvm-objcopy" --strip-debug "target/$target/release/spin-hopos-server" "$asset"
	ASSETS+=("$asset")
done
file "$OUTDIR/spin-server-arm64.elf" | grep -q "ARM aarch64" || { echo "FOUT: HopOS arm64 artifact is geen AArch64 ELF" >&2; exit 1; }
file "$OUTDIR/spin-server-riscv64.elf" | grep -q "RISC-V" || { echo "FOUT: HopOS riscv64 artifact is geen RISC-V ELF" >&2; exit 1; }

# A build that changes the tree (a stale Cargo.lock after a version bump) is not the commit being released.
if [[ -n "$(git -C "$ROOTDIR" status --porcelain)" && -z "${RELEASE_ALLOW_DIRTY:-}" ]]; then
	echo "FOUT: de build wijzigde de working tree (Cargo.lock?); commit en herhaal." >&2
	exit 1
fi

(cd "$OUTDIR" && shasum -a 256 spin-* > SHA256SUMS)
ASSETS+=("$OUTDIR/SHA256SUMS")

cat > "$OUTDIR/RELEASE-NOTES.md" <<NOTES
Spin $VERSION ($SHORT_COMMIT), built $BUILT_AT.

Artifacts:
- spin-server-arm64.elf / spin-server-riscv64.elf: native HopOS slot images (control plane and web frontend)
- spin-client-darwin-arm64: macOS runner (needs Docker Desktop)

The runner connects outbound to SPIN_SERVER; it derives wss:// from https:// and uses /api/runner/ws. Supply the same SPIN_WORKER_TOKEN on both sides.

HopOS: publish ER_PORT_HTTP, mount /data, and set SPIN_MASTER_KEY plus the SPIN_S3_* settings; see hop-spin-server.example.json.
NOTES
ASSETS+=("$OUTDIR/RELEASE-NOTES.md")

echo ">> artifacts"
ls -lh "$OUTDIR"

[[ "$PUBLISH" == "1" ]] || { echo "KLAAR (PUBLISH=0): $OUTDIR"; exit 0; }

permission="$(gh api "repos/$REPO" --jq .permissions.push 2>/dev/null || true)"
[[ "$permission" == "true" ]] || { echo "FOUT: actieve gh-user mag niet pushen naar $REPO" >&2; exit 1; }

if git -C "$ROOTDIR" rev-parse -q --verify "refs/tags/$VERSION" >/dev/null; then
	tag_commit="$(git -C "$ROOTDIR" rev-list -n 1 "$VERSION")"
	[[ "$tag_commit" == "$COMMIT" ]] || { echo "FOUT: $VERSION bestaat al op $tag_commit, niet op $COMMIT" >&2; exit 1; }
else
	git -C "$ROOTDIR" tag -a "$VERSION" -m "Spin $VERSION" "$COMMIT"
fi
git -C "$ROOTDIR" push origin "refs/tags/$VERSION"

if gh release view "$VERSION" --repo "$REPO" >/dev/null 2>&1; then
	gh release upload "$VERSION" --repo "$REPO" --clobber "${ASSETS[@]}"
	gh release edit "$VERSION" --repo "$REPO" --title "Spin $VERSION" --notes-file "$OUTDIR/RELEASE-NOTES.md"
else
	gh release create "$VERSION" --repo "$REPO" --verify-tag --title "Spin $VERSION" \
		--notes-file "$OUTDIR/RELEASE-NOTES.md" "${ASSETS[@]}"
fi

# rolling is intentionally mutable: the HOP jobs pin this URL while immutable
# version releases and checksums remain available for audits and rollback.
git -C "$ROOTDIR" tag -f -a rolling -m "Spin rolling ($VERSION)" "$COMMIT"
git -C "$ROOTDIR" push --force origin refs/tags/rolling
if gh release view rolling --repo "$REPO" >/dev/null 2>&1; then
	gh release upload rolling --repo "$REPO" --clobber "${ASSETS[@]}"
	gh release edit rolling --repo "$REPO" --title "Spin rolling ($VERSION)" \
		--notes-file "$OUTDIR/RELEASE-NOTES.md" --prerelease --latest=false
else
	gh release create rolling --repo "$REPO" --verify-tag --title "Spin rolling ($VERSION)" \
		--notes-file "$OUTDIR/RELEASE-NOTES.md" --prerelease --latest=false "${ASSETS[@]}"
fi

echo "KLAAR"
echo "  version: https://github.com/$REPO/releases/tag/$VERSION"
echo "  rolling: https://github.com/$REPO/releases/tag/rolling"
