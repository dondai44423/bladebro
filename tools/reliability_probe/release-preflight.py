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
    for name in ('release.sh', 'scripts/publish-npm.sh', 'Cargo.toml', 'CHANGELOG.md', 'deny.toml', 'tools/reliability_probe/linux-abi.py'):
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
    script('cargo', 'if [ "$1" = deny ]; then shift; exec cargo-deny "$@"; fi\nexit 0')
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
        (path / 'LICENSE').write_text('copied license fixture')
    for target, filename in [('release','bladebro'),('aarch64-unknown-linux-gnu/release','bladebro'),('x86_64-pc-windows-gnu/release','bladebro.exe'),('x86_64-apple-darwin/release','bladebro'),('aarch64-apple-darwin/release','bladebro')]:
        path = root / 'target' / target
        path.mkdir(parents=True,exist_ok=True)
        (path / filename).write_text('copied test fixture')
    calls = root / 'npm-calls'
    env.update(PATH=str(commands)+':'+os.environ['PATH'], TEST_NPM_CALLS=str(calls))
    script('npm', 'echo "$PWD $*" >> "$TEST_NPM_CALLS"\ncase "$1" in publish) echo Publishing;; view) echo 4.0.30;; *) exit 1;; esac')
    script('sleep', 'exit 0')
    script('objdump', 'echo "0000 DF .text (GLIBC_2.28) statx"')
    for name in ('cargo-zigbuild', 'zig'):
        script(name, 'exit 0')
    script('git', 'case "$1" in branch) echo main;; rev-parse) exit 1;; *) exit 0;; esac')
    script('gh', 'case "$1" in repo) echo fixture/bladebro;; api) echo "$TEST_OPEN_ALERT";; *) exit 0;; esac')
    script('cargo-deny', 'if [ "$TEST_DENY_FAIL" = 1 ]; then echo "advisories FAILED" >&2; exit 1; fi')
    # Authentication succeeds; only the actual security gates may refuse.
    script('npm', 'case "$1" in whoami) echo fixture-publisher;; *) exit 1;; esac')
    env.update(TEST_OPEN_ALERT='1', TEST_DENY_FAIL='0')
    result = subprocess.run(['/bin/bash', str(root / 'release.sh'),'4.1.1'], env=env,cwd=root,capture_output=True,text=True)
    assert result.returncode != 0 and 'resolve open security advisories' in result.stderr, (result.stdout,result.stderr)
    assert all((root / name).read_bytes() == data for name,data in before.items())
    print('PASS open GitHub advisory refuses before version mutation')
    env.update(TEST_OPEN_ALERT='', TEST_DENY_FAIL='1')
    result = subprocess.run(['/bin/bash', str(root / 'release.sh'),'4.1.1'], env=env,cwd=root,capture_output=True,text=True)
    assert result.returncode != 0 and 'advisories FAILED' in result.stderr, (result.stdout,result.stderr)
    assert all((root / name).read_bytes() == data for name,data in before.items())
    print('PASS locked dependency advisory failure refuses before version mutation')
    script('npm', 'echo "$PWD $*" >> "$TEST_NPM_CALLS"\ncase "$1" in publish) echo Publishing;; view) echo 4.0.30;; *) exit 1;; esac')
    result = subprocess.run(['/bin/bash', str(root / 'scripts/publish-npm.sh'),'--no-build'], env=env,cwd=root,capture_output=True,text=True)
    assert result.returncode != 0 and 'main package was not published' in result.stderr, (result.stdout,result.stderr)
    lines = calls.read_text().splitlines()
    assert sum(' publish ' in line for line in lines) == 5, lines
    assert not any('/npm/bladebro publish ' in line for line in lines), lines
    # 30 platform-propagation iterations × 5 packages (the deadline was
    # widened 12→30 in the 4.2.0 post-ship fix; the fixture never propagates).
    assert sum(' view ' in line for line in lines) == 150, 'propagation deadline not reached'
    print('PASS exact-version mismatch reaches propagation deadline and prevents meta publish')
    # Exercise the real CI block against command receipts, never a publisher.
    text = (root / 'release.sh').read_text()
    start = text.index('git push origin main\n', text.index('# Native CI is a release gate.'))
    end = text.index('\ncheck_release_security\n', start)
    gate = root / 'ci-gate.sh'
    gate.write_text('set -euo pipefail\n' + text[start:end] + '\necho CI_GATE_PASSED\n')
    script('git', 'test "$1" = push')
    script('gh', r"""exec python3 - "$@" <<'GH'
import json,os,sys
args=sys.argv[1:];mode=os.environ['TEST_CI_MODE'];sha=os.environ['RELEASE_SHA']
with open(os.environ['TEST_CI_CALLS'],'a') as f:f.write(json.dumps(args)+'\n')
if args[:2]==['run','list']:
    assert args[args.index('--commit')+1]==sha
    event=args[args.index('--event')+1]
    if event=='push' and mode=='push':print('101')
    elif event=='workflow_dispatch' and mode in ('dispatch','ci-failure'):print('202')
elif args[0]=='api':
    assert args[1]=='repos/fixture/bladebro/git/ref/heads/main'
    print('b'*40 if mode=='advanced' else sha)
elif args[:2]==['workflow','run']:assert args==['workflow','run','CI','--ref','main']
elif args[:2]==['run','watch']:
    assert args==['run','watch','101' if mode=='push' else '202','--exit-status']
    if mode=='ci-failure':sys.exit(1)
else:raise AssertionError(args)
GH""")
    env.update(RELEASE_SHA='a'*40, RELEASE_REPO='fixture/bladebro', TEST_CI_CALLS=str(root/'ci-calls'))
    for mode in ('push','dispatch','advanced','race','ci-failure'):
        calls_path = root / 'ci-calls'
        calls_path.unlink(missing_ok=True)
        env['TEST_CI_MODE'] = mode
        result = subprocess.run(['/bin/bash',str(gate)],env=env,cwd=root,capture_output=True,text=True)
        assert calls_path.exists(),(mode,result.stderr)
        calls = [json.loads(line) for line in calls_path.read_text().splitlines()]
        dispatches = [a for a in calls if a[:2]==['workflow','run']]
        watches = [a for a in calls if a[:2]==['run','watch']]
        if mode in ('push','dispatch'):
            assert result.returncode==0 and 'CI_GATE_PASSED' in result.stdout, (mode,result.stderr)
            assert len(watches)==1 and len(dispatches)==(mode=='dispatch'),calls
        else:
            assert result.returncode!=0 and 'CI_GATE_PASSED' not in result.stdout,(mode,result.stdout)
            assert len(watches)==(mode=='ci-failure') and len(dispatches)==(mode!='advanced'),calls
        if mode=='advanced':assert 'main advanced' in result.stderr
        if mode=='race':
            assert 'no native CI run' in result.stderr
            assert sum('--event' in a and a[a.index('--event')+1]=='workflow_dispatch' for a in calls)==12
        print('PASS release CI gate:',mode)
    print('RELEASE PREFLIGHT 10/10')
