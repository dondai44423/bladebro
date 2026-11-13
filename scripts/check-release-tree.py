#!/usr/bin/env python3
"""Release tree consistency check — run by the release workflow's plan job.

One version lives in three places (Cargo.toml, the six npm manifests, the
CHANGELOG heading); a mismatch is a broken release, so a tag that does not
match the tree fails here before any runner builds anything.
"""

import json
import re
import sys
from pathlib import Path

PACKAGES = [
    "bladebro",
    "bladebro-linux-x64",
    "bladebro-linux-arm64",
    "bladebro-windows-x64",
    "bladebro-darwin-x64",
    "bladebro-darwin-arm64",
]


def fail(message: str) -> None:
    print(f"ERROR: {message}", file=sys.stderr)
    raise SystemExit(1)


def main() -> None:
    if len(sys.argv) != 2 or not re.fullmatch(r"\d+\.\d+\.\d+", sys.argv[1]):
        fail("usage: check-release-tree.py <major.minor.patch>")
    version = sys.argv[1]

    cargo = Path("Cargo.toml").read_text()
    match = re.search(r'^version = "([^"]+)"', cargo, re.MULTILINE)
    if not match or match.group(1) != version:
        fail(f"Cargo.toml version is {match.group(1) if match else 'missing'}, tag says {version}")

    for name in PACKAGES:
        path = Path("npm") / name / "package.json"
        if not path.is_file():
            fail(f"{path} is missing")
        data = json.loads(path.read_text())
        if data.get("name") != name:
            fail(f"{path}: name is {data.get('name')!r}")
        if data.get("version") != version:
            fail(f"{path}: version is {data.get('version')!r}, tag says {version}")
        for dep, want in data.get("optionalDependencies", {}).items():
            if want != version:
                fail(f"{path}: optional dependency {dep} is {want!r}, want {version}")
        if not (path.parent / "LICENSE").is_file():
            fail(f"{path.parent}/LICENSE is missing")

    changelog = Path("CHANGELOG.md").read_text()
    heading = re.compile(rf"^## \[{re.escape(version)}\] - \d{{4}}-\d{{2}}-\d{{2}}$", re.MULTILINE)
    if not heading.search(changelog):
        fail(f"CHANGELOG.md has no '## [{version}] - YYYY-MM-DD' section")
    if changelog.count("[Unreleased]: ") != 1:
        fail("CHANGELOG.md must carry exactly one [Unreleased]: link")

    print(f"release tree OK: v{version}, 6 npm manifests, changelog section present")


if __name__ == "__main__":
    main()
