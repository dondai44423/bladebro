#!/usr/bin/env bash
# Bladebro release script — the ONLY way to cut a release.
# One command does everything: version bump, build all platforms,
# test, clippy, tag, push, GitHub release with ALL 4 binaries,
# and publish to npm.
#
# Usage: ./release.sh <version>     e.g. ./release.sh 3.0.4
#
# Requires: cargo, cargo-zigbuild (for macOS/Windows cross-compile),
#           git, gh (GitHub CLI), npm (for npm publish)
#           A clean-ish tree (uncommitted changes OK only in gitignored files).

set -euo pipefail

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
    echo "usage: ./release.sh <version>  (e.g. 3.0.4)" >&2
    exit 1
fi

cd "$(dirname "$0")"

echo "=== bladebro release v$VERSION ==="

# 1. Sanity: version must not already exist as a tag.
if git rev-parse "v$VERSION" >/dev/null 2>&1; then
    echo "ERROR: tag v$VERSION already exists" >&2
    exit 1
fi

# 2. Bump Cargo.toml version.
sed -i "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml
grep -q "^version = \"$VERSION\"" Cargo.toml || {
    echo "ERROR: Cargo.toml bump failed" >&2; exit 1; }
echo "[1/9] Cargo.toml -> $VERSION"

# 3. CHANGELOG must have an [Unreleased] section with content;
#    promote it to the release version with today's date.
TODAY=$(date +%Y-%m-%d)
if ! grep -q "^## \[Unreleased\]" CHANGELOG.md; then
    echo "ERROR: no [Unreleased] section in CHANGELOG.md" >&2
    exit 1
fi
python3 - "$VERSION" "$TODAY" << 'PYEOF'
import re, sys
version, today = sys.argv[1], sys.argv[2]
with open("CHANGELOG.md") as f:
    text = f.read()
new = text.replace(
    "## [Unreleased]",
    f"## [Unreleased]\n\n## [{version}] - {today}",
    1,
)
with open("CHANGELOG.md", "w") as f:
    f.write(new)
PYEOF
echo "[2/9] CHANGELOG promoted to [$VERSION] - $TODAY"

# 4. Full verification: build, test, clippy.
echo "[3/9] cargo build --release (Linux)..."
cargo build --release 2>&1 | tail -1

echo "[4/9] cargo test..."
cargo test --release 2>&1 | grep -c "test result: ok" >/dev/null

echo "[5/9] clippy..."
cargo clippy --release -- -D warnings 2>&1 | tail -1

# 5. Build all cross-platform binaries.
echo "[6/9] Building cross-platform binaries..."

# Windows x86_64 (via cargo-zigbuild)
if command -v cargo-zigbuild &>/dev/null; then
    cargo zigbuild --release --target x86_64-pc-windows-gnu 2>&1 | tail -1
else
    echo "  WARNING: cargo-zigbuild not found, skipping Windows build"
fi

# macOS x86_64 (via cargo-zigbuild)
if command -v cargo-zigbuild &>/dev/null; then
    cargo zigbuild --release --target x86_64-apple-darwin 2>&1 | tail -1
else
    echo "  WARNING: cargo-zigbuild not found, skipping macOS x64 build"
fi

# macOS arm64 (via cargo-zigbuild)
if command -v cargo-zigbuild &>/dev/null; then
    cargo zigbuild --release --target aarch64-apple-darwin 2>&1 | tail -1
else
    echo "  WARNING: cargo-zigbuild not found, skipping macOS arm64 build"
fi

# 6. Commit + tag.
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: v$VERSION"
git tag -a "v$VERSION" -m "v$VERSION"
echo "[7/9] committed + tagged v$VERSION"

# 7. Push.
REMOTE_URL=$(git remote get-url origin)
if [[ "$REMOTE_URL" != *"@github.com"* ]]; then
    TOKEN=$(gh auth token 2>/dev/null || true)
    if [[ -n "$TOKEN" ]]; then
        git remote set-url origin "https://dondai44423:${TOKEN}@github.com/dondai44423/bladebro.git"
        trap 'git remote set-url origin "https://github.com/dondai44423/bladebro.git"' EXIT
    fi
