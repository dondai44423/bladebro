#!/usr/bin/env python3
"""Own disposable profiles only: fresh sessions are private; guessed-name
pre-plants are never adopted, entered, or mutated.

Contract 2026-10-07: BLADE_FRESH=1 sessions land in an unpredictable
`bladebro-chrome-<entropy>` dir created with exclusive mkdir — a pre-plant
at any GUESSED name (including the pre-4.2.0 pid-based scheme) can never be
hit, and a name collision fails creation and retries instead of adopting.
The deterministic pockets that remain under attacker influence are the
BLADE_PROFILE_DIR override and reused-owned dirs; both refuse symlinks and
non-dirs and secure (0700) anything they adopt.

Checked against a real MCP + Chromium:
  1-3. guessed-name plant (0777 / symlink / 0755) at the old pid name:
       navigate succeeds, plant content and mode stay untouched, and the
       ACTUAL fresh dir appears private (0700) and is removed on exit
  4.   BLADE_PROFILE_DIR = symlink      -> refused before Chrome writes
  5.   BLADE_PROFILE_DIR = plain file   -> refused before Chrome writes
  6.   BLADE_PROFILE_DIR = owned 0777   -> chmodded 0700, session works
"""
import http.server
import json
import os
from pathlib import Path
import select
import shutil
import stat
import subprocess
import tempfile
import threading

if os.name != 'posix':
    raise SystemExit('This permission probe requires Unix; Windows uses profile ACLs.')
binary = os.environ.get('BLADEBRO', str(Path(__file__).resolve().parents[2] / 'target/release/bladebro'))


class Fixture(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'<title>private profile fixture</title>')

    def log_message(self, *args):
        pass


def call(proc, identity, method, params):
    proc.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': identity, 'method': method, 'params': params}) + '\n')
    proc.stdin.flush()
    assert select.select([proc.stdout], [], [], 60)[0], 'MCP response deadline exceeded'
    response = json.loads(proc.stdout.readline())
    assert response['id'] == identity
    return response


