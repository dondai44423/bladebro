#!/usr/bin/env python3
"""Root selector waits and explicit toggle fill values; owned MCP/live fixtures.

BLADEBRO selects a binary, POLISH_OUT retains JSON-RPC + live readbacks.
--baseline records failures without aborting. Run under capped.
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
OUT = Path(os.environ.get('POLISH_OUT', 'work/release465/qol.json'))
OUT.parent.mkdir(parents=True, exist_ok=True)
checks, receipts = [], []

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'''<!doctype html><html data-ready="yes"><head><title>Live464 fixture</title></head>
        <body><main>Ready fixture</main><label><input id="consent" type="checkbox">Consent</label>
        <label><input id="choice" type="radio" name="pick">Choice</label>
        <button id="switch" role="switch" aria-checked="false" onclick="this.setAttribute('aria-checked',this.getAttribute('aria-checked')==='true'?'false':'true');window.toggles++">Power</button>
        <input id="name" aria-label="Name" value="initial">
        <iframe id="frame" srcdoc="&lt;html data-frame-ready=yes&gt;&lt;body&gt;FRAME_MARKER&lt;/body&gt;&lt;/html&gt;"></iframe>
        <div id="shadow"></div><script>
        window.toggles=0;document.querySelector('#consent').onchange=()=>window.toggles++;
        document.querySelector('#choice').onchange=()=>window.toggles++;
        document.querySelector('#shadow').attachShadow({mode:'open'}).innerHTML='<span class="shadow-ready">SHADOW_MARKER</span>';
        </script></body></html>'''
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
    if not passed:
        print('FAILURE READBACK '+json.dumps(receipts[-2:],ensure_ascii=True),flush=True)

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
origin = 'http://127.0.0.1:' + str(server.server_port)
try:
    with tempfile.TemporaryDirectory(prefix='blade-qol465-') as home:
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
                assert not error and 'Live464 fixture' in text, text

            try:
                fresh()
                check('fixture roots exist in live DOM', evaluate("document.documentElement.dataset.ready==='yes' && document.querySelector('#frame').contentDocument.documentElement.dataset.frameReady==='yes'") is True)
                for selector in ('html[data-ready=yes]', 'html[data-frame-ready=yes]', 'body', '.shadow-ready'):
                    text,error=call('act',dict(action='wait',condition='selector',text=selector,timeout=0))
                    check('visible selector '+selector,not error)
                evaluate("document.querySelector('#frame').hidden=true")
                text,error=call('act',dict(action='wait',condition='selector',text='html[data-frame-ready=yes]',timeout=0))
                check('hidden frame root cannot satisfy wait',error)
                evaluate("document.querySelector('#frame').hidden=false")
                text,error=call('act',dict(action='wait',condition='selector',text='[',timeout=0))
                check('invalid CSS reports error',error and 'selector wait failed' in text)
                for selector in ('#consent','#choice','#switch'):
                    for value in ({'text':'false'}, {'option':'false'}, {'text':'','check':False}):
                        fresh()
                        text,error=call('act',dict(action='fill',fields=[dict(selector=selector,**value)],slim=True))
                        state=evaluate("({toggles:window.toggles,checked:document.querySelector('#consent').checked,radio:document.querySelector('#choice').checked,switch:document.querySelector('#switch').getAttribute('aria-checked')})")
                        check('toggle refuses ignored value '+selector+' '+json.dumps(value),error and 'check' in text and state==dict(toggles=0,checked=False,radio=False,switch='false'))
                fresh()
                for selector in ('#consent','#switch'):
                    for desired in (True,True,False):
                        before=evaluate('window.toggles')
                        was=evaluate("document.querySelector('"+selector+"')."+('checked' if selector=='#consent' else "getAttribute('aria-checked')==='true'"))
                        text,error=call('act',dict(action='fill',fields=[dict(selector=selector,check=desired)],slim=True))
                        checked=evaluate("document.querySelector('"+selector+"')."+('checked' if selector=='#consent' else "getAttribute('aria-checked')==='true'"))
                        after=evaluate('window.toggles')
                        check('explicit check readback '+selector+' '+str(desired)+' '+str(before),not error and checked==desired and after-before==int(was!=desired))
                fresh()
                for tool,args in [('act',dict(action='batch',steps=[dict(action='fill',fields=[dict(selector='#consent',text='false')])])),('run',dict(steps=[dict(action='fill',fields=[dict(selector='#consent',option='false')])]))]:
                    fresh()
                    text,error=call(tool,args)
                    check('nested fill refuses without toggling '+tool,'require check=true/false' in text and evaluate("!document.querySelector('#consent').checked && window.toggles===0") is True)
                fresh()
                text,error=call('act',dict(action='fill',fields=[dict(selector='#choice',check=True)],slim=True))
                check('explicit radio selection remains supported',not error and evaluate("document.querySelector('#choice').checked && window.toggles===1") is True)
                text,error=call('act',dict(action='fill',fields=[dict(selector='#choice',check=False)],slim=True))
                check('checked radio refuses impossible uncheck',error and 'cannot be unchecked' in text and evaluate("document.querySelector('#choice').checked && window.toggles===1") is True)
                fresh()
                text,error=call('act',dict(action='fill',fields=[dict(selector='#consent')],slim=True))
                check('field-only toggle stays supported',not error and evaluate("document.querySelector('#consent').checked && window.toggles===1") is True)
                text,error=call('act',dict(action='fill',fields=[dict(selector='#name',text='')],slim=True))
                check('explicit empty text still clears textbox',not error and evaluate("document.querySelector('#name').value")== '')
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
print(f'QOL465 {len(checks)-failed}/{len(checks)}; failures {failed}', flush=True)
sys.exit(0 if '--baseline' in sys.argv else bool(failed))
