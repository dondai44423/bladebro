#!/usr/bin/env python3
"""Owned-browser regression checks for navigation, setup and OTR contexts.

Each invocation owns its HTTP server, daemon/MCP processes and disposable
homes. Set BLADEBRO to test an exact candidate; no user profile is read.
"""
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time

BINARY = os.environ.get('BLADEBRO', str(Path(__file__).resolve().parents[2] / 'target/release/bladebro'))
CHECKS = 0
REQUESTS = []
EFFECTS = []


def check(name, condition, detail=None):
    global CHECKS
    assert condition, (name, detail)
    CHECKS += 1
    print('PASS', name, flush=True)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        REQUESTS.append(self.path)
        body = b'''<title>Setup fixture</title><h1 id="route"></h1>
        <button onclick="fetch('/effect',{method:'POST'})">Send once</button>
        <script>function render(){document.querySelector('h1').textContent=location.pathname+location.search+location.hash}
        addEventListener('hashchange',render);render()</script>'''
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        EFFECTS.append(self.path)
        self.send_response(204)
        self.end_headers()

    def log_message(self, *_):
        pass


def environment(home, mode):
    env = dict(os.environ, BLADE_HOME=str(home), BLADE_LANE='agent',
               BLADE_INCOGNITO=mode, BLADE_NO_WARMING='1', BLADE_NO_UPDATE_CHECK='1')
    for name in ('BLADE_CHROME_FLAGS', 'BLADE_PROFILE_DIR', 'BLADE_FRESH',
                 'BLADE_TRANSPORT', 'DISPLAY', 'WAYLAND_DISPLAY', 'XAUTHORITY'):
        env.pop(name, None)
    return env


def cli(env, *args, ok=True):
    start = time.monotonic()
    result = subprocess.run([BINARY, '--json', *args], env=env,
                            capture_output=True, text=True, timeout=60)
    value = json.loads(result.stdout)
    good = result.returncode == 0 and value.get('ok') and not value.get('is_error')
    assert bool(good) == ok, (args, result.returncode, value, result.stderr)
    return value['text'], time.monotonic() - start


def value_from_eval(text):
    line = next(line for line in text.splitlines() if line.startswith('result: '))
    return json.loads(line[len('result: '):])


def owned_browser(root, origin, mode, raw=False):
    label = mode + ('-raw' if raw else '')
    home = root / ('owned-' + label)
    env = environment(home, mode)
    if raw:
        env['BLADE_CHROME_FLAGS'] = '--incognito'
    def evaluate(js):
        return value_from_eval(cli(env, 'act', 'eval', js)[0])
    try:
        text, elapsed = cli(env, 'nav', origin)
        check(label + ' startup reaches actual HTTP fixture', 'Setup fixture' in text, text)
        print('TIMING', label, 'cold navigation', round(elapsed, 3), flush=True)
        count = len(REQUESTS)
        cli(env, 'nav', origin + '/')
        check(label + ' equivalent root navigation does not reload', len(REQUESTS) == count)
        for suffix in ('/#first', '/#second', '/route', '/route/', '/?view=x'):
            text, elapsed = cli(env, 'nav', origin + suffix)
            check(label + ' exact URL ' + suffix, evaluate('location.href') == origin + suffix, text)
            check(label + ' rendered route ' + suffix, evaluate("document.querySelector('h1').textContent") == suffix)
            check(label + ' navigation has no ten-second event penalty ' + suffix, elapsed < 5, elapsed)
            print('TIMING', label, suffix, round(elapsed, 3), flush=True)
        before = len(EFFECTS)
        cli(env, 'act', 'click', '--text', 'Send once')
        check(label + ' quiet mutation dispatches once', len(EFFECTS) == before + 1)
        cli(env, 'state', 'set-cookie', 'setup-auth', 'saved')
        cli(env, 'state', 'set-ls', 'draft', 'π🦀')
        cli(env, 'state', 'save', 'setup')
        cli(env, 'state', 'open-tab', origin)
        check(label + ' new tab shares the live cookie context', evaluate("document.cookie.includes('setup-auth=saved')"))
        check(label + ' new tab shares localStorage', evaluate("localStorage.getItem('draft')") == 'π🦀')
        cli(env, 'state', 'clear-ls')
        cli(env, 'state', 'load', 'setup')
        check(label + ' named session restores Unicode storage', evaluate("localStorage.getItem('draft')") == 'π🦀')
        cli(env, 'stop')
        sidecar = json.loads((home / 'logins.json').read_text())
        check(label + ' browser-wide snapshot reads the active context', any(c['name'] == 'setup-auth' and c['value'] == 'saved' for c in sidecar), sidecar)
        cli(env, 'nav', origin)
        check(label + ' login survives a fresh owned browser', evaluate("document.cookie.includes('setup-auth=saved')"))
        if mode == '0' and not raw:
            check(label + ' ordinary profile retains automatic localStorage', evaluate("localStorage.getItem('draft')") == 'π🦀')
        else:
            check(label + ' OTR storage loss is explicit and accurate', evaluate("localStorage.getItem('draft')") is None)
            cli(env, 'state', 'load', 'setup')
            check(label + ' saved storage restores after OTR restart', evaluate("localStorage.getItem('draft')") == 'π🦀')
        cli(env, 'state', 'del-cookie', 'setup-auth')
        cli(env, 'stop')
        cli(env, 'nav', origin)
        check(label + ' logout is never resurrected', not evaluate("document.cookie.includes('setup-auth=')"))
    finally:
        subprocess.run([BINARY, 'stop'], env=env, capture_output=True, timeout=45)