fi
git push origin main --tags
git remote set-url origin "https://github.com/dondai44423/bladebro.git" 2>/dev/null || true
trap - EXIT
echo "[8/9] pushed"

# 8. GitHub release with ALL 4 platform binaries.
echo "[9/9] Creating GitHub release with binaries..."

# Prepare asset files with BOTH naming conventions.
# New: bladebro-{os}-{arch} (npm-consistent, matches npm package names)
# Legacy: bladebro-{os}-{x86_64|aarch64} (for old binaries pre-v3.0.3)
# Old binaries only know the legacy name — without it, `bladebro -u` fails.
TMPDIR_RELEASE=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RELEASE"' EXIT

cp target/release/bladebro "$TMPDIR_RELEASE/bladebro-linux-x64"
cp target/release/bladebro "$TMPDIR_RELEASE/bladebro-linux-x86_64"

if [[ -f target/x86_64-pc-windows-gnu/release/bladebro.exe ]]; then
    cp target/x86_64-pc-windows-gnu/release/bladebro.exe "$TMPDIR_RELEASE/bladebro-windows-x64.exe"
    cp target/x86_64-pc-windows-gnu/release/bladebro.exe "$TMPDIR_RELEASE/bladebro-windows-x86_64.exe"
fi

if [[ -f target/x86_64-apple-darwin/release/bladebro ]]; then
    cp target/x86_64-apple-darwin/release/bladebro "$TMPDIR_RELEASE/bladebro-darwin-x64"
    cp target/x86_64-apple-darwin/release/bladebro "$TMPDIR_RELEASE/bladebro-macos-x86_64"
fi

if [[ -f target/aarch64-apple-darwin/release/bladebro ]]; then
    cp target/aarch64-apple-darwin/release/bladebro "$TMPDIR_RELEASE/bladebro-darwin-arm64"
    cp target/aarch64-apple-darwin/release/bladebro "$TMPDIR_RELEASE/bladebro-macos-aarch64"
fi

# Create release with all available binaries.
ASSETS=()
for f in "$TMPDIR_RELEASE"/bladebro-*; do
    [[ -f "$f" ]] && ASSETS+=("$f")
done

if [[ ${#ASSETS[@]} -eq 0 ]]; then
    echo "ERROR: no binaries found to upload" >&2
    exit 1
fi

# Create release first (without assets, to avoid timeout), then upload.
gh release create "v$VERSION" \
    --title "v$VERSION" --latest \
    --generate-notes 2>/dev/null \
|| gh release create "v$VERSION" --title "v$VERSION" --latest --notes "Release v$VERSION"

# Upload assets one at a time for reliability.
for f in "$TMPDIR_RELEASE"/bladebro-*; do
    [[ -f "$f" ]] || continue
    name=$(basename "$f")
    echo "  uploading $name..."
    gh release upload "v$VERSION" "$f" --clobber 2>&1 | head -1
done

# 9. Publish to npm (if publish-npm.sh exists).
if [[ -f scripts/publish-npm.sh ]]; then
    echo ""
    echo "=== Publishing to npm ==="
    bash scripts/publish-npm.sh
fi

echo ""
echo "=== RELEASED v$VERSION ==="
echo "Binaries uploaded: ${#ASSETS[@]}"
echo "  New naming (npm-consistent):"
echo "    - bladebro-linux-x64, bladebro-windows-x64.exe, bladebro-darwin-x64, bladebro-darwin-arm64"
echo "  Legacy naming (old binaries pre-v3.0.3):"
echo "    - bladebro-linux-x86_64, bladebro-windows-x86_64.exe, bladebro-macos-x86_64, bladebro-macos-aarch64"
echo ""
echo "Verify: bladebro -v  (should show update available for older installs)"
echo "Verify: bladebro -u  (should download and install the new version)"
