#!/usr/bin/env python3
"""Own disposable profiles only: reject unsafe fresh paths before Chrome writes."""
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

checks = 0
with tempfile.TemporaryDirectory(prefix='blade-profile-probe-') as tmp:
    root = Path(tmp)
    fixture = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    threading.Thread(target=fixture.serve_forever, daemon=True).start()
    try:
        for kind in ('writable', 'symlink', 'safe-existing'):
            env = dict(os.environ, BLADE_HOME=str(root / kind), BLADE_FRESH='1', BLADE_NO_WARMING='1', BLADE_NO_UPDATE_CHECK='1', BLADE_LANE='agent')
            for key in ('DISPLAY', 'WAYLAND_DISPLAY', 'XAUTHORITY'):
                env.pop(key, None)
            proc = subprocess.Popen([binary, 'mcp'], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
            profile = Path(tempfile.gettempdir()) / f'bladebro-chrome-{proc.pid}'
            assert not profile.exists(), 'test cannot adopt an existing directory'
            target = root / 'symlink-target'
            try:
                if kind == 'symlink':
                    target.mkdir(mode=0o755)
                    target.chmod(0o755)
                    (target / 'keep').write_text('preserve')
                    profile.symlink_to(target, target_is_directory=True)
                else:
                    profile.mkdir(mode=0o777 if kind == 'writable' else 0o755)
                    profile.chmod(0o777 if kind == 'writable' else 0o755)
                    (profile / 'keep').write_text('preserve')
                def call(identity, method, params):
                    proc.stdin.write(json.dumps({'jsonrpc':'2.0','id':identity,'method':method,'params':params})+'\n')
                    proc.stdin.flush()
                    assert select.select([proc.stdout], [], [], 30)[0], 'MCP response deadline exceeded'
                    response = json.loads(proc.stdout.readline())
                    assert response['id'] == identity
                    return response
                call(1, 'initialize', {'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'profile-permissions','version':'1'}})
                response = call(2, 'tools/call', {'name':'act','arguments':{'action':'navigate','url':f'http://127.0.0.1:{fixture.server_port}/'}})
                rejected = bool(response.get('error') or response.get('result', {}).get('isError'))
                if kind == 'safe-existing':
                    assert not rejected, response
                    assert 'private profile fixture' in json.dumps(response)
                    assert stat.S_IMODE(profile.stat().st_mode) == 0o700
                    assert (profile / 'keep').read_text() == 'preserve'
                else:
                    assert rejected and 'unsafe' in json.dumps(response), response
                    unchanged = target if kind == 'symlink' else profile
                    assert sorted(path.name for path in unchanged.iterdir()) == ['keep']
                    assert (unchanged / 'keep').read_text() == 'preserve'
                    assert stat.S_IMODE(unchanged.stat().st_mode) == (0o755 if kind == 'symlink' else 0o777)
                checks += 1
                print('PASS actual MCP fresh profile:', kind, flush=True)
            finally:
                proc.stdin.close()
                try:
                    proc.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
                # The path was absent until this test created it for this held PID.
                if profile.is_symlink():
                    profile.unlink()
                elif profile.exists():
                    shutil.rmtree(profile)
    finally:
        fixture.shutdown()
        fixture.server_close()
print(f'PROFILE PERMISSIONS {checks}/3')
