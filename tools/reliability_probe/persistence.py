#!/usr/bin/env python3
"""Verify owned-profile storage flush across CLI/MCP exits, a Ctrl-C to a one-shot
command's process group, and forked launchers.

BLADEBRO selects the candidate. POLISH_OUT retains actual calls/readbacks.
--baseline records old-binary failures. Only disposable profiles are used.
"""
import hashlib
import http.server
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading

BINARY = os.environ.get('BLADEBRO', str(Path(__file__).resolve().parents[2] / 'target/release/bladebro'))
OUT = Path(os.environ.get('POLISH_OUT', 'work/persistence/receipts.json'))
OUT.parent.mkdir(parents=True, exist_ok=True)
VALUE = 'persistent π🦀 ' + 'value=' * 40
checks, receipts = [], []
WRITTEN, RELEASE = threading.Event(), threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/held':
            # Write storage, report it, then hold the document open: the
            # command that loads this page is still running when signalled.
            script = f"localStorage.setItem('draft',{json.dumps(VALUE)});fetch('/written')"
            self.send_response(200)
            self.send_header('Content-Type', 'text/html; charset=utf-8')
            self.send_header('Connection', 'close')
            self.end_headers()
            try:
                self.wfile.write(f'<!doctype html><title>Held</title><script>{script}</script>'.encode() + b' ' * 2048)
                self.wfile.flush()
                RELEASE.wait(60)
            except OSError:
                pass
            return
        if self.path == '/written':
            WRITTEN.set()
        body = b'<!doctype html><title>Persistence fixture</title><p>Owned storage</p>'
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def check(name, passed):
    checks.append(dict(name=name, passed=bool(passed)))
    print(('PASS ' if passed else 'FAIL ') + name, flush=True)


def environment(home, chrome=None, transport=None):
    env = dict(os.environ, BLADE_HOME=str(home), BLADE_LANE='agent',
               BLADE_INCOGNITO='0', BLADE_NO_WARMING='1', BLADE_NO_UPDATE_CHECK='1')
    for key in ('BLADE_PROFILE_DIR', 'BLADE_TRANSPORT', 'BLADE_FRESH', 'BLADE_PROXY',
                'BLADE_CHROME_FLAGS', 'DISPLAY', 'WAYLAND_DISPLAY', 'XAUTHORITY'):
        env.pop(key, None)
    if chrome:
        env['CHROME_PATH'] = str(chrome)
    if transport:
        env['BLADE_TRANSPORT'] = transport
    return env


def owned_processes(home):
    if not sys.platform.startswith('linux'):
        return None
    processes = []
    for path in Path('/proc').iterdir():
        if path.name.isdigit():
            try:
                command = (path / 'cmdline').read_bytes()
            except OSError:
                continue
            if str(home).encode() in command:
                processes.append(int(path.name))
    return processes


def one_shot(home, origin, chrome=None):
    env = environment(home, chrome)
    outputs = []
    for js in [f"(localStorage.setItem('draft',{json.dumps(VALUE)}),localStorage.getItem('draft'))",
               "localStorage.getItem('draft')"]:
        command = [BINARY, '--no-daemon', '--json', 'act', 'eval', '--url', origin, '--js', js]
        result = subprocess.run(command, env=env, capture_output=True, text=True, encoding='utf-8', timeout=60)
        receipts.append(dict(command=command, stdout=result.stdout, stderr=result.stderr, code=result.returncode))
        response = json.loads(result.stdout)
        check(home.name + ' CLI exits successfully ' + str(len(outputs)), result.returncode == 0 and response.get('ok'))
        text = response.get('text', '')
        value = next((json.loads(line[8:]) for line in text.splitlines() if line.startswith('result: ')), None)
        outputs.append(value)
    check(home.name + ' write readback', outputs[0] == VALUE)
    check(home.name + ' full storage survives fresh one-shot browser', outputs[1] == VALUE)
    remaining = owned_processes(home)
    if remaining is not None:
        check(home.name + ' leaves no owned processes', not remaining)