def navigate(proc, port):
    call(proc, 1, 'initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                                 'clientInfo': {'name': 'profile-permissions', 'version': '1'}})
    return call(proc, 2, 'tools/call', {'name': 'act',
                'arguments': {'action': 'navigate', 'url': f'http://127.0.0.1:{port}/'}})


def rejected(response):
    return bool(response.get('error') or response.get('result', {}).get('isError'))


def close_mcp(proc):
    try:
        proc.stdin.close()
    except Exception:
        pass
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def fresh_dirs():
    return {p.name for p in Path(tempfile.gettempdir()).glob('bladebro-chrome-*')}


def base_env(blade_home):
    env = dict(os.environ, BLADE_HOME=str(blade_home), BLADE_FRESH='1',
               BLADE_NO_WARMING='1', BLADE_NO_UPDATE_CHECK='1', BLADE_LANE='agent')
    for key in ('DISPLAY', 'WAYLAND_DISPLAY', 'XAUTHORITY', 'BLADE_PROFILE_DIR'):
        env.pop(key, None)
    return env


checks = 0
with tempfile.TemporaryDirectory(prefix='blade-profile-probe-') as tmp:
    root = Path(tmp)
    fixture = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    threading.Thread(target=fixture.serve_forever, daemon=True).start()
    try:
        # ── 1-3: guessed-name pre-plants are ignored, untouched; the real
        # fresh dir is private and cleaned up.
        for kind in ('writable', 'symlink', 'safe-existing'):
            env = base_env(root / kind)
            proc = subprocess.Popen([binary, 'mcp'], env=env, stdin=subprocess.PIPE,
                                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
            # The pid name is a GUESS now (the real name is entropy-based):
            # plant it exactly where the old scheme would have looked.
            profile = Path(tempfile.gettempdir()) / f'bladebro-chrome-{proc.pid}'
            assert not profile.exists(), 'test cannot adopt an existing directory'
            target = root / f'symlink-target-{kind}'
            if kind == 'symlink':
                target.mkdir()
                target.chmod(0o755)
                (target / 'keep').write_text('preserve')
                profile.symlink_to(target, target_is_directory=True)
            else:
                mode = 0o777 if kind == 'writable' else 0o755
                profile.mkdir(mode=mode)
                profile.chmod(mode)
                (profile / 'keep').write_text('preserve')
            before = fresh_dirs()
            try:
                response = navigate(proc, fixture.server_port)
                assert not rejected(response), f'planted {kind} must not block the session: {response}'
                assert 'private profile fixture' in json.dumps(response), response
                # The guessed-name plant is untouched: same mode, same content.
                unchanged = target if kind == 'symlink' else profile
                assert sorted(path.name for path in unchanged.iterdir()) == ['keep']
                assert (unchanged / 'keep').read_text() == 'preserve'
                planted_mode = 0o777 if kind == 'writable' else 0o755
                assert stat.S_IMODE(unchanged.stat().st_mode) == planted_mode
                # The ACTUAL fresh dir: exactly one new, private, ours.
                new = fresh_dirs() - before
                assert len(new) == 1, f'expected one fresh dir, saw {sorted(new)}'
                actual = Path(tempfile.gettempdir()) / next(iter(new))
                assert stat.S_IMODE(actual.stat().st_mode) == 0o700
                close_mcp(proc)
                leftover = fresh_dirs() - before
                assert not leftover, f'fresh dir must be cleaned on exit, saw {sorted(leftover)}'
                checks += 1
                print(f'PASS guessed-name plant ignored ({kind}); fresh dir private + cleaned', flush=True)
            finally:
                if proc.poll() is None:
                    close_mcp(proc)
                if profile.is_symlink():
                    profile.unlink()
                elif profile.exists():
                    shutil.rmtree(profile)
                for name in fresh_dirs() - before:
                    shutil.rmtree(Path(tempfile.gettempdir()) / name, ignore_errors=True)

        # ── 4-6: the deterministic BLADE_PROFILE_DIR override.
        # 4: symlink -> refused.
        link = root / 'pd-link'
        target = root / 'pd-target'
        target.mkdir()
        link.symlink_to(target, target_is_directory=True)
        env = base_env(root / 'pd-symlink')
        env['BLADE_PROFILE_DIR'] = str(link)
        proc = subprocess.Popen([binary, 'mcp'], env=env, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        try:
            response = navigate(proc, fixture.server_port)
            assert rejected(response) and 'symlink' in json.dumps(response), response
            checks += 1
            print('PASS BLADE_PROFILE_DIR symlink refused', flush=True)
        finally:
            close_mcp(proc)

        # 5: plain file -> refused.
        notdir = root / 'pd-file'
        notdir.write_text('not a dir')
        env = base_env(root / 'pd-file-home')
        env['BLADE_PROFILE_DIR'] = str(notdir)
        proc = subprocess.Popen([binary, 'mcp'], env=env, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        try:
            response = navigate(proc, fixture.server_port)
            assert rejected(response) and 'not a directory' in json.dumps(response), response
            checks += 1
            print('PASS BLADE_PROFILE_DIR plain file refused', flush=True)
        finally:
            close_mcp(proc)

        # 6: owned 0777 dir -> secured to 0700, session works.
        pd = root / 'pd-writable'
        pd.mkdir()
        pd.chmod(0o777)
        env = base_env(root / 'pd-writable-home')
        env['BLADE_PROFILE_DIR'] = str(pd)
        proc = subprocess.Popen([binary, 'mcp'], env=env, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        try:
            response = navigate(proc, fixture.server_port)
            assert not rejected(response), response
            assert stat.S_IMODE(pd.stat().st_mode) == 0o700, oct(stat.S_IMODE(pd.stat().st_mode))
            checks += 1
            print('PASS BLADE_PROFILE_DIR loose owned dir secured to 0700', flush=True)
        finally:
            close_mcp(proc)
    finally:
        fixture.shutdown()
        fixture.server_close()
print(f'PROFILE PERMISSIONS {checks}/6')
