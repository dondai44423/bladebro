#!/usr/bin/env bash
# ── release-prep.sh: the whole local half of a release, in under a second ──
# Usage: scripts/release-prep.sh X.Y.Z [--push]
#
# It edits versions, promotes the CHANGELOG, commits and tags. It builds
# NOTHING and runs NO test — the tag is the input to .github/workflows/
# release.yml, which runs the gate and builds every artifact on native
# runners in parallel. Long local verification is what this replaces.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:-}"
PUSH="${2:-}"
if [[ ! "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "usage: scripts/release-prep.sh <major.minor.patch> [--push]" >&2
    exit 1
fi
[[ $(git branch --show-current) == main ]] || { echo "ERROR: release from main" >&2; exit 1; }
[[ -z $(git status --porcelain) ]] || { echo "ERROR: commit tracked changes first" >&2; exit 1; }
! git rev-parse --verify "refs/tags/v$VERSION" >/dev/null 2>&1 || { echo "ERROR: tag v$VERSION exists" >&2; exit 1; }

python3 - "$VERSION" <<'PY'
import json, sys, re
from datetime import date
from pathlib import Path

version = sys.argv[1]

# 1. CHANGELOG: [Unreleased] must hold the notes; promote it in place and
#    leave a fresh empty [Unreleased] for the next cycle.
text = Path("CHANGELOG.md").read_text()
assert text.count("## [Unreleased]") == 1, "exactly one [Unreleased] heading required"
assert text.count("[Unreleased]: ") == 1, "exactly one [Unreleased] link required"
section = text.split("## [Unreleased]", 1)[1].split("\n## [", 1)[0]
assert any(l.startswith("- ") for l in section.splitlines()), "Unreleased must contain release notes"
today = date.today().isoformat()
text = text.replace("## [Unreleased]", f"## [{version}] - {today}", 1)
text = text.replace(f"## [{version}] - {today}", f"## [Unreleased]\n\n## [{version}] - {today}", 1)
ref = next(l for l in text.splitlines() if l.startswith("[Unreleased]: "))
repo = ref[len("[Unreleased]: "):].split("/compare/", 1)[0]
text = text.replace(ref, f"[Unreleased]: {repo}/compare/v{version}...HEAD\n[{version}]: {repo}/releases/tag/v{version}", 1)
Path("CHANGELOG.md").write_text(text)

# 2. Cargo.toml (the crate version) and Cargo.lock (its own package block).
cargo = Path("Cargo.toml").read_text()
cargo = re.sub(r'^version = ".*"', f'version = "{version}"', cargo, count=1, flags=re.MULTILINE)
Path("Cargo.toml").write_text(cargo)
lock = Path("Cargo.lock").read_text()
lock = re.sub(r'(\[\[package\]\]\nname = "bladebro"\nversion = )"[^"]+"', rf'\g<1>"{version}"', lock, count=1)
Path("Cargo.lock").write_text(lock)

# 3. Every npm manifest: its own version and its optionalDependencies.
for path in Path("npm").glob("*/package.json"):
    data = json.loads(path.read_text())
    data["version"] = version
    for dep in data.get("optionalDependencies", {}):
        data["optionalDependencies"][dep] = version
    path.write_text(json.dumps(data, indent=2) + "\n")

print(f"version {version} written to Cargo.toml, Cargo.lock, 6 npm manifests, CHANGELOG")
PY

git add Cargo.toml Cargo.lock CHANGELOG.md npm/*/package.json
git commit -q -m "release: v$VERSION"
git tag -a "v$VERSION" -m "v$VERSION"
echo "committed and tagged v$VERSION"

if [[ "$PUSH" == "--push" ]]; then
    git push origin main
    git push origin "v$VERSION"
    echo "pushed — .github/workflows/release.yml is building all five artifacts in parallel"
else
    echo "push with:  git push origin main --tags"
    echo "then watch: gh run watch --workflow Release"
fi