def mcp(root, origin, transport):
    env = environment(root / ('mcp-' + transport), '1')
    env['BLADE_TRANSPORT'] = transport
    def run(operations):
        requests = [{'jsonrpc':'2.0', 'id':1, 'method':'initialize', 'params':{}}]
        requests += [{'jsonrpc':'2.0', 'id':i + 2, 'method':'tools/call', 'params':{'name':name, 'arguments':args}} for i, (name, args) in enumerate(operations)]
        result = subprocess.run([BINARY, 'mcp'], input=''.join(json.dumps(r) + '\n' for r in requests),
                                env=env, capture_output=True, text=True, timeout=120)
        replies = {r['id']:r for r in map(json.loads, result.stdout.splitlines()) if 'id' in r}
        assert result.returncode == 0 and len(replies) == len(requests), (result.returncode, replies, result.stderr)
        texts = []
        for i in range(2, len(requests) + 1):
            response = replies[i]
            assert 'error' not in response and not response['result'].get('isError'), response
            texts.append('\n'.join(c.get('text', '') for c in response['result']['content']))
        return texts
    texts = run([
        ('act', {'action':'navigate', 'url':origin}),
        ('state', {'op':'set-cookie', 'name':'mcp-auth', 'value':'persist'}),
        ('state', {'op':'set-ls', 'name':'draft', 'value':'π🦀'}),
        ('state', {'op':'open-tab', 'url':origin}),
        ('act', {'action':'eval', 'js':"({cookie:document.cookie,draft:localStorage.getItem('draft')})"}),
    ])
    value = value_from_eval(texts[-1])
    check(transport + ' MCP new tab keeps cookie context', 'mcp-auth=persist' in value['cookie'])
    check(transport + ' MCP new tab keeps storage context', value['draft'] == 'π🦀')
    texts = run([('act', {'action':'navigate', 'url':origin}), ('act', {'action':'eval', 'js':'document.cookie'})])
    check(transport + ' MCP EOF snapshot survives restart', 'mcp-auth=persist' in value_from_eval(texts[-1]))


def setup_errors(root):
    env = environment(root / 'invalid', '0')
    env['CHROME_PATH'] = str(root)
    result = subprocess.run([BINARY, 'doctor'], env=env, capture_output=True, text=True, timeout=35)
    check('doctor rejects a directory masquerading as Chrome and exits nonzero', result.returncode != 0 and 'CHROME_PATH is not an executable file' in result.stdout, result.stdout)
    env['CHROME_PATH'] = str(root / 'missing-browser')
    try:
        text, _ = cli(env, 'nav', 'about:blank', ok=False)
        check('invalid explicit browser path never silently launches a different browser', 'CHROME_PATH is not an executable file' in text, text)
    finally:
        subprocess.run([BINARY, 'stop'], env=env, capture_output=True, timeout=45)
    if os.name == 'posix':
        broken = root / 'broken-browser'
        broken.write_text('#!/bin/sh\nprintf "Chromium 154.0.0.0\\n"\nexit 1\n')
        broken.chmod(0o700)
        env['CHROME_PATH'] = str(broken)
        result = subprocess.run([BINARY, 'doctor'], env=env, capture_output=True, text=True, timeout=35)
        check('doctor does not trust version text from a failed executable', result.returncode != 0 and '--version failed' in result.stdout, result.stdout)
        broken.write_text('#!/bin/sh\nwhile :; do :; done\n')
        start = time.monotonic()
        result = subprocess.run([BINARY, 'doctor'], env=env, capture_output=True, text=True, timeout=35)
        check('doctor kills a wedged version probe and reports failure', result.returncode != 0 and '--version failed' in result.stdout and time.monotonic() - start < 9, result.stdout)
    env = environment(root / 'invalid-policy', 'typo')
    try:
        text, _ = cli(env, 'nav', 'about:blank', ok=False)
        check('invalid compatibility setting fails with recovery guidance', 'BLADE_INCOGNITO must be auto, 0 or 1' in text, text)
        check('invalid compatibility setting creates no owned browser profile', not (Path(env['BLADE_HOME']) / 'profiles').exists())
    finally:
        subprocess.run([BINARY, 'stop'], env=env, capture_output=True, timeout=45)


with tempfile.TemporaryDirectory(prefix='blade-setup-') as directory:
    root = Path(directory)
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = 'http://127.0.0.1:' + str(server.server_port)
    try:
        setup_errors(root)
        owned_browser(root, origin, '0')
        owned_browser(root, origin, '1')
        owned_browser(root, origin, '0', raw=True)
        mcp(root, origin, 'ws')
        if os.name == 'posix':
            mcp(root, origin, 'pipe')
    finally:
        server.shutdown()
        server.server_close()
print(f'BROWSER SETUP {CHECKS}/{CHECKS}', flush=True)
