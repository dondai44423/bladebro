#!/usr/bin/env python3
"""Agent-facing parity and mutation-honesty checks, with owned fixtures only.

Use --baseline to retain every failed check instead of stopping on failure.
BLADEBRO chooses the actual binary under test; POLISH_OUT retains receipts.
"""
import http.server
import json
import os
import queue
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

BINARY = os.environ.get('BLADEBRO', str(Path(__file__).resolve().parents[2] / 'target/release/bladebro'))
OUT = Path(os.environ.get('POLISH_OUT', 'work/release463/polish.json'))
OUT.parent.mkdir(parents=True, exist_ok=True)
checks = []
receipts = []


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/slow':
            time.sleep(.16)
            body = b'late'
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        body = '''<!doctype html><meta charset="utf-8"><title>Polish fixture</title>
        <form onsubmit="event.preventDefault();window.submits++">
        <label>Name<input id="name" value="preserve"></label><button>Submit</button></form>
        <label>Consent<input type="checkbox" id="consent"></label>
        <label>Choice<input type="radio" id="choice" checked></label>
        <button id="effect" style="position:absolute;left:400px;top:300px;width:100px;height:40px"
        onclick="window.clicks++;window.lastTrusted=event.isTrusted">Effect</button>
        <button id="long">''' + 'readable ' * 100 + '''</button>
        <script>window.clicks=0;window.submits=0;window.replays=0;window.lastTrusted=false;</script>'''
        if self.path.startswith('/late'):
            body += "<script>fetch('/slow').then(r=>r.text()).then(()=>{let b=document.createElement('button');b.textContent='Late mount';document.body.append(b)})</script>"
        body = body.encode()
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


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
origin = 'http://127.0.0.1:' + str(server.server_port)
try:
    with tempfile.TemporaryDirectory(prefix='blade-polish-') as home:
        env = dict(os.environ, BLADE_HOME=home, BLADE_NO_WARMING='1', BLADE_NO_UPDATE_CHECK='1', BLADE_LANE='agent')
        for key in ('BLADE_PROFILE_DIR', 'BLADE_TRANSPORT', 'BLADE_PROXY', 'BLADE_FRESH'):
            env.pop(key, None)
        with OUT.with_suffix('.stderr.log').open('w') as stderr:
            proc = subprocess.Popen([BINARY, 'mcp'], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, encoding='utf-8')
            identity = 0
            replies = queue.Queue()

            def read_replies():
                for line in proc.stdout:
                    replies.put(line)
                replies.put('')

            threading.Thread(target=read_replies, daemon=True).start()

            def call(tool, args):
                global identity
                identity += 1
                request = dict(jsonrpc='2.0', id=identity, method='tools/call', params=dict(name=tool, arguments=args))
                start = time.monotonic()
                proc.stdin.write(json.dumps(request) + '\n')
                proc.stdin.flush()
                response = json.loads(replies.get(timeout=60))
                assert response['id'] == identity, response
                result = response.get('result', {})
                text = '\n'.join(c.get('text', '') for c in result.get('content', []))
                receipts.append(dict(request=request, response=response, seconds=time.monotonic()-start))
                return text, result.get('isError', False) or 'error' in response

            def evaluate(js):
                text, error = call('act', dict(action='eval', js=js, format='json'))
                assert not error, text
                return json.loads(text)

            def fresh():
                text, error = call('act', dict(action='navigate', url=origin + '/?n=' + str(identity)))
                assert not error and 'Polish fixture' in text, text

            try:
                fresh()
                text, error = call('act', dict(action='eval', js="(()=>{window.replays++;throw new SyntaxError('runtime failure')})()"))
                check('runtime SyntaxError is surfaced', error and 'runtime failure' in text)
                check('runtime SyntaxError never replays side effects', evaluate('window.replays') == 1)
                fresh()
                text, error = call('act', dict(action='eval', js='var n=++window.replays; n'))
                check('statement fallback executes side effects exactly once', not error and evaluate('window.replays') == 1)
                fresh()
                evaluate('Error.stackTraceLimit=0')
                text, error = call('act', dict(action='eval', js="(()=>{window.replays++;JSON.parse('{invalid')})()"))
                check('runtime parser error with no stack never replays', error and evaluate('window.replays') == 1)
                fresh()
                text, error = call('run', dict(steps=[dict(action='type', selector='#name', text='query', press='Enter')]))
                check('run type+press submits exactly once', not error and evaluate('window.submits') == 1)
                fresh()
                text, error = call('run', dict(steps=[dict(action='click', x=450, y=320)]))
                check('run coordinate click reaches trusted handler once', not error and evaluate('window.clicks===1 && window.lastTrusted') is True)
                text, error = call('run', dict(steps=[dict(action='dialog', expect='prompt', prompt_text='answer'), dict(action='eval', js="prompt('Question')")]))
                check('run dialog expectation reaches prompt', not error and 'answer' in text and 'run stopped' not in text)
                text, error = call('run', dict(steps=[dict(action='read', selector='#long')]))
                check('run read never silently truncates at 200 chars', not error and (text.count('readable') >= 100 or 'truncat' in text))
                fresh()
                # eval deliberately does not recapture: emulate live property changes
                # between observations, without changing the element's identity.
                evaluate("document.querySelector('#consent').checked=true")
                text, error = call('act', dict(action='fill', fields=[dict(selector='#consent', check=False)]))
                check('fill check uses live state instead of cached model', not error and evaluate("document.querySelector('#consent').checked") is False)
                text, error = call('act', dict(action='fill', fields=[dict(selector='#choice', check=False)]))
                check('fill refuses impossible radio uncheck honestly', error and evaluate("document.querySelector('#choice').checked") is True)
                fresh()
                text, error = call('act', dict(action='type', selector='#name', text=['invalid']))
                check('invalid type text rejected before clearing the field', error and evaluate("document.querySelector('#name').value") == 'preserve')
                fresh()
                text, error = call('act', dict(action='fill', fields=[dict(selector='#name', text='changed'), dict(selector='#consent', check='invalid')]))
                check('malformed later fill rejected before all mutations', error and evaluate("document.querySelector('#name').value") == 'preserve')
                text, error = call('act', dict(action='fill', fields=[dict(selector='#name')]))
                check('missing fill text never clears a field implicitly', error and evaluate("document.querySelector('#name').value") == 'preserve')
                fresh()
                text, error = call('act', dict(action='eval', js='++window.replays', format='typo'))
                check('invalid eval format rejected before executing JS', error and evaluate('window.replays') == 0)
                text, error = call('see', dict(mode='typo'))
                check('invalid see mode gives recovery guidance', error and 'mode' in text and 'content' in text)
                text, error = call('run', dict(steps=[dict(action='clik', selector='#effect')]))
                check('run typo suggests the valid action with page state', 'click' in text and 'clik' in text and '--- current page state ---' in text)
                evaluate('document.querySelector("#name").id=\'quote"key\'')
                text, error = call('see', dict(extract='forms', format='json'))
                forms = json.loads(text) if not error else []
                check('form extraction handles quotes in control IDs', bool(forms) and forms[0]['fields'][0]['label'] == 'Name')
                fresh()
                evaluate("(()=>{let h=document.createElement('div');document.body.append(h);h.attachShadow({mode:'open'}).innerHTML='<a href=\"/shadow\">Shadow link</a><form><label>Shadow name<input></label></form>';let f=document.createElement('iframe');f.srcdoc='<a href=\"/frame\">Frame link</a>';document.body.append(f);return new Promise(r=>f.onload=()=>r(true))})()")
                text, error = call('see', dict(extract='links', format='json'))
                links = json.loads(text) if not error else []
                check('link extraction reaches shadow and same-origin frame', {l['text'] for l in links} >= {'Shadow link', 'Frame link'})
                text, error = call('see', dict(extract='forms', format='json'))
                forms = json.loads(text) if not error else []
                check('form extraction reaches shadow labels', any(f['fields'] and f['fields'][0]['label'] == 'Shadow name' for f in forms))
                fresh()
                evaluate("(()=>{document.body.innerHTML='';for(let i=0;i<60;i++){let a=document.createElement('a');a.href='/item/'+i;a.textContent='Item '+i;document.body.append(a)}})()")
                text, error = call('see', dict(extract='links', format='json', limit=2))
                value = json.loads(text) if not error else None
                check('basic extraction honors limit and discloses omitted items', isinstance(value, dict) and len(value.get('data', [])) == 2 and '60' in value.get('note', ''))
                fresh()
                evaluate("(()=>{document.querySelectorAll=()=>{throw new Error('owned extraction failure')};Object.defineProperty(document,'childNodes',{get(){throw new Error('owned extraction failure')}})})()")
                text, error = call('see', dict(extract='links', format='json'))
                check('extraction JS failure never becomes an empty success', error and 'owned extraction failure' in text)
                fresh()
                for tool, args in [
                    ('act', dict(action='navigate', url=origin + '/late?direct')),
                    ('run', dict(steps=[dict(action='navigate', url=origin + '/late?run')])),
                    ('act', dict(action='batch', steps=[dict(action='navigate', url=origin + '/late?batch')])),
                ]:
                    text, error = call(tool, args)
                    check('navigation includes fetched late mount: ' + tool + '/' + args.get('action', 'run'), not error and 'Late mount' in text)
                for _ in range(3):
                    for tool, args in [
                        ('act', dict(action='navigate', url=origin + '/?direct=' + str(identity))),
                        ('run', dict(steps=[dict(action='navigate', url=origin + '/?run=' + str(identity))])),
                        ('act', dict(action='batch', steps=[dict(action='navigate', url=origin + '/?batch=' + str(identity))])),
                    ]:
                        text, error = call(tool, args)
                        check('navigation benchmark reaches fixture: ' + tool + '/' + args.get('action', 'run'), not error and 'Polish fixture' in text)
            finally:
                proc.stdin.close()
                try:
                    proc.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    proc.terminate()
                    try:
                        proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=10)
                proc.stdout.close()
finally:
    server.shutdown()
    server.server_close()
    OUT.write_text(json.dumps(dict(binary=BINARY, checks=checks, receipts=receipts), indent=2))
failed = sum(not c['passed'] for c in checks)
print(f'POLISH {len(checks)-failed}/{len(checks)}; failures {failed}', flush=True)
sys.exit(0 if '--baseline' in sys.argv else bool(failed))
