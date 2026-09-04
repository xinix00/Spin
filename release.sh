#!/usr/bin/env bash
set -euo pipefail

# Build and publish one immutable Spin release plus the stable rolling channel.
#
#   ./release.sh                 # v1.0.0
#   ./release.sh 1.1.0           # normalized to v1.1.0
#   PUBLISH=0 ./release.sh v1.1.0 # compile gate only
#
# Linux server/client artifacts are static native binaries. The Docker client
# still runs one of those native architectures inside its container; Docker is
# the packaging/isolation boundary, not CPU emulation. HopOS server artifacts
# are real GOOS=tamago slot images built against the selected HopOS metal tree.

ROOTDIR="$(cd "$(dirname "$0")" && pwd)"
REPO="${REPO:-xinix00/Spin}"
HOPOS_DIR="${HOPOS_DIR:-$HOME/Git/hop-os}"
TAMAGO="${TAMAGO:-$HOME/tamago-go/bin/go}"
PUBLISH="${PUBLISH:-1}"

input="${1:-v1.0.0}"
case "$input" in
	v*) VERSION="$input" ;;
	*)  VERSION="v$input" ;;
esac
if [[ ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]]; then
	echo "FOUT: versie moet semver zijn (bijvoorbeeld v1.0.0)" >&2
	exit 1
fi

OUTDIR="$ROOTDIR/dist/$VERSION"
COMMIT="$(git -C "$ROOTDIR" rev-parse HEAD)"
SHORT_COMMIT="$(git -C "$ROOTDIR" rev-parse --short=12 HEAD)"
BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
LDFLAGS="-s -w -X easyacp/internal/buildinfo.Version=$VERSION -X easyacp/internal/buildinfo.Commit=$SHORT_COMMIT -X easyacp/internal/buildinfo.BuiltAt=$BUILT_AT"

if [[ -n "$(git -C "$ROOTDIR" status --porcelain)" && -z "${RELEASE_ALLOW_DIRTY:-}" ]]; then
	echo "FOUT: Spin working tree is niet schoon; commit eerst (of RELEASE_ALLOW_DIRTY=1 voor alleen een bewuste lokale gate)." >&2
	exit 1
fi
[[ -d "$HOPOS_DIR/metal" ]] || { echo "FOUT: HopOS metal ontbreekt op $HOPOS_DIR/metal (zet HOPOS_DIR)" >&2; exit 1; }
[[ -x "$TAMAGO" ]] || { echo "FOUT: Tamago-toolchain ontbreekt op $TAMAGO (zet TAMAGO)" >&2; exit 1; }
command -v gh >/dev/null || [[ "$PUBLISH" != "1" ]] || { echo "FOUT: gh ontbreekt" >&2; exit 1; }

echo "== Spin $VERSION =="
echo "   commit: $SHORT_COMMIT"
echo "   HopOS:  $HOPOS_DIR"

rm -rf "$OUTDIR"
mkdir -p "$OUTDIR"

ASSETS=()
for arch in amd64 arm64; do
	echo ">> linux/$arch"
	for component in server client; do
		asset="$OUTDIR/spin-$component-linux-$arch"
		CGO_ENABLED=0 GOOS=linux GOARCH="$arch" go build -trimpath -ldflags "$LDFLAGS" \
			-o "$asset" "./cmd/spin-$component"
		ASSETS+=("$asset")
	done
done

tmpdir="$(mktemp -d)"
cleanup() { rm -rf "$tmpdir"; }
trap cleanup EXIT INT TERM
cp "$ROOTDIR/go.mod" "$tmpdir/hopos.mod"
cp "$ROOTDIR/go.sum" "$tmpdir/hopos.sum"
go mod edit -modfile="$tmpdir/hopos.mod" -replace="github.com/xinix00/HopOS/metal/v2=$HOPOS_DIR/metal"

echo ">> HopOS/arm64"
hopos_arm="$OUTDIR/spin-server-arm64-tamago.elf"
GOWORK=off GOTOOLCHAIN=local GOOS=tamago GOOSPKG=github.com/usbarmory/tamago GOARCH=arm64 \
	"$TAMAGO" build -modfile="$tmpdir/hopos.mod" -tags linkcpuinit -trimpath \
	-ldflags "-w -T 0x50010000 -R 0x1000 -X easyacp/internal/buildinfo.Version=$VERSION -X easyacp/internal/buildinfo.Commit=$SHORT_COMMIT -X easyacp/internal/buildinfo.BuiltAt=$BUILT_AT" \
	-o "$hopos_arm" ./cmd/spin-server
ASSETS+=("$hopos_arm")

echo ">> HopOS/riscv64"
hopos_riscv="$OUTDIR/spin-server-riscv64-tamago.elf"
GOWORK=off GOTOOLCHAIN=local GOOS=tamago GOOSPKG=github.com/usbarmory/tamago GOARCH=riscv64 \
	"$TAMAGO" build -modfile="$tmpdir/hopos.mod" -tags "linkramsize linkcpuinit" -trimpath \
	-ldflags "-w -T 0x88010000 -R 0x1000 -X easyacp/internal/buildinfo.Version=$VERSION -X easyacp/internal/buildinfo.Commit=$SHORT_COMMIT -X easyacp/internal/buildinfo.BuiltAt=$BUILT_AT" \
	-o "$hopos_riscv" ./cmd/spin-server
ASSETS+=("$hopos_riscv")

file "$hopos_arm" | grep -q "ARM aarch64" || { echo "FOUT: HopOS arm64 artifact is geen AArch64 ELF" >&2; exit 1; }
file "$hopos_riscv" | grep -q "RISC-V" || { echo "FOUT: HopOS riscv64 artifact is geen RISC-V ELF" >&2; exit 1; }
for elf in "$hopos_arm" "$hopos_riscv"; do
	n="$($TAMAGO tool nm "$elf" 2>/dev/null | grep -c gvisor || true)"
	[[ "$n" -eq 0 ]] || { echo "FOUT: $(basename "$elf") linkt $n gVisor-symbolen" >&2; exit 1; }
done

(cd "$OUTDIR" && shasum -a 256 spin-* > SHA256SUMS)
ASSETS+=("$OUTDIR/SHA256SUMS")

cat > "$OUTDIR/RELEASE-NOTES.md" <<EOF
Spin $VERSION ($SHORT_COMMIT), built $BUILT_AT.

Artifacts:
- spin-server-linux-amd64 / arm64: cloud control plane and web frontend
- spin-client-linux-amd64 / arm64: native Docker runner (it needs Docker CLI + socket)
- spin-server-arm64-tamago.elf: native HopOS arm64 slot image
- spin-server-riscv64-tamago.elf: native HopOS riscv64 slot image

The client connects outbound to the configured server URL; it derives wss:// from https:// and uses /api/runner/ws. Supply the same SPIN_WORKER_TOKEN on both sides.

HopOS: publish ER_PORT_HTTP, mount /data for /data/spin.db, and provide SPIN_WORKER_TOKEN plus a stable SPIN_MASTER_KEY. The libc-free SQLite build uses Spin's HopOS VFS; Job attachments, central Docker snapshot archives and one-file database backup/restore are enabled on arm64 and riscv64.
EOF
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

# rolling is intentionally mutable: clients can pin this URL while immutable
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
