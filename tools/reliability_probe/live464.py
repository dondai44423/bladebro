#!/usr/bin/env python3
"""Agent-facing parity and mutation-honesty checks, with owned fixtures only.

Use --baseline to retain every failed check instead of stopping on failure.
BLADEBRO chooses the actual binary under test; POLISH_OUT retains receipts.
"""
import http.server
import json
import os
import queue
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

BINARY = os.environ.get('BLADEBRO', str(Path(__file__).resolve().parents[2] / 'target/release/bladebro'))
OUT = Path(os.environ.get('POLISH_OUT', 'work/release464/live.json'))
OUT.parent.mkdir(parents=True, exist_ok=True)
checks = []
receipts = []


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith('/slow-network'):
            time.sleep(1.2)
        if self.path.startswith('/request'):
            self.send_response(204)
            self.end_headers()
            return
        body = """<!doctype html><meta charset="utf-8"><title>Live464 fixture</title>
        <main><p>An <a href="/application">application</a> for browsing, <strong>very</strong> useful.</p>
        <a id="blank" target="_blank" href="/spawn">Blank</a>
        <button id="effect" onclick="window.clicks++;document.title='Clicked '+window.clicks">Effect</button>
        <input id="name" value="abc"><div id="marker">VISIBLE_MARKER</div>
        <section><button onclick="window.deletes++">Delete</button></section>
        <section><button onclick="window.deletes++">Delete</button></section>
        <ul><li><h2><a href="https://www.google.com/goto?url=OPAQUE">First result</a></h2><p>Visible one<span style="display:none">HIDDEN_POISON</span></p></li>
        <li><h2><a href="https://en.wikipedia.org/wiki/URL">Second result</a></h2><p>Visible two</p></li>
        <li><h2><a href="https://example.com/three">Third result</a></h2><p>Visible three</p></li></ul>
        <article id="own-id">Self value</article></main>
        <script>window.clicks=0;window.deletes=0;fetch('/request'+location.pathname);</script>"""
        if self.path.startswith('/pages/'):
            n=int(self.path.split('/')[-1].split('?')[0])
            body=body.replace('First result', 'First result page '+str(n)).replace('Second result','Second result page '+str(n)).replace('Third result','Third result page '+str(n))
            if n<3: body+='<a href="/pages/'+str(n+1)+'">Next</a>'
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
                assert not error and 'Live464 fixture' in text, text

            try:
                fresh()
                evaluate("document.body.insertAdjacentHTML('beforeend','<input id=second value=def>')")
                call('see',{})
                text,error=call('act',dict(action='type',label='username',text='replacement'))
                check('G3 unlabelled field ambiguity refuses without writing',error and evaluate("Array.from(document.querySelectorAll('input')).map(e=>e.value).join('|')")=='abc|def')
                evaluate("document.querySelector('#second').remove()")
                call('see',{})
                evaluate("document.querySelector('#name').addEventListener('input',e=>window.aliasTrusted=e.isTrusted)")
                text,error=call('act',dict(action='type',label='username',text='single'))
                check('G3 single unlabelled field fallback remains trusted',not error and evaluate("document.querySelector('#name').value==='single' && window.aliasTrusted") is True)
                text,error=call('act',dict(action='type',label='username',role='link',text='wrong'))
                check('G3 role filter cannot fall back to an unrelated field',error and evaluate("document.querySelector('#name').value")=='single')
                fresh()
                text, error = call('act', dict(action='click', selector='#effect'))
                check('A before popup click trusted effect', not error and evaluate('window.clicks') == 1)
                text, error = call('act', dict(action='click', selector='#blank'))
                check('G1 popup outcome appears exactly once', not error and text.count('new tab opened') == 1)
                text, error = call('act', dict(action='click', selector='#effect'))
                check('A origin mouse works while popup stays open', not error and evaluate('window.clicks') == 2)
                text,error=call('act',dict(action='hover',selector='#effect'))
                check('A hover works with popup still open',not error)
                text,error=call('act',dict(action='scroll',dy=100))
                check('A scroll remains bounded with popup open',not error and receipts[-1]['seconds']<10)
                # Use fresh owned tab for remaining baseline checks even if origin wedged.
                text, error = call('state', dict(op='open-tab', url=origin+'/?fresh'))
                fresh()
                text, error = call('see', dict(mode='content'))
                check('C markdown preserves inline word boundaries', ']('+origin+'/application) for' in text and '**very** useful' in text)
                text, error = call('see', dict(extract='auto', format='json'))
                check('D auto extract omits hidden descendants', not error and 'HIDDEN_POISON' not in text)
                value=json.loads(text) if not error else {}
                if isinstance(value.get('data'),dict):value=value['data']
                check('E opaque redirect is disclosed without ordinary URL false flags',not error and value.get('redirects_unresolved')==1 and not next((i for i in value.get('items',[]) if i.get('url')=='https://en.wikipedia.org/wiki/URL'),{}).get('url_wrapped'))
                for tool,args in [('act',dict(action='click',text='MISSING')),('act',dict(action='batch',steps=[dict(action='click',text='MISSING')])),('run',dict(steps=[dict(action='click',text='MISSING')]))]:
                    text,error=call(tool,args)
                    check('F exactly one error context '+tool+'/'+args.get('action','run'), text.count('--- current page') == 1)
                text,error=call('act',dict(action='press',selector='#name',key='Meta+a' if sys.platform=='darwin' else 'Control+a'))
                check('G2 targeted press focuses before chord', not error and evaluate("document.activeElement.id==='name' && document.querySelector('#name').selectionEnd===3") is True)
                text,error=call('act',dict(action='click',text='Delete'))
                check('G3 ambiguity refuses before mutation', error and '2' in text and evaluate('window.deletes') == 0)
                evaluate('window.deletes=0')
                text,error=call('act',dict(action='click',text='Delete',nth=2))
                check('G3 explicit nth clicks once', not error and evaluate('window.deletes') == 1)
                call('act',dict(action='navigate',url=origin+'/second'))
                text,error=call('see',dict(logs='network'))
                check('G4 network logs scoped to navigation', re.search(r'/request/\s', text) is None and '/request/second' in text)
                text,error=call('act',dict(action='wait',condition='element',text='VISIBLE_MARKER',timeout=0))
                check('G5 element wait covers visible ordinary text', not error)
                text,error=call('act',dict(action='navigate',url=origin+'/?slim',slim=True))
                check('G6 slim navigation verdict only', not error and 'actionable' not in text and len(text)<300)
                text,error=call('see',dict(extract='json',template={'rows':{'container':'article','fields':{'id':'@id'}}},format='json'))
                check('G7 template own attribute', not error and 'own-id' in text)
                text,error=call('run',dict(steps=[dict(action='if',condition='text',text='MISSING',timeout=5)]))
                check('G9 if false wording does not invent elapsed timeout', 'skipped (timeout 5s)' not in text)
                text,error=call('run',dict(slim=True,steps=[dict(action='navigate',url=origin+'/?run-slim'),dict(action='see',extract='links',format='json')]))
                check('J1 run slim retains reads and omits models', not error and 'actionable' not in text and 'application' in text)
                text,error=call('act',dict(action='batch',slim=True,steps=[dict(action='navigate',url=origin+'/?batch-slim'),dict(action='see',extract='links',format='json')]))
                check('J1 batch slim retains reads and omits models', not error and 'actionable' not in text and 'application' in text)
                text,error=call('act',dict(action='wait',selector='#marker',timeout=0))
                check('J9 selector wait', not error and 'VISIBLE_MARKER' not in text)
                text,error=call('act',dict(action='wait',selector='#ABSENT',timeout=0))
                check('J9 missing selector times out instead of silently settling', error and 'timeout' in text)
                text,error=call('act',dict(action='wait',selector='[',timeout=0))
                check('J9 malformed selector is an actionable error', error and 'selector' in text)
                text,error=call('act',dict(action='wait',selector='#marker',text='MIXED',timeout=0))
                check('J9 conflicting wait fields refused', error)
                text,error=call('act',dict(action='wait',condition='typo',timeout=0))
                check('J9 unknown wait condition refused', error and 'unknown' in text)
                text,error=call('see',dict(logs='network',clear=True))
                text,error=call('see',dict(logs='network'))
                check('J6 logs clear drains records', not error and '/request' not in text)
                text,error=call('see',dict(extract='auto',preview=True,format='json'))
                check('J11 auto schema preview omits rows', not error and 'fields' in text and 'First result' not in text)
                # Artifact envelopes are JSON; default data is a character chunk,
                # while opt-in item pages contain independently usable row values.
                payload=[{'i':i,'text':'नेपाल '+('x'*40)} for i in range(250)]
                text,error=call('act',dict(action='eval',js=json.dumps(payload,ensure_ascii=False),format='json'))
                envelope=json.loads(text) if not error else {}
                check('B oversized JSON offloads with start offset zero', 'artifact' in envelope and envelope.get('next_offset')==0)
                if 'artifact' in envelope:
                    path=envelope['artifact'];offset=0;chunks=[]
                    while offset is not None:
                        text,error=call('see',dict(artifact=path,format='json',offset=offset,limit=4000))
                        page=json.loads(text);chunks.append(page['data']);offset=page['next_offset']
                    check('B char pages reconstruct complete Unicode JSON', json.loads(''.join(chunks))==payload)
                    rows=[];offset=0
                    while offset is not None:
                        text,error=call('see',dict(artifact=path,format='json',items=True,offset=offset,limit=99))
                        page=json.loads(text) if not error else {}
                        rows.extend(page.get('items',[]));offset=page.get('next_offset')
                    check('J7 item pages reconstruct complete Unicode rows',rows==payload)
                text,error=call('see',dict(items=True))
                check('J7 item mode requires artifact',error)
                text,error=call('act',dict(action='download',body=True,url=origin+'/body'))
                body=json.loads(text) if not error else {}
                saved=Path(body.get('path','/missing'))
                check('J10 explicit body fetch saves HTML bytes', not error and body.get('status')==200 and saved.is_file() and b'Live464 fixture' in saved.read_bytes())
                text,error=call('act',dict(action='download',body=True,url='file:///etc/passwd'))
                check('J10 body fetch refuses non-HTTP URL',error)
                text,error=call('act',dict(action='navigate',url=origin+'/pages/1'))
                text,error=call('act',dict(action='paginate',next='text=Next',max=5,extract='auto'))
                pages=json.loads(text) if not error else {}
                check('J4 paginate reads all three distinct pages',not error and pages.get('count')==3 and all('page '+str(i+1) in json.dumps(row) for i,row in enumerate(pages.get('pages',[]))))
                check('J4 paginate discloses completion when Next disappears',pages.get('complete') is True)
                call('act',dict(action='navigate',url=origin+'/pages/1'))
                text,error=call('act',dict(action='paginate',next='text=Next',max=1,extract='auto'))
                limited=json.loads(text) if not error else {}
                check('J4 max cap is partial and never clicks ahead',not error and limited.get('complete') is False and limited.get('count')==1 and evaluate('location.pathname')=='/pages/1')
                fresh()
                text,error=call('see',dict(changes=True,format='json'))
                evaluate("document.body.insertAdjacentHTML('beforeend','<button id=\"delta\">Delta button</button>')")
                text,error=call('see',dict(changes=True,format='json'))
                delta=json.loads(text) if not error else {}
                check('J12 JSON delta identifies new ref',any(e.get('name')=='Delta button' for e in delta.get('added',[])))
                text,error=call('see',dict(changes=True,format='json'))
                delta=json.loads(text) if not error else {}
                check('J12 unchanged delta is empty',not error and not delta.get('added') and not delta.get('removed') and not delta.get('changed'))
                text,error=call('state',dict(op='block',classes='images'))
                evaluate("new Promise(r=>{let i=new Image;i.onload=i.onerror=()=>r(true);i.src='/image';document.body.append(i)})")
                text,error=call('state',dict(op='block'))
                check('J13 resource stats count actual blocked request',not error and re.search(r'[1-9]\d* requests blocked',text) is not None)
                check('J13 no fabricated byte or time savings', 'unmeasured' in text)
                call('state',dict(op='block',clear=True))
                text,error=call('act',dict(action='wait',condition='network',timeout=1))
                check('J6 network idle requires a quiet window',not error and receipts[-1]['seconds']>=.28)
                evaluate("void fetch('/slow-network')")
                text,error=call('act',dict(action='wait',condition='network',timeout=0))
                check('J6 busy network wait refuses false success',error)
                text,error=call('act',dict(action='wait',condition='network',timeout=4))
                check('J6 network wait recovers when request finishes',not error)
                fresh()
                # Focus a new input and prove type still uses native trusted events.
                evaluate("document.querySelector('#name').addEventListener('input',e=>window.trusted=e.isTrusted)")
                text,error=call('act',dict(action='type',selector='#name',text='Unicode नेपाल',slim=True))
                check('J8 type focuses and verifies trusted Unicode',not error and evaluate("document.querySelector('#name').value==='Unicode नेपाल' && window.trusted") is True)
                text,error=call('act',dict(action='press',selector='#marker',key='Enter'))
                check('G2 unfocusable target refuses keys',error and ('no keys' in text or 'not actionable' in text))
                text,error=call('see',dict(extract='json',format='json',template={'bad':{'container':'article','fields':{'bad':'['}}}))
                check('G7 invalid template CSS error has no JS stack',error and 'extract failed:' in text and 'at visit' not in text and '<anonymous>' not in text)
                evaluate("document.querySelector('#marker').style.display='none'")
                text,error=call('act',dict(action='wait',condition='element',text='VISIBLE_MARKER',timeout=0))
                check('G5 hidden text never satisfies element wait',error)
                text,error=call('run',dict(slim=True,steps=[dict(action='if',condition='text',text='VISIBLE',then=[dict(action='navigate',url=origin+'/?nested')]),dict(action='click',text='MISSING')]))
                check('J1 slim retains actionable failure state', '--- current page state ---' in text)
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
print(f'LIVE464 {len(checks)-failed}/{len(checks)}; failures {failed}', flush=True)
sys.exit(0 if '--baseline' in sys.argv else bool(failed))
