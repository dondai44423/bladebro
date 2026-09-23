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
    cargo zigbuild --release --target aarch64-unknown-linux-gnu
    cargo zigbuild --release --target x86_64-pc-windows-gnu
    cargo zigbuild --release --target x86_64-apple-darwin
    cargo zigbuild --release --target aarch64-apple-darwin
  else
    echo "WARNING: cargo-zigbuild not found — cross binaries will not be rebuilt."
  fi
else
  echo "Using the binaries from the current release build (--no-build)."
fi

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
echo "Waiting for npm registry propagation..."
for i in $(seq 1 12); do
  ALL_OK=true
  for pkg in bladebro-linux-x64 bladebro-linux-arm64 bladebro-windows-x64 bladebro-darwin-x64 bladebro-darwin-arm64; do
    if ! npm view "$pkg@$VERSION" version 2>/dev/null | grep -q "$VERSION"; then
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

# ── publish main package ────────────────────────────────────────────
echo "Publishing main package: bladebro@$VERSION..."
(cd npm/bladebro && npm publish --access public)

echo ""
echo "Done! Published bladebro@$VERSION to npm (all platforms)."
echo "  Install:  npm install -g bladebro"
echo "  Run:     npx bladebro mcp"
