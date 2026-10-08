#!/usr/bin/env python3
"""Exercise the real Hermes CLI and runtime in disposable homes (no model calls)."""
import argparse
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading

p = argparse.ArgumentParser()
p.add_argument('--binary', default='target/release/bladebro', type=Path)
p.add_argument('--hermes', default=shutil.which('hermes'))
p.add_argument('--hermes-root', type=Path, required=True)
p.add_argument('--python', type=Path, required=True, help='Python from the installed Hermes dependency environment')
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()
a.binary = a.binary.resolve()
a.output.mkdir(parents=True, exist_ok=True)
count = 0


def check(ok, label):
    global count
    assert ok, label
    count += 1
    print(f'PASS {label}', flush=True)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        page = '''<!doctype html><title>Hermes fixture</title><h1>Hermes fixture</h1>
<p>HERMES_VISIBLE_SENTINEL</p><p hidden>HERMES_HIDDEN_SENTINEL</p>
<label>Name<input id="name"></label><button onclick="document.querySelector('#receipt').textContent='Saved '+document.querySelector('#name').value">Save</button><p id="receipt"></p>'''
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.end_headers()
        self.wfile.write(page.encode())
    def log_message(self, *_):
        pass


with tempfile.TemporaryDirectory(prefix='blade-hermes-probe-') as tmp:
    root = Path(tmp)
    home = root / 'Hermes home Δ with spaces'
    home.mkdir(mode=0o700)
    env = dict(os.environ, HERMES_HOME=str(home), BLADE_HOME=str(root / 'browser'),
               BLADE_NO_UPDATE_CHECK='1', PYTHONIOENCODING='utf-8', NO_COLOR='1')
    env.pop('HERMES_PROFILE', None)
    env.pop('BLADE_LANE', None)
    long_tmp = root / ('long harness temporary directory ' * 3)
    long_tmp.mkdir(mode=0o700)
    env['TMPDIR'] = str(long_tmp)
    cfg = home / 'config.yaml'
    state = home / '.bladebro-browser/state.json'
    base = {'agent': {'disabled_toolsets': ['tts']}, 'mcp_servers': {
        'keep': {'command': 'never-run', 'enabled': False, 'env': {'SENTINEL': 'preserve'}}},
        'browser': {'backend': 'off'}, 'platform_toolsets': {'cli': ['hermes-cli']},
        'terminal': {'backend': 'local'}}

    def write(c):
        cfg.write_text(json.dumps(c, ensure_ascii=False), encoding='utf-8')
        cfg.chmod(0o600)

    def run(command, label, ok=True):
        result = subprocess.run([str(x) for x in command], env=env, capture_output=True,
                                text=True, encoding='utf-8', timeout=240)
        (a.output / f'{label}.log').write_text(result.stdout + result.stderr, encoding='utf-8')
        check((result.returncode == 0) == ok, f'{label}: expected exit status (actual {result.returncode})')
        return result

    def cli(*args, label, ok=True, hermes=None):
        result = run([a.binary, 'hermes', *args, '--hermes', hermes or a.hermes, '--json'], label, ok)
        body = json.loads(result.stdout)
        check(body['ok'] == ok and body['is_error'] != ok, f'{label}: one honest JSON object')
        return body

    def read():
        # Hermes rewrites YAML; get its public raw JSON rather than adding a YAML
        # dependency to this harness. Read only the two sections under test.
        out = {}
        for key in ['agent', 'mcp_servers', 'browser']:
            r = subprocess.run([a.hermes, 'config', 'get', key, '--json', '--raw'], env=env,
                               capture_output=True, text=True, timeout=240)
            assert r.returncode == 0, (key, r.stderr)
            out[key] = json.loads(r.stdout)
        return out

    write(base)
    # Boot a new home before testing, so dependency preparation is independently
    # observable instead of being confused with a browser setup failure.
    run([a.hermes, 'config', 'path'], 'prepare')
    baseline = read()
    cli('on', label='on')
    got = read()
    check(got['agent']['disabled_toolsets'] == ['tts', 'browser'], 'on preserves other suppressions')
    check(got['mcp_servers']['keep'] == base['mcp_servers']['keep'], 'unrelated MCP entry preserved')
    check(got['browser'] == baseline['browser'], 'backend choice preserved')
    check(state.exists(), 'recovery exists before restore')
    if os.name != 'nt':
        check(state.stat().st_mode & 0o777 == 0o600, 'recovery file private')
        check(state.parent.stat().st_mode & 0o777 == 0o700, 'recovery directory private')
    snapshot = state.read_bytes()
    cli('on', label='on-again')
    check(state.read_bytes() == snapshot, 'idempotent on retains original restore point')
    check('Bladebro configured' in cli('status', label='status')['text'], 'status reports read-back state')
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        result = run([a.python, Path(__file__).with_name('runtime.py'), '--hermes-root', a.hermes_root,
                      '--url', f'http://127.0.0.1:{server.server_port}/'], 'runtime')
        check('PASS TOTAL ' in result.stdout, 'runtime reaches final assertions under a long TMPDIR')
        check((root / 'browser').is_dir(), 'MCP uses the explicit disposable Bladebro home')
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    run([a.hermes, 'config', 'set', 'agent.disabled_toolsets', '["tts","browser","memory"]'], 'unrelated-change')
    cli('off', label='off')
    got = read()
    check(got['agent']['disabled_toolsets'] == ['tts', 'memory'], 'restore preserves later unrelated suppression')
    check(got['mcp_servers'] == base['mcp_servers'], 'restore removes only managed MCP entry')
    check(not state.exists(), 'restore consumes recovery state')
    cli('off', label='off-again')

    result = run([a.python, Path(__file__).with_name('runtime.py'), '--hermes-root', a.hermes_root,
                  '--native'], 'native-catalog')
    check('PASS TOTAL ' in result.stdout, 'native catalog restored after off')
    run([a.hermes, 'profile', 'create', 'bladeprobe', '--no-alias', '--no-skills'], 'profile-create')
    default_before = cfg.read_bytes()
    cli('on', '--profile', 'bladeprobe', label='profile-on')
    check(cfg.read_bytes() == default_before, 'named profile leaves default config untouched')
    check('Bladebro configured' in cli('status', '--profile', 'bladeprobe', label='profile-status')['text'],
          'named profile setup reads back independently')
    cli('off', '--profile', 'bladeprobe', label='profile-off')
    check(cfg.read_bytes() == default_before, 'named profile restore leaves default untouched')

    # An existing custom Bladebro entry and prior browser suppression must survive.
    custom = json.loads(json.dumps(base))
    custom['agent']['disabled_toolsets'].append('browser')
    custom['mcp_servers']['bladebro'] = {'enabled': False, 'command': 'old-browser',
                                       'env': {'SENTINEL': 'private-prior-value'}}
    write(custom)
    cli('on', label='existing-on')
    cli('off', label='existing-off')
    got = read()
    check(got['agent']['disabled_toolsets'] == custom['agent']['disabled_toolsets'], 'prior browser suppression restored')
    check(got['mcp_servers'] == custom['mcp_servers'], 'prior custom MCP entry restored exactly')

    # Simulate transport failure using a wrapper around the REAL Hermes CLI.
    if os.name != 'nt':
        import shlex
        wrapper = root / 'hermes-wrapper'
        wrapper.write_text('#!/bin/sh\nif [ "$1 $2" = "mcp test" ]; then exit 1; fi\nexec ' +
                           shlex.quote(a.hermes) + ' "$@"\n')
        wrapper.chmod(0o700)
        write(base)
        cli('on', hermes=str(wrapper), label='probe-failure', ok=False)
        check(read()['agent']['disabled_toolsets'] == ['tts'], 'connection failure keeps native browser enabled')
        check(state.exists(), 'connection failure keeps recovery')
        cli('on', label='resume-after-failure')
        check('browser' in read()['agent']['disabled_toolsets'], 'interrupted on resumes')
        cli('off', label='resume-off')

        wrapper.write_text('#!/bin/sh\nif [ "$1 $2 $3" = "config set agent.disabled_toolsets" ]; then exit 0; fi\nexec ' +
                           shlex.quote(a.hermes) + ' "$@"\n')
        cli('on', hermes=str(wrapper), label='lying-writer', ok=False)
        check(read()['agent']['disabled_toolsets'] == ['tts'], 'exit zero without persistence is detected')
        cli('off', label='lying-writer-off')

    write(base)
    cli('on', label='conflict-on')
    installed = json.loads(state.read_text())['installed']
    run([a.hermes, 'config', 'set', 'mcp_servers.bladebro.command', 'user-edited'], 'conflict-edit')
    before = cfg.read_bytes()
    cli('off', label='conflict-off', ok=False)
    check(cfg.read_bytes() == before and state.exists(), 'conflicting edits are not overwritten')
    run([a.hermes, 'config', 'set', 'mcp_servers.bladebro', json.dumps(installed)], 'conflict-resolve')
    cli('off', label='conflict-restored')

    for label, invalid in [('malformed', 'agent: [unterminated'),
                           ('wrong-type', json.dumps({'agent': {'disabled_toolsets': 42}}))]:
        cfg.write_text(invalid)
        before = cfg.read_bytes()
        cli('on', label=label, ok=False)
        check(cfg.read_bytes() == before and not state.exists(), f'{label}: no config writes or restore point')
    write(base)
    state.write_text('{"version":999}')
    state.chmod(0o600)
    cli('on', label='bad-state', ok=False)
    check(read()['mcp_servers'] == base['mcp_servers'], 'invalid recovery state changes nothing')
    state.unlink()
    if os.name != 'nt':
        target = root / 'sentinel'
        target.write_text('sentinel')
        state.symlink_to(target)
        cli('on', label='symlink-state', ok=False)
        check(target.read_text() == 'sentinel', 'symlink target unchanged')
        state.unlink()
    print(f'PASS TOTAL {count}', flush=True)
