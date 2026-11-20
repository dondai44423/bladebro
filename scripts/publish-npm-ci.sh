#!/usr/bin/env bash
# ── publish-npm-ci.sh: publish the six packages from CI artifacts ──────
# Usage: scripts/publish-npm-ci.sh <dist-dir>
# <dist-dir> holds the release binaries named exactly as the GitHub assets:
#   bladebro-linux-x64, bladebro-linux-arm64, bladebro-windows-x64.exe,
#   bladebro-darwin-x64, bladebro-darwin-arm64
# Order is fixed: platform packages first, the launcher last. If the launcher
# is already published at this version, fail closed. If an earlier run failed
# partway through (e.g. a transient Sigstore Rekor 409 during provenance),
# already-published platform packages are skipped while unpublished ones and
# the launcher complete.
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

is_published() {
    npm view "$1@$VERSION" version --prefer-online 2>/dev/null | grep -Fxq "$VERSION"
}

# Fail closed if the public launcher itself is already live at this version.
if is_published "bladebro"; then
    echo "ERROR: bladebro@$VERSION is already published — never republish a release" >&2
    exit 1
fi

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

publish_pkg() {
    local pkg="$1"
    local allow_skip="${2:-0}"

    if is_published "$pkg"; then
        if [[ "$allow_skip" == "1" ]]; then
            echo "  $pkg@$VERSION (already published — skipping)"
            return 0
        fi
        echo "ERROR: $pkg@$VERSION is already published — never republish a version" >&2
        exit 1
    fi

    local attempt log
    for attempt in 1 2 3; do
        echo "  $pkg@$VERSION (attempt $attempt/3)"
        log=$(mktemp)
        if ( cd "npm/$pkg" && npm publish --access public --provenance --no-audit --no-fund ) 2>&1 | tee "$log"; then
            rm -f "$log"
            return 0
        fi
        # Re-check registry first in case npm accepted the publish before a
        # transient response/tlog error surfaced.
        sleep 5
        if is_published "$pkg"; then
            echo "  $pkg@$VERSION confirmed live on registry after transient error"
            rm -f "$log"
            return 0
        fi
        if grep -qE 'TLOG_CREATE_ENTRY_ERROR|E5[0-9]{2}|E429|ETIMEDOUT|ECONNRESET|EAI_AGAIN' "$log"; then
            rm -f "$log"
            if [[ "$attempt" -lt 3 ]]; then
                local wait_s=$((attempt * 10))
                echo "  transient publish/provenance error for $pkg@$VERSION — retrying in ${wait_s}s..." >&2
                sleep "$wait_s"
                continue
            fi
        else
            rm -f "$log"
            exit 1
        fi
    done

    echo "ERROR: failed to publish $pkg@$VERSION after 3 attempts" >&2
    exit 1
}

for pkg in bladebro-linux-x64 bladebro-linux-arm64 bladebro-windows-x64 bladebro-darwin-x64 bladebro-darwin-arm64; do
    publish_pkg "$pkg" 1
done
publish_pkg bladebro 0

# Registry propagation: a bounded wait, not the old ten-minute window. Each
# package must be visible at this version before the release goes Latest.
echo "verifying the registry..."
for _ in $(seq 1 30); do
    ok=0
    for pkg in bladebro bladebro-linux-x64 bladebro-linux-arm64 bladebro-windows-x64 bladebro-darwin-x64 bladebro-darwin-arm64; do
        if is_published "$pkg"; then
            ok=$((ok + 1))
        fi
    done
    [[ "$ok" == 6 ]] && { echo "all six packages live at v$VERSION"; exit 0; }
    sleep 10
done
echo "ERROR: packages did not all appear within 300s — never republish; re-check, then expose Latest" >&2
exit 1
