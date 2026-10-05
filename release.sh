#!/usr/bin/env bash
# Bladebro release script — the ONLY way to cut a release.
# One command does everything: version bump, build all platforms,
# test, clippy, tag, push, GitHub release with all 5 platforms,
# and publish to npm.
# Resource-capped: re-execs under half the CPUs at low priority + sccache,
# so the machine stays usable while it runs (see status.md workflow).
#
# Usage: ./release.sh <version>     e.g. ./release.sh 3.0.4
#
# Requires: cargo, cargo-zigbuild (for macOS/Windows cross-compile),
#           git, gh (GitHub CLI), npm (for npm publish)
#           A clean-ish tree (uncommitted changes OK only in gitignored files).

set -euo pipefail

# Absolute self-path for the resource-cap re-exec (works from any cwd).
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"

VERSION="${1:-}"
if [[ $# != 1 || ! "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "usage: ./release.sh <major.minor.patch>" >&2
    exit 1
fi

cd "$(dirname "$0")"

# ── Resource cap ──────────────────────────────────────────────────────
# Re-exec under half the CPUs at low priority so the machine stays usable
# (browser/editor keep their share). Children inherit the cap; the guard
# prevents a re-exec loop.
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

# ── Parallelism + compiler cache ──────────────────────────────────────
# The local (gitignored) .cargo/config.toml sets the same; these exports
# keep the script self-sufficient on a fresh checkout.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"
if command -v sccache >/dev/null 2>&1; then
    export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
fi

echo "=== bladebro release v$VERSION ==="

[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || { echo "ERROR: release builds require a Linux x86_64 host" >&2; exit 1; }

# Fail before editing version files: stale artifacts are never a substitute
# for an unavailable toolchain, and a release starts from a reviewable tree.
for command in cargo cargo-zigbuild zig git gh npm python3 sha256sum; do
    command -v "$command" >/dev/null || { echo "ERROR: missing $command" >&2; exit 1; }
done
[[ $(git branch --show-current) == main ]] || { echo "ERROR: release from main" >&2; exit 1; }
[[ -z $(git status --porcelain) ]] || { echo "ERROR: commit tracked changes before releasing" >&2; exit 1; }
if git rev-parse --verify "refs/tags/v$VERSION" >/dev/null 2>&1; then
    echo "ERROR: tag v$VERSION already exists" >&2; exit 1
fi
gh auth status >/dev/null 2>&1
npm whoami >/dev/null
python3 - <<'CHECK'
from pathlib import Path
text = Path('CHANGELOG.md').read_text()
assert text.count('## [Unreleased]') == 1, 'exactly one Unreleased section required'
section = text.split('## [Unreleased]', 1)[1].split('\n## [', 1)[0]
assert any(line.startswith('- ') for line in section.splitlines()), 'Unreleased must contain release notes'
import json
packages=['bladebro','bladebro-linux-x64','bladebro-linux-arm64','bladebro-windows-x64','bladebro-darwin-x64','bladebro-darwin-arm64']
for name in packages:
    path=Path('npm')/name
    assert json.loads((path/'package.json').read_text())['name']==name
    assert (path/'LICENSE').is_file(), f'{name} license missing'
CHECK

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
import sys
version, today = sys.argv[1], sys.argv[2]
with open("CHANGELOG.md") as f:
    text = f.read()
# RENAME the [Unreleased] heading in place so its content follows the
# new version heading. (The old insert-a-heading-above approach left a
# duplicate [Unreleased] holding all the content, tethering releases to
# a phantom section that eventually produced two [Unreleased] blocks.)
new = text.replace(
    "## [Unreleased]",
    f"## [{version}] - {today}",
    1,
)
assert new != text, "[Unreleased] heading not found"
# Fresh empty [Unreleased] on top for the next dev cycle (convention).
new = new.replace(
    f"## [{version}] - {today}",
    f"## [Unreleased]\n\n## [{version}] - {today}",
    1,
)
with open("CHANGELOG.md", "w") as f:
    f.write(new)
PYEOF
echo "[2/9] CHANGELOG promoted to [$VERSION] - $TODAY"

# Synchronize every manifest before verification and the release commit.
python3 - "$VERSION" <<'SYNC'
import json,sys
from pathlib import Path
for path in Path('npm').glob('*/package.json'):
    data=json.loads(path.read_text())
    data['version']=sys.argv[1]
    for name in data.get('optionalDependencies', {}):
        data['optionalDependencies'][name]=sys.argv[1]
    path.write_text(json.dumps(data,indent=2)+'\n')
SYNC

cargo fmt --check
cargo clippy --release -- -D warnings
# Each test process owns fresh fixtures and data roots; cache files stay intact.
cargo test --release
cargo test --release

# Commit first, build artifacts from this exact clean commit. A failed build
# leaves a local commit, never a public tag or partial release.
git add Cargo.toml Cargo.lock CHANGELOG.md npm/*/package.json
git commit -m "release: v$VERSION"
RELEASE_SHA=$(git rev-parse HEAD)
cargo build --release
cargo zigbuild --release --target x86_64-pc-windows-gnu
RUSTC_WRAPPER= cargo zigbuild --release --target x86_64-apple-darwin
RUSTC_WRAPPER= cargo zigbuild --release --target aarch64-apple-darwin
cargo zigbuild --release --target aarch64-unknown-linux-gnu
for artifact in target/release/bladebro \
    target/x86_64-pc-windows-gnu/release/bladebro.exe \
    target/x86_64-apple-darwin/release/bladebro \
    target/aarch64-apple-darwin/release/bladebro \
    target/aarch64-unknown-linux-gnu/release/bladebro; do
    [[ -s "$artifact" ]] || { echo "ERROR: missing $artifact" >&2; exit 1; }
done

# Native CI is a release gate. Push the commit before making a version tag.
git push origin main
RUN_ID=""
for _ in $(seq 1 12); do
    RUN_ID=$(gh run list --workflow CI --commit "$RELEASE_SHA" --event push --json databaseId --jq '.[0].databaseId // empty')
    [[ -n "$RUN_ID" ]] && break
    sleep 5
done
[[ -n "$RUN_ID" ]] || { echo "ERROR: no native CI run for $RELEASE_SHA" >&2; exit 1; }
gh run watch "$RUN_ID" --exit-status
git tag -a "v$VERSION" -m "v$VERSION"
git push origin "v$VERSION"

# 8. GitHub release with all 5 platform binaries.
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

if [[ -f target/aarch64-unknown-linux-gnu/release/bladebro ]]; then
    cp target/aarch64-unknown-linux-gnu/release/bladebro "$TMPDIR_RELEASE/bladebro-linux-arm64"
    cp target/aarch64-unknown-linux-gnu/release/bladebro "$TMPDIR_RELEASE/bladebro-linux-aarch64"
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
gh release create "v$VERSION" --verify-tag --title "v$VERSION" --draft --generate-notes

# Generate .sha256 checksums for every binary. SECURITY: the self-updater
# (v3.3.0+) fail-closes when a release ships no checksum — releases without
# these files cannot be installed via `bladebro -u`.
for f in "$TMPDIR_RELEASE"/bladebro-*; do
    [[ -f "$f" ]] || continue
    [[ "$f" == *.sha256 ]] && continue
    (cd "$TMPDIR_RELEASE" && sha256sum "$(basename "$f")" > "$(basename "$f").sha256")
done

# Upload assets one at a time for reliability.
for f in "$TMPDIR_RELEASE"/bladebro-*; do
    [[ -f "$f" ]] || continue
    name=$(basename "$f")
    echo "  uploading $name..."
    gh release upload "v$VERSION" "$f" --clobber 2>&1 | head -1
done

# Verify the complete draft before publishing any npm meta package.
ASSET_COUNT=$(gh release view "v$VERSION" --json assets --jq '.assets | length')
[[ "$ASSET_COUNT" == 20 ]] || { echo "ERROR: draft has $ASSET_COUNT assets, expected 20" >&2; exit 1; }
bash scripts/publish-npm.sh --no-build
# npm publication verifies all exact package versions. Only now expose the
# GitHub release to self-update clients as Latest.
gh release edit "v$VERSION" --draft=false --latest

echo ""
echo "=== RELEASED v$VERSION ==="
echo "Binaries uploaded: ${#ASSETS[@]}"
echo "  New naming (npm-consistent):"
echo "    - bladebro-linux-x64, bladebro-linux-arm64, bladebro-windows-x64.exe, bladebro-darwin-x64, bladebro-darwin-arm64"
echo "  Legacy naming (old binaries pre-v3.0.3):"
echo "    - bladebro-linux-x86_64, bladebro-linux-aarch64, bladebro-windows-x86_64.exe, bladebro-macos-x86_64, bladebro-macos-aarch64"
echo ""
echo "Verify: bladebro -v  (should show update available for older installs)"
echo "Verify: bladebro -u  (should download and install the new version)"