def mcp(home, origin, transport='ws', chrome=None):
    env = environment(home, chrome, transport)
    values = []
    for writing in (True, False):
        operations = [('act', dict(action='navigate', url=origin))]
        if writing:
            operations.append(('state', dict(op='set-ls', name='draft', value=VALUE)))
        operations.append(('act', dict(action='eval', js="localStorage.getItem('draft')", format='json')))
        requests = [dict(jsonrpc='2.0', id=i, method='tools/call', params=dict(name=tool, arguments=args))
                    for i, (tool, args) in enumerate(operations, 1)]
        result = subprocess.run([BINARY, 'mcp'], env=env, input=''.join(json.dumps(r) + '\n' for r in requests),
                                capture_output=True, text=True, encoding='utf-8', timeout=60)
        responses = [json.loads(line) for line in result.stdout.splitlines()]
        receipts.append(dict(transport=transport, requests=requests, responses=responses, stderr=result.stderr, code=result.returncode))
        good = result.returncode == 0 and len(responses) == len(requests) and all(
            'error' not in r and not r.get('result', {}).get('isError') for r in responses)
        check(home.name + ' MCP calls and clean EOF ' + str(writing), good)
        text = '\n'.join(c.get('text', '') for c in responses[-1].get('result', {}).get('content', []))
        value = json.loads(text) if good else None
        if isinstance(value, dict) and 'data' in value and 'note' in value:
            value = value['data']
        values.append(value)
    check(home.name + ' write readback', values[0] == VALUE)
    check(home.name + ' full storage survives MCP EOF', values[1] == VALUE)
    remaining = owned_processes(home)
    if remaining is not None:
        check(home.name + ' leaves no owned processes', not remaining)


def interrupted(home, origin):
    """Ctrl-C to a one-shot command's whole process group (a terminal's ^C)
    while it runs: the owned browser still closes gracefully, so storage
    written just before the signal survives the restart."""
    env = environment(home)
    WRITTEN.clear()
    RELEASE.clear()
    command = [BINARY, '--no-daemon', 'nav', origin + '/held']
    process = subprocess.Popen(command, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, encoding='utf-8', start_new_session=True)
    reached = WRITTEN.wait(60) and process.poll() is None
    if reached:
        os.killpg(process.pid, signal.SIGINT)
    try:
        stdout, stderr = process.communicate(timeout=60)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate()
    RELEASE.set()
    receipts.append(dict(command=command, signal='SIGINT to the process group', stdout=stdout, stderr=stderr,
                         code=process.returncode))
    check(home.name + ' command in flight at Ctrl-C', reached)
    check(home.name + ' dies of the Ctrl-C after cleanup', process.returncode == -signal.SIGINT)
    check(home.name + ' graceful close after Ctrl-C',
          'graceful close of the owned browser was unavailable' not in stderr)
    js = "localStorage.getItem('draft')"
    command = [BINARY, '--no-daemon', '--json', 'act', 'eval', '--url', origin, '--js', js]
    result = subprocess.run(command, env=env, capture_output=True, text=True, encoding='utf-8', timeout=60)
    receipts.append(dict(command=command, stdout=result.stdout, stderr=result.stderr, code=result.returncode))
    text = json.loads(result.stdout).get('text', '') if result.stdout.strip() else ''
    value = next((json.loads(line[8:]) for line in text.splitlines() if line.startswith('result: ')), None)
    check(home.name + ' storage written before Ctrl-C survives', value == VALUE)
    remaining = owned_processes(home)
    if remaining is not None:
        check(home.name + ' leaves no owned processes', not remaining)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
origin = 'http://127.0.0.1:' + str(server.server_port)
try:
    with tempfile.TemporaryDirectory(prefix='blade-persistence-') as scratch:
        root = Path(scratch)
        one_shot(root / 'one-shot', origin)
        mcp(root / 'mcp-ws', origin)
        if os.name == 'posix':
            mcp(root / 'mcp-pipe', origin, 'pipe')
            interrupted(root / 'ctrl-c-one-shot', origin)
        if sys.platform.startswith('linux'):
            chrome = os.environ.get('CHROME_PATH') or shutil.which('chromium') or shutil.which('google-chrome-stable') or shutil.which('google-chrome')
            assert chrome, 'Chrome is required for the forked-launcher fixture'
            wrapper = root / 'browser-wrapper'
            wrapper.write_text('#!/bin/sh\n' + shlex.quote(chrome) + ' "$@" &\nwait\n')
            wrapper.chmod(0o700)
            one_shot(root / 'wrapper-one-shot', origin, wrapper)
            mcp(root / 'wrapper-mcp', origin, chrome=wrapper)
finally:
    server.shutdown()
    server.server_close()
    OUT.write_text(json.dumps(dict(binary=BINARY, sha256=hashlib.sha256(Path(BINARY).read_bytes()).hexdigest(),
                                  checks=checks, receipts=receipts), indent=2))
failed = sum(not c['passed'] for c in checks)
print(f'PERSISTENCE {len(checks)-failed}/{len(checks)}; failures {failed}', flush=True)
sys.exit(bool(failed) and '--baseline' not in sys.argv)
