#!/usr/bin/env python3
"""Authenticated cross-origin downloads in disposable WS/pipe profiles.

Checks actual bytes, request cookies, failure honesty, tab cleanup and the
original page after successful and rejected downloads. No user profile used.
"""
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading

BINARY = os.environ.get('BLADEBRO', str(Path(__file__).resolve().parents[2] / 'target/release/bladebro'))
REQUESTS = []
CHECKS = 0
PAYLOAD = b'owned authenticated attachment \xcf\x80\xf0\x9f\xa6\x80\n'


def check(name, passed, detail=None):
    global CHECKS
    assert passed, (name, detail)
    CHECKS += 1
    print('PASS', name, flush=True)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        auth = 'download-auth=owned' in (self.headers.get('Cookie') or '')
        REQUESTS.append((self.path, self.headers.get('Sec-Fetch-Mode'), auth))
        protected = self.path in ('/attachment.txt', '/denied.txt', '/inline.txt')
        allowed = auth and self.path != '/denied.txt'
        attachment = self.path == '/attachment.txt' and allowed
        body = PAYLOAD if attachment else b'<title>Download fixture</title><h1>Original owned page</h1>'
        self.send_response(200 if not protected or allowed else 403)
        self.send_header('Content-Type', 'application/octet-stream' if attachment else 'text/html')
        if attachment:
            self.send_header('Content-Disposition', 'attachment; filename="attachment.txt"')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def run_case(root, server, mode, transport):
    label = mode + '-' + transport
    home = root / label
    env = dict(os.environ, BLADE_HOME=str(home), BLADE_LANE='agent', BLADE_INCOGNITO=mode,
               BLADE_TRANSPORT=transport, BLADE_NO_WARMING='1', BLADE_NO_UPDATE_CHECK='1')
    for key in ('BLADE_CHROME_FLAGS', 'BLADE_PROFILE_DIR', 'BLADE_FRESH', 'BLADE_PROXY',
                'DISPLAY', 'WAYLAND_DISPLAY', 'XAUTHORITY'):
        env.pop(key, None)
    origin = f'http://127.0.0.1:{server.server_port}/'
    other = f'http://localhost:{server.server_port}'
    calls = [
        ('act', {'action':'navigate', 'url':origin}),
        ('state', {'op':'set-cookie', 'name':'download-auth', 'value':'owned', 'url':other}),
        ('state', {'op':'tabs'}),
        ('act', {'action':'download', 'url':other + '/attachment.txt', 'timeout':5}),
        ('act', {'action':'download', 'url':other + '/denied.txt', 'timeout':2}),
        ('act', {'action':'download', 'url':other + '/inline.txt', 'timeout':2}),
        ('act', {'action':'eval', 'js':'({url:location.href,title:document.title,body:document.body.innerText})'}),
        ('state', {'op':'tabs'}),
    ]
    requests = [{'jsonrpc':'2.0', 'id':1, 'method':'initialize', 'params':{}}]
    requests += [{'jsonrpc':'2.0', 'id':i+2, 'method':'tools/call', 'params':{'name':name, 'arguments':args}} for i, (name, args) in enumerate(calls)]
    before = len(REQUESTS)
    result = subprocess.run([BINARY, 'mcp'], env=env, capture_output=True, text=True,
                            input=''.join(json.dumps(r)+'\n' for r in requests), timeout=120)
    replies = {r['id']:r for r in map(json.loads, result.stdout.splitlines()) if 'id' in r}
    check(label + ' JSON-RPC completes and owns browser teardown', result.returncode == 0 and len(replies) == len(requests), result.stderr)
    def text(identity):
        return '\n'.join(c.get('text','') for c in replies[identity]['result']['content'])
    for identity in (2, 3, 4, 5, 8, 9):
        check(label + ' successful call ' + str(identity), 'error' not in replies[identity] and not replies[identity]['result'].get('isError'), replies[identity])
    check(label + ' download reports real completion', 'download complete:' in text(5).lower(), text(5))
    files = list((home / 'downloads').glob('*'))
    check(label + ' exactly the requested attachment exists', len(files) == 1 and files[0].name == 'attachment.txt', files)
    check(label + ' attachment bytes are exact', files[0].read_bytes() == PAYLOAD)
    for identity in (6, 7):
        check(label + ' rejected/inline URL reports failure ' + str(identity), replies[identity]['result'].get('isError') is True, replies[identity])
        check(label + ' rejected/inline URL never claims success ' + str(identity), 'no completed download' in text(identity), text(identity))
    value = json.loads(next(l[len('result: '):] for l in text(8).splitlines() if l.startswith('result: ')))
    check(label + ' original URL and DOM survive every fallback', value == {'url':origin, 'title':'Download fixture', 'body':'Original owned page'}, value)
    check(label + ' auxiliary tabs close on success and failure', text(4) == text(9), (text(4), text(9)))
    observed = REQUESTS[before:]
    for path in ('/attachment.txt', '/denied.txt', '/inline.txt'):
        navigation = [r for r in observed if r[0] == path and r[1] == 'navigate']
        check(label + ' exactly one authenticated fallback navigation ' + path, len(navigation) == 1 and navigation[0][2], observed)
        fetches = [r for r in observed if r[0] == path and r[1] == 'cors']
        check(label + ' fixture really blocks cross-origin fetch ' + path, len(fetches) == 1 and not fetches[0][2], observed)


with tempfile.TemporaryDirectory(prefix='blade-download-') as directory:
    root = Path(directory)
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        for transport in ('ws', 'pipe') if os.name == 'posix' else ('ws',):
            for mode in ('0', '1'):
                run_case(root, server, mode, transport)
    finally:
        server.shutdown()
        server.server_close()
print(f'DOWNLOAD REGRESSION {CHECKS}/{CHECKS}', flush=True)
