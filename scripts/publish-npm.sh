#!/usr/bin/env bash
# ── publish-npm.sh: Build binary, publish platform + main packages ─────
# Run from project root:  ./scripts/publish-npm.sh
# Or via CI:              ./scripts/publish-npm.sh
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

# ── sync version into npm package.json files ──────────────────────────
for pkg in npm/bladebro npm/bladebro-linux-x64; do
  if [ -f "$pkg/package.json" ]; then
    # Use python to update version in-place (jq may not be installed)
    python3 -c "
import json, sys
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

# ── build the release binary ──────────────────────────────────────────
echo "Building release binary..."
cargo build --release

# ── copy binary into platform package ─────────────────────────────────
echo "Copying binary to platform package..."
cp target/release/bladebro npm/bladebro-linux-x64/bladebro
chmod +x npm/bladebro-linux-x64/bladebro

# ── dry-run validation (catch packaging errors before publish) ────────
echo "Validating packages..."
(cd npm/bladebro-linux-x64 && npm pack --dry-run 2>&1 | grep -E "bladebro|postinstall|package.json" | head -5)
(cd npm/bladebro && npm pack --dry-run 2>&1 | grep -E "bin.js|README|LICENSE" | head -5)

# ── publish platform package FIRST ───────────────────────────────────
echo "Publishing platform package: bladebro-linux-x64@$VERSION..."
(cd npm/bladebro-linux-x64 && npm publish --access public)

# ── wait for npm registry propagation ────────────────────────────────
echo "Waiting for npm registry propagation..."
for i in $(seq 1 12); do
  if npm view "bladebro-linux-x64@$VERSION" version 2>/dev/null | grep -q "$VERSION"; then
    echo "  platform package propagated after ${i}0s"
    break
  fi
  sleep 10
  echo "  waiting... (${i}0s)"
done

# ── publish main package ─────────────────────────────────────────────
echo "Publishing main package: bladebro@$VERSION..."
(cd npm/bladebro && npm publish --access public)

echo ""
echo "Done! Published bladebro@$VERSION to npm."
echo "  Install:  npm install -g bladebro"
echo "  Run:     npx bladebro mcp"
echo "  Verify:  npm view bladebro@$VERSION"
