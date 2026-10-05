#!/usr/bin/env python3
"""Live mutation/state regression suite. Own Chrome, HTTP fixture and data home.
BLADEBRO=/path/to/binary python3 tools/reliability_probe/run.py
"""
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time

BINARY = os.environ.get('BLADEBRO', str(Path(__file__).resolve().parents[2] / 'target/release/bladebro'))
CHROME = os.environ.get('CHROME', shutil.which('chromium') or shutil.which('google-chrome') or '')
assert CHROME, 'Chrome/Chromium is required'
checks = 0
latencies = []
effects = []


def check(name, condition):
    global checks
    assert condition, name
    checks += 1
    print('PASS', name, flush=True)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'''<title>Reliability</title><button id="effect" onclick="fetch('/effect',{method:'POST'})">Send once</button><input id="field"><button id="submit" onclick="fetch('/effect',{method:'POST'})">Submit once</button>'''
        self.send_response(200)
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        effects.append(self.path)
        self.send_response(204)
        self.end_headers()

    def log_message(self, *args):
        pass


with tempfile.TemporaryDirectory(prefix='blade-reliability-') as root:
    home = Path(root) / 'blade'
    env = dict(os.environ, BLADE_HOME=str(home), BLADE_NO_WARMING='1', BLADE_NO_UPDATE_CHECK='1', BLADE_LANE='agent')
    for key in ('DISPLAY', 'WAYLAND_DISPLAY', 'XAUTHORITY'):
        env.pop(key, None)
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    chrome = subprocess.Popen([CHROME, '--headless=new', '--no-sandbox', '--no-first-run', '--remote-debugging-port=0', '--user-data-dir=' + root + '/chrome', 'about:blank'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        portfile = Path(root) / 'chrome/DevToolsActivePort'
        deadline = time.monotonic() + 15
        while not portfile.exists():
            assert chrome.poll() is None and time.monotonic() < deadline, 'Chrome startup failed'
            time.sleep(.025)
        port = portfile.read_text().splitlines()[0]

        def cli(*args, ok=True):
            start = time.monotonic()
            result = subprocess.run([BINARY, '--port', port, '--json', *args], env=env, capture_output=True, text=True, timeout=40)
            elapsed = time.monotonic() - start
            parsed = json.loads(result.stdout)
            if ok:
                assert result.returncode == 0 and parsed['ok'] and not parsed['is_error'], (args, parsed, result.stderr)
            else:
                assert result.returncode != 0 and (not parsed['ok'] or parsed['is_error']), (args, parsed)
            return parsed['text'], elapsed

        def evaluate(js):
            text, _ = cli('act', 'eval', js)
            assert text.startswith('result: '), text
            return json.loads(text[len('result: '):])

        origin = 'http://127.0.0.1:' + str(server.server_port)
        foreign = 'http://localhost:' + str(server.server_port)
        cli('nav', origin)
        _, elapsed = cli('act', 'click', '--selector', '#effect')
        latencies.append(elapsed)
        time.sleep(.1)
        check('one accepted click sends exactly one HTTP mutation', len(effects) == 1)
        _, elapsed = cli('act', 'fill', json.dumps([{'selector': '#field', 'text': 'π🦀'}]), '--submit', 'Submit once')
        latencies.append(elapsed)
        time.sleep(.1)
        check('fill submits exactly once without a DOM effect', len(effects) == 2)
        check('Unicode fill readback', evaluate("document.querySelector('#field').value") == 'π🦀')
        cli('act', 'fill', json.dumps([{'selector': '#field', 'text': 'changed'}, {'text': 'missing target'}]), ok=False)
        check('malformed later fill field rejected before any mutation', evaluate("document.querySelector('#field').value") == 'π🦀')
        secret = 'π🦀 long secret\n' + 'x' * 512
        cli('state', 'set-ls', 'draft', secret)
        cli('state', 'set-ls', '', 'empty-key')
        cli('state', 'set-cookie', 'auth', 'session-secret')
        cli('state','set-cookie','scoped','domain-secret','--domain','.example.com')
        check('explicit cookie domain overrides current page default', 'scoped=domain-secret' in cli('state','cookies','--url','http://example.com')[0])
        cli('state','del-cookie','scoped','--domain','.example.com')
        check('explicit delete scope removes the requested domain', 'scoped=' not in cli('state','cookies','--url','http://example.com')[0])
        check('explicit delete preserves current site cookie', evaluate("document.cookie.includes('auth=session-secret')"))
        cli('state', 'save', 'roundtrip')
        saved = home / 'sessions/roundtrip.json'
        content = json.loads(saved.read_text())
        check('saved values preserve Unicode, newlines and full length', any(e['key'] == 'draft' and e['value'] == secret for e in content['localStorage']))
        if os.name == 'posix':
            check('session file private at creation', saved.stat().st_mode & 0o777 == 0o600)
        cli('state', 'clear-ls')
        cli('state', 'del-cookie', 'auth')
        check('clear and delete readback', evaluate("localStorage.length===0 && document.cookie===''"))
        cli('nav', foreign)
        cli('state', 'load', 'roundtrip', ok=False)
        check('origin mismatch changes neither storage nor cookies', evaluate("localStorage.length===0 && document.cookie===''"))
        cli('nav', origin)
        cli('state', 'load', 'roundtrip')
        check('session cookie restores without becoming expired', evaluate("document.cookie.includes('auth=session-secret')"))
        check('localStorage restores full fidelity', evaluate("localStorage.getItem('draft')") == secret)
        check('empty storage key restores', evaluate("localStorage.getItem('')") == 'empty-key')
        cli('state', 'set-cookie', '__Host-invalid', 'x', '--secure', '--path', '/invalid', ok=False)
        check('rejected cookie never reports success', not evaluate("document.cookie.includes('__Host-invalid')"))
        for name in ('CON', 'a:b', '../escape', 'LPT1', 'trailing.'):
            cli('state', 'save', name, ok=False)
            check('portable session name rejected: ' + name, not (home / 'sessions' / (name + '.json')).exists())
        # Explicit MCP attach must respect the caller's cookie store.
        sidecar = home / 'logins.json'
        sidecar.write_text(json.dumps([{'name':'external-secret','value':'must-not-inject','domain':'127.0.0.1','path':'/','secure':False,'httpOnly':False,'expires':-1}]))
        before = sidecar.read_bytes()
        requests = [
            {'jsonrpc':'2.0','id':1,'method':'initialize','params':{}},
            {'jsonrpc':'2.0','id':2,'method':'tools/call','params':{'name':'see','arguments':{}}},
            {'jsonrpc':'2.0','id':3,'method':'tools/call','params':{'name':'state','arguments':{'op':'set-ls','name':'','value':['invalid']}}},
        ]
        attached = subprocess.run([BINARY,'--port',port,'mcp'], input=''.join(json.dumps(r)+'\n' for r in requests), env=env, capture_output=True, text=True, timeout=30)
        responses = [json.loads(line) for line in attached.stdout.splitlines()]
        check('MCP attach returns actual page result', attached.returncode == 0 and any(r.get('id') == 2 and 'result' in r for r in responses))
        check('invalid state value rejected before mutation', any(r.get('id') == 3 and r.get('result',{}).get('isError') for r in responses) and evaluate("localStorage.getItem('')") == 'empty-key')
        check('MCP attach does not inject saved cookies', not evaluate("document.cookie.includes('external-secret')"))
        check('MCP attach leaves private login sidecar unchanged', sidecar.read_bytes() == before)
        # Owned daemon persistence: both sites survive; explicit deletions win.
        owned_home = Path(root) / 'owned'
        owned_env = dict(env, BLADE_HOME=str(owned_home))
        def owned(*args):
            result = subprocess.run([BINARY,'--json',*args],env=owned_env,capture_output=True,text=True,timeout=45)
            value = json.loads(result.stdout)
            assert result.returncode == 0 and value['ok'] and not value['is_error'], (args,value,result.stderr)
            return value['text']
        try:
            owned('nav', origin)
            owned('state','set-cookie','site-a','a-secret')
            owned('nav', foreign)
            owned('state','set-cookie','site-b','b-secret')
            owned('stop')
            login_path = owned_home / 'logins.json'
            check('snapshot retains both sites after cross-site navigation', {c['name'] for c in json.loads(login_path.read_text())} == {'site-a','site-b'})
            owned('nav', origin)
            check('site A session cookie survives a fresh browser', 'site-a=a-secret' in owned('state','cookies'))
            owned('state','del-cookie','site-a')
            owned('stop')
            check('deleted cookie removed from authoritative snapshot', {c['name'] for c in json.loads(login_path.read_text())} == {'site-b'})
            owned('nav', origin)
            check('restart does not resurrect deleted cookie', 'site-a=' not in owned('state','cookies'))
            owned('nav', foreign)
            check('site B survived deletion on site A', 'site-b=b-secret' in owned('state','cookies'))
            owned('state','del-cookie','site-b')
            owned('stop')
            check('explicit logout writes empty cookie store', json.loads(login_path.read_text()) == [])
        finally:
            subprocess.run([BINARY,'stop'],env=owned_env,capture_output=True,timeout=45)
        cli('nav', 'about:blank')
        for args in (('clear-ls',), ('set-ls', 'draft', 'x'), ('rm-ls', 'draft'), ('clear-ss',), ('set-ss', 'draft', 'x'), ('rm-ss', 'draft')):
            text, _ = cli('state', *args, ok=False)
            check('opaque origin surfaces storage error: ' + args[0], 'SecurityError' in text or 'denied' in text.lower())
        print('LATENCY click/fill seconds:', ', '.join(f'{v:.3f}' for v in latencies), flush=True)
        print(f'RELIABILITY {checks}/{checks}', flush=True)
    finally:
        chrome.terminate()
        try:
            chrome.wait(timeout=10)
        except subprocess.TimeoutExpired:
            chrome.kill()
            chrome.wait()
        server.shutdown()
        server.server_close()
