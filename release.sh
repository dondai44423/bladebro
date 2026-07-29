#!/usr/bin/env bash
# Bladebro release script — the ONLY way to cut a release.
# One command does everything, in the right order, with
# verification at every step. Version skew becomes impossible:
# the script refuses to release if anything is out of sync.
#
# Usage: ./release.sh <version>     e.g. ./release.sh 2.0.0
#
# Requires: cargo, git, gh (GitHub CLI), a clean-ish tree
# (uncommitted changes OK only in gitignored files).

set -euo pipefail

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
    echo "usage: ./release.sh <version>  (e.g. 2.0.0)" >&2
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
echo "[1/7] Cargo.toml -> $VERSION"

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
# Promote Unreleased -> [version] - date, add fresh Unreleased.
new = text.replace(
    "## [Unreleased]",
    f"## [Unreleased]\n\n## [{version}] - {today}",
    1,
)
with open("CHANGELOG.md", "w") as f:
    f.write(new)
PYEOF
echo "[2/7] CHANGELOG promoted to [$VERSION] - $TODAY"

# 4. Full verification: build, test, clippy.
echo "[3/7] cargo build --release..."
cargo build --release 2>&1 | tail -1
echo "[4/7] cargo test..."
cargo test --release 2>&1 | grep -c "test result: ok" >/dev/null
echo "[5/7] clippy..."
cargo clippy --release -- -D warnings 2>&1 | tail -1

# 5. Commit + tag.
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: v$VERSION"
git tag -a "v$VERSION" -m "v$VERSION"
echo "[6/7] committed + tagged v$VERSION"

# 6. Push. Use gh's stored token for the remote if the
#    remote URL has no credentials.
REMOTE_URL=$(git remote get-url origin)
if [[ "$REMOTE_URL" != *"@github.com"* ]]; then
    TOKEN=$(gh auth token 2>/dev/null || true)
    if [[ -n "$TOKEN" ]]; then
        git remote set-url origin "https://dondai44423:${TOKEN}@github.com/dondai44423/bladebro.git"
        trap 'git remote set-url origin "https://github.com/dondai44423/bladebro.git"' EXIT
    fi
fi
git push origin master:main --tags
# Scrub the token from the remote URL.
git remote set-url origin "https://github.com/dondai44423/bladebro.git" 2>/dev/null || true
trap - EXIT
echo "[7/7] pushed"

# 7. GitHub release with the binary.
ASSET="bladebro-linux-x86_64"
cp target/release/bladebro "/tmp/$ASSET"
gh release create "v$VERSION" "/tmp/$ASSET" \
    --title "v$VERSION" \
    --notes-from-tag 2>/dev/null \
|| gh release create "v$VERSION" "/tmp/$ASSET" --title "v$VERSION" --generate-notes

echo ""
echo "=== RELEASED v$VERSION ==="
echo "Binary attached: $ASSET"
echo "Verify: bladebro -v  (should show update available for older installs)"
