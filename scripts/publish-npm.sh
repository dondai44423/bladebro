#!/usr/bin/env bash
# ── publish-npm.sh: Build binary, publish platform + main packages ─────
# Run from project root:  ./scripts/publish-npm.sh
#   --no-build   skip building; copy the binaries release.sh just built
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SELF="$SCRIPT_DIR/$(basename "$0")"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

# ── resource cap: same policy as release.sh (skip when already capped) ─
if [[ -z "${BLADE_RELEASE_CAPPED:-}" ]]; then
    export BLADE_RELEASE_CAPPED=1
    cpus=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
    half=$(( cpus / 2 )); if (( half < 2 )); then half=2; fi
    cap=()
    if command -v nice   >/dev/null 2>&1; then cap+=(nice -n 10); fi
    if command -v ionice >/dev/null 2>&1; then cap+=(ionice -c2 -n6); fi
    if command -v taskset >/dev/null 2>&1; then cap+=(taskset -c "0-$((half - 1))"); fi
    if (( ${#cap[@]} > 0 )); then
        exec "${cap[@]}" "$SELF" "$@"
    fi
fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"
if command -v sccache >/dev/null 2>&1; then
    export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
fi

# ── version sync: Cargo.toml is the single source of truth ───────────
VERSION=$(grep '^version' Cargo.toml | head -1 | awk -F'"' '{print $2}')
if [ -z "$VERSION" ]; then
  echo "ERROR: could not read version from Cargo.toml"
  exit 1
fi
echo "Publishing bladebro v$VERSION to npm..."

# ── sync version into npm package.json files ────────────────────────
for pkg in npm/bladebro npm/bladebro-linux-x64 npm/bladebro-linux-arm64 npm/bladebro-windows-x64 npm/bladebro-darwin-x64 npm/bladebro-darwin-arm64; do
  if [ -f "$pkg/package.json" ]; then
    python3 -c "
import json
p = '$pkg/package.json'
d = json.load(open(p))
d['version'] = '$VERSION'
if 'optionalDependencies' in d:
    for k in d['optionalDependencies']:
        d['optionalDependencies'][k] = '$VERSION'
json.dump(d, open(p, 'w'), indent=2)
open(p, 'a').write('\n')
"
    echo "  synced $pkg/package.json → v$VERSION"
  fi
done

# ── binaries: build (unless --no-build), then copy into the packages ──
# release.sh calls this script with --no-build right after it built all
# five binaries itself, so the duplicate second build pass is gone.
SKIP_BUILD=0
if [[ "${1:-}" == "--no-build" ]]; then SKIP_BUILD=1; fi

if [[ "$SKIP_BUILD" == "0" ]]; then
  echo "Building binaries (native + 4 cross targets)..."
  cargo build --release
  if command -v cargo-zigbuild >/dev/null 2>&1; then
    cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28
    cp target/x86_64-unknown-linux-gnu/release/bladebro target/release/bladebro
    cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.28
    cargo zigbuild --release --target x86_64-pc-windows-gnu
    RUSTC_WRAPPER= cargo zigbuild --release --target x86_64-apple-darwin
    RUSTC_WRAPPER= cargo zigbuild --release --target aarch64-apple-darwin
  else
    echo "ERROR: cargo-zigbuild is required; refusing stale cross binaries" >&2
    exit 1
  fi
else
  echo "Using the binaries from the current release build (--no-build)."
fi

python3 tools/reliability_probe/linux-abi.py target/release/bladebro target/aarch64-unknown-linux-gnu/release/bladebro

# Copy every binary into its package. All five platforms are mandatory —
# a missing binary is a hard error, never a partial publish.
copy_bin() {
  if [[ ! -f "$1" ]]; then
    echo "ERROR: missing $1 — build all platforms first (cargo zigbuild must be installed)" >&2
    exit 1
  fi
  cp "$1" "$2"
  chmod +x "$2" 2>/dev/null || true
}
copy_bin target/release/bladebro npm/bladebro-linux-x64/bladebro

copy_bin target/aarch64-unknown-linux-gnu/release/bladebro npm/bladebro-linux-arm64/bladebro
copy_bin target/x86_64-pc-windows-gnu/release/bladebro.exe npm/bladebro-windows-x64/bladebro.exe
copy_bin target/x86_64-apple-darwin/release/bladebro npm/bladebro-darwin-x64/bladebro
copy_bin target/aarch64-apple-darwin/release/bladebro npm/bladebro-darwin-arm64/bladebro

# ── publish platform packages ───────────────────────────────────────
echo "Publishing platform packages..."
for pkg in bladebro-linux-x64 bladebro-linux-arm64 bladebro-windows-x64 bladebro-darwin-x64 bladebro-darwin-arm64; do
  if [ -f "npm/$pkg/package.json" ]; then
    echo "  Publishing $pkg@$VERSION..."
    (cd "npm/$pkg" && npm publish --access public 2>&1 | grep -E "Publishing|error|Tarball")
  fi
done

# ── wait for npm registry propagation ───────────────────────────────
# npm "processing" after a publish can take minutes (the 4.2.0 Windows
# package needed ~3 min; an accepted 4.2.3 publish exceeded the old 5-minute
# window). Allow up to 10 minutes before failing closed.
echo "Waiting for npm registry propagation..."
for i in $(seq 1 60); do
  ALL_OK=true
  for pkg in bladebro-linux-x64 bladebro-linux-arm64 bladebro-windows-x64 bladebro-darwin-x64 bladebro-darwin-arm64; do
    if ! npm view "$pkg@$VERSION" version --prefer-online 2>/dev/null | grep -Fxq "$VERSION"; then
      ALL_OK=false
    fi
  done
  if [ "$ALL_OK" = true ]; then
    echo "  all platform packages propagated after $((i*10))s"
    break
  fi
  sleep 10
  echo "  waiting... ($((i*10))s)"
done

if [[ "$ALL_OK" != true ]]; then
  echo "ERROR: platform packages not all available after 10 minutes; main package was not published" >&2
  echo "       If \`npm stage list\` shows an entry for this version, the publish is STAGED and" >&2
  echo "       awaits approval (npm stage approve <id>; interactive 2FA required)." >&2
  echo "       If it shows nothing, a publish may still be PROCESSING on the registry —" >&2
  echo "       re-check `npm view` for a few more minutes before retrying, and never" >&2
  echo "       republish a version the registry already accepted." >&2
  exit 1
fi

# ── publish main package ────────────────────────────────────────────
echo "Publishing main package: bladebro@$VERSION..."
(cd npm/bladebro && npm publish --access public)

# ── verify the main package is actually live ────────────────────────
# Registry publishes can land asynchronously ("being processed"), and the
# platform-package check above says nothing about the meta package — a
# silent miss would leave `npm i -g bladebro` on the PREVIOUS version while
# this script prints "Done!". `--prefer-online` matters: a plain `npm view`
# serves a stale cache entry for minutes and reads as "missing".
echo "Waiting for bladebro@$VERSION to go live..."
META_OK=false
for i in $(seq 1 60); do
  if npm view "bladebro@$VERSION" version --prefer-online 2>/dev/null | grep -Fxq "$VERSION"; then
    echo "  bladebro@$VERSION is live after $((i*10))s"
    META_OK=true
    break
  fi
  sleep 10
  echo "  waiting... ($((i*10))s)"
done
if [ "$META_OK" != true ]; then
  echo "ERROR: bladebro@$VERSION never appeared on the registry after 10 minutes." >&2
  echo "       If \`npm stage list\` shows an entry for this version, the publish is STAGED" >&2
  echo "       and awaits approval: npm stage approve <id> (interactive 2FA required)." >&2
  echo "       If it shows nothing, the publish may still be PROCESSING (re-check" >&2
  echo "       `npm view` before doing anything) or it failed outright — never republish" >&2
  echo "       a version the registry already accepted." >&2
  exit 1
fi

echo ""
echo "Done! Published bladebro@$VERSION to npm (all platforms)."
echo "  Install:  npm install -g bladebro"
echo "  Run:     npx bladebro mcp"
