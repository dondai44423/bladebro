#!/usr/bin/env python3
"""Release failure paths, with copied files and fake publishing commands.
No real GitHub or npm mutations. Assert refusal before version edits and
before publishing a meta package whose platform versions are unavailable.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

repo = Path(__file__).resolve().parents[2]
with tempfile.TemporaryDirectory(prefix='blade-release-test-') as tmp:
    root = Path(tmp)
    for name in ('release.sh', 'scripts/publish-npm.sh', 'Cargo.toml', 'CHANGELOG.md', 'tools/reliability_probe/linux-abi.py'):
        destination = root / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(repo / name, destination)
    commands = root / 'commands'
    commands.mkdir()
    def script(name, body):
        path = commands / name
        path.write_text('#!/bin/sh\n' + body + '\n')
        path.chmod(0o700)
    script('uname', 'case "$1" in -s) echo Linux;; -m) echo x86_64;; esac')
    script('cargo', 'exit 0')
    env = dict(os.environ, PATH=str(commands), BLADE_RELEASE_CAPPED='1')
    before = {name: (root / name).read_bytes() for name in ('Cargo.toml','CHANGELOG.md')}
    result = subprocess.run(['/bin/bash', str(root / 'release.sh'),'4.1.0'], env=env, cwd=root, capture_output=True, text=True)
    # dirname/basename are used before preflight; preserve genuine utilities.
    # This deliberately restricted PATH must still reach missing-cross-tool guard.
    if 'missing cargo-zigbuild' not in result.stderr:
        for name in ('dirname','basename'):
            (commands / name).symlink_to(shutil.which(name))
        result = subprocess.run(['/bin/bash', str(root / 'release.sh'),'4.1.0'], env=env, cwd=root, capture_output=True, text=True)
    assert result.returncode != 0 and 'missing cargo-zigbuild' in result.stderr, result.stderr
    assert all((root / name).read_bytes() == data for name,data in before.items())
    print('PASS missing cross builder refuses before any version mutation')
    # Existing artifacts must not allow bypassing the unavailable builder.
    (root / 'target/release').mkdir(parents=True)
    (root / 'target/release/bladebro').write_text('stale')
    result = subprocess.run(['/bin/bash', str(root / 'release.sh'),'4.1.0'], env=env, cwd=root, capture_output=True, text=True)
    assert result.returncode != 0 and 'missing cargo-zigbuild' in result.stderr
    assert all((root / name).read_bytes() == data for name,data in before.items())
    print('PASS stale artifacts cannot bypass the cross builder gate')
    packages = ['bladebro','bladebro-linux-x64','bladebro-linux-arm64','bladebro-windows-x64','bladebro-darwin-x64','bladebro-darwin-arm64']
    for package in packages:
        path = root / 'npm' / package
        path.mkdir(parents=True)
        (path / 'package.json').write_text(json.dumps({'name':package,'version':'4.0.3'}))
    for target, filename in [('release','bladebro'),('aarch64-unknown-linux-gnu/release','bladebro'),('x86_64-pc-windows-gnu/release','bladebro.exe'),('x86_64-apple-darwin/release','bladebro'),('aarch64-apple-darwin/release','bladebro')]:
        path = root / 'target' / target
        path.mkdir(parents=True,exist_ok=True)
        (path / filename).write_text('copied test fixture')
    calls = root / 'npm-calls'
    env.update(PATH=str(commands)+':'+os.environ['PATH'], TEST_NPM_CALLS=str(calls))
    script('npm', 'echo "$PWD $*" >> "$TEST_NPM_CALLS"\ncase "$1" in publish) echo Publishing;; view) echo 4.0.30;; *) exit 1;; esac')
    script('sleep', 'exit 0')
    script('objdump', 'echo "0000 DF .text (GLIBC_2.28) statx"')
    result = subprocess.run(['/bin/bash', str(root / 'scripts/publish-npm.sh'),'--no-build'], env=env,cwd=root,capture_output=True,text=True)
    assert result.returncode != 0 and 'main package was not published' in result.stderr, (result.stdout,result.stderr)
    lines = calls.read_text().splitlines()
    assert sum(' publish ' in line for line in lines) == 5, lines
    assert not any('/npm/bladebro publish ' in line for line in lines), lines
    assert sum(' view ' in line for line in lines) == 60, 'propagation deadline not reached'
    print('PASS exact-version mismatch reaches propagation deadline and prevents meta publish')
    print('RELEASE PREFLIGHT 3/3')
