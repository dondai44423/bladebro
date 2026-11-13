#!/usr/bin/env bash
# ── publish-npm-ci.sh: publish the six packages from CI artifacts ──────
# Usage: scripts/publish-npm-ci.sh <dist-dir>
# <dist-dir> holds the release binaries named exactly as the GitHub assets:
#   bladebro-linux-x64, bladebro-linux-arm64, bladebro-windows-x64.exe,
#   bladebro-darwin-x64, bladebro-darwin-arm64
# Order is fixed: platform packages first, the launcher last. A version the
# registry already has is never republished — it fails closed.
set -euo pipefail

cd "$(dirname "$0")/.."
DIST="${1:-}"
[[ -d "$DIST" ]] || { echo "usage: scripts/publish-npm-ci.sh <dist-dir>" >&2; exit 1; }

VERSION=$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)
[[ -n "$VERSION" ]] || { echo "ERROR: no version in Cargo.toml" >&2; exit 1; }
echo "publishing bladebro v$VERSION from $DIST"

for pkg in bladebro bladebro-linux-x64 bladebro-linux-arm64 bladebro-windows-x64 bladebro-darwin-x64 bladebro-darwin-arm64; do
    want=$(python3 -c "import json;print(json.load(open('npm/$pkg/package.json'))['version'])")
    [[ "$want" == "$VERSION" ]] || { echo "ERROR: npm/$pkg is $want, Cargo.toml is $VERSION" >&2; exit 1; }
done

copy() {
    [[ -f "$1" ]] || { echo "ERROR: missing $1" >&2; exit 1; }
    cp "$1" "$2"
    chmod +x "$2" 2>/dev/null || true
}
copy "$DIST/bladebro-linux-x64"   npm/bladebro-linux-x64/bladebro
copy "$DIST/bladebro-linux-arm64" npm/bladebro-linux-arm64/bladebro
copy "$DIST/bladebro-windows-x64.exe" npm/bladebro-windows-x64/bladebro.exe
copy "$DIST/bladebro-darwin-x64"  npm/bladebro-darwin-x64/bladebro
copy "$DIST/bladebro-darwin-arm64" npm/bladebro-darwin-arm64/bladebro

publish() {
    local pkg="$1"
    if npm view "$pkg@$VERSION" version --prefer-online 2>/dev/null | grep -Fxq "$VERSION"; then
        echo "ERROR: $pkg@$VERSION is already published — never republish a version" >&2
        exit 1
    fi
    echo "  $pkg@$VERSION"
    ( cd "npm/$pkg" && npm publish --access public --provenance --no-audit --no-fund )
}

for pkg in bladebro-linux-x64 bladebro-linux-arm64 bladebro-windows-x64 bladebro-darwin-x64 bladebro-darwin-arm64; do
    publish "$pkg"
done
publish bladebro

# Registry propagation: a bounded wait, not the old ten-minute window. Each
# package must be visible at this version before the release goes Latest.
echo "verifying the registry..."
for _ in $(seq 1 30); do
    ok=0
    for pkg in bladebro bladebro-linux-x64 bladebro-linux-arm64 bladebro-windows-x64 bladebro-darwin-x64 bladebro-darwin-arm64; do
        if npm view "$pkg@$VERSION" version --prefer-online 2>/dev/null | grep -Fxq "$VERSION"; then
            ok=$((ok + 1))
        fi
    done
    [[ "$ok" == 6 ]] && { echo "all six packages live at v$VERSION"; exit 0; }
    sleep 10
done
echo "ERROR: packages did not all appear within 300s — never republish; re-check, then expose Latest" >&2
exit 1
