#!/usr/bin/env bash
# ── publish-npm.sh: Build binary, publish platform + main packages ─────
# Run from project root:  ./scripts/publish-npm.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

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

# ── build all platform binaries ─────────────────────────────────────
echo "Building binaries..."

# Linux x86_64 (native)
cargo build --release
cp target/release/bladebro npm/bladebro-linux-x64/bladebro
chmod +x npm/bladebro-linux-x64/bladebro

# Linux arm64 (via cargo-zigbuild)
if command -v cargo-zigbuild &>/dev/null; then
  cargo zigbuild --release --target aarch64-unknown-linux-gnu
  cp target/aarch64-unknown-linux-gnu/release/bladebro npm/bladebro-linux-arm64/bladebro
  chmod +x npm/bladebro-linux-arm64/bladebro
else
  echo "WARNING: cargo-zigbuild not found. Skipping Linux arm64 build."
fi

# Windows x86_64 (via cargo-zigbuild + zig linker)
if command -v cargo-zigbuild &>/dev/null; then
  cargo zigbuild --release --target x86_64-pc-windows-gnu
  cp target/x86_64-pc-windows-gnu/release/bladebro.exe npm/bladebro-windows-x64/bladebro.exe
else
  echo "WARNING: cargo-zigbuild not found. Skipping Windows build."
fi

# macOS x86_64 + arm64 (via cargo-zigbuild + zig)
if command -v cargo-zigbuild &>/dev/null; then
  cargo zigbuild --release --target x86_64-apple-darwin
  cp target/x86_64-apple-darwin/release/bladebro npm/bladebro-darwin-x64/bladebro
  chmod +x npm/bladebro-darwin-x64/bladebro

  cargo zigbuild --release --target aarch64-apple-darwin
  cp target/aarch64-apple-darwin/release/bladebro npm/bladebro-darwin-arm64/bladebro
  chmod +x npm/bladebro-darwin-arm64/bladebro
else
  echo "WARNING: cargo-zigbuild not found. Skipping macOS builds."
fi

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
