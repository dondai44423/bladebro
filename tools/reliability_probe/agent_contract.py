#!/usr/bin/env python3
"""Agent contract, preflight safety and sequence fidelity; owned MCP/live fixtures.

BLADEBRO selects a binary, POLISH_OUT retains JSON-RPC + live readbacks.
--baseline records failures without aborting. Run under capped.
"""
import http.server
import hashlib
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
OUT = Path(os.environ.get('POLISH_OUT', 'work/agent-reliability-20261104/contract.json'))
OUT.parent.mkdir(parents=True, exist_ok=True)
checks, receipts = [], []

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'''<!doctype html><html data-ready="yes"><head><title>Agent contract fixture</title></head>
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
        print('See retained JSON-RPC receipts for failure readback',flush=True)

# A malformed first call must return usage guidance even without Chrome.
# This exercises the actual stdio boundary rather than a mocked validator.
with tempfile.TemporaryDirectory(prefix='blade-contract-cold-') as cold_home:
    cold_env=dict(os.environ,BLADE_HOME=cold_home,CHROME_PATH=str(Path(cold_home)/'missing-browser'),BLADE_NO_WARMING='1',BLADE_NO_UPDATE_CHECK='1',BLADE_LANE='agent')
    for key in ('BLADE_PROFILE_DIR','BLADE_TRANSPORT','BLADE_PROXY','BLADE_FRESH'):
        cold_env.pop(key,None)
    requests=[dict(jsonrpc='2.0',id=i,method='tools/call',params=dict(name=tool,arguments=args)) for i,(tool,args) in enumerate([
        ('act',dict(action='type',selector='#name')),('vision',dict(marks=7)),('missing',{})],1)]
    start=time.monotonic()
    cold=subprocess.run([BINARY,'mcp'],input=''.join(json.dumps(r)+'\n' for r in requests),text=True,encoding='utf-8',capture_output=True,env=cold_env,timeout=30)
    responses=[json.loads(line) for line in cold.stdout.splitlines()]
    receipts.append(dict(cold=True,requests=requests,responses=responses,stderr=cold.stderr,seconds=time.monotonic()-start))
    for response,expected in zip(responses,['type requires text','marks must be a boolean','unknown tool']):
        check('cold invalid call needs no browser '+expected,response.get('result',{}).get('isError') and expected in json.dumps(response))
    check('cold MCP rejects all requests and exits cleanly',cold.returncode==0 and len(responses)==3)

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
origin = 'http://127.0.0.1:' + str(server.server_port)
try:
    with tempfile.TemporaryDirectory(prefix='blade-contract-') as home:
        env = dict(os.environ, BLADE_HOME=home, BLADE_NO_WARMING='1', BLADE_NO_UPDATE_CHECK='1', BLADE_LANE='agent')
        for key in ('BLADE_PROFILE_DIR', 'BLADE_TRANSPORT', 'BLADE_PROXY', 'BLADE_FRESH'):
            env.pop(key, None)
        if os.environ.get('CONTRACT_TRANSPORT'):
            env['BLADE_TRANSPORT']=os.environ['CONTRACT_TRANSPORT']
        with OUT.with_suffix('.stderr.log').open('w') as stderr:
            proc = subprocess.Popen([BINARY, 'mcp'], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, encoding='utf-8')
            identity = 0
            replies = queue.Queue()

            def read_replies():
                for line in proc.stdout:
                    replies.put(line)
                replies.put('')

            threading.Thread(target=read_replies, daemon=True).start()

            def rpc(method, params):
                global identity
                identity += 1
                request = dict(jsonrpc='2.0', id=identity, method=method, params=params)
                start = time.monotonic()
                proc.stdin.write(json.dumps(request) + '\n')
                proc.stdin.flush()
                response = json.loads(replies.get(timeout=60))
                assert response['id'] == identity, response
                receipts.append(dict(request=request, response=response, seconds=time.monotonic()-start))
                return response

            def call(tool, args):
                response = rpc('tools/call', dict(name=tool, arguments=args))
                result = response.get('result', {})
                return '\n'.join(c.get('text', '') for c in result.get('content', [])), bool(result.get('isError') or 'error' in response)

            def evaluate(js):
                text, error = call('act', dict(action='eval', js=js, format='json'))
                assert not error, text
                return json.loads(text)

            def fresh():
                text, error = call('act', dict(action='navigate', url=origin + '/?n=' + str(identity)))
                assert not error and 'Agent contract fixture' in text, text

            try:
                definitions = {t['name']:t for t in rpc('tools/list', {})['result']['tools']}
                check('exactly five tools', set(definitions)=={'act','see','state','run','vision'})
                for tool, keys in [('act',['x','y','press','block']),('state',['path','secure','httpOnly','sameSite','slim','mode'])]:
                    for key in keys:
                        check('schema exposes '+tool+' '+key, key in definitions[tool]['inputSchema']['properties'])
                check('state compress is discoverable', 'compress' in definitions['state']['inputSchema']['properties']['op']['enum'])
                fresh()
                for bad in [dict(action='wait',condition='js'),dict(action='wait',text='Ready',timeout=1.5),dict(action='eval'),dict(action='select',selector='#name'),dict(action='extract',extract='json',template=[]),dict(action='extract',extract='links',limit='bad')]:
                    current=evaluate('location.href')
                    text,error=call('act',dict(bad,url=origin+'/?invalid='+str(identity)))
                    check('preflight before navigation '+json.dumps(bad), error and evaluate('location.href')==current)
                fresh()
                text,error=call('act',dict(action='type',selector='#name',text='must-not-apply',press='Control+'))
                check('malformed submit chord fails before typing',error and evaluate("document.querySelector('#name').value")=='initial')
                for tool in ('run','act'):
                    fresh()
                    args=dict(steps=[dict(action='eval',js='window.toggles++'),dict(action='type',selector='#name',text=7)])
                    if tool=='act': args['action']='batch'
                    text,error=call(tool,args)
                    check('malformed later step prevents all effects '+tool, evaluate('window.toggles')==0 and 'text' in text)
                fresh()
                text,error=call('run',dict(steps=[dict(action='if',condition='js',js='true',then=[dict(action='click',selector='#missing',optional=True),dict(action='eval',js='window.toggles++')])]))
                check('nested optional failure continues branch', 'continued' in text and evaluate('window.toggles')==1)
                fresh()
                text,error=call('run',dict(steps=[dict(action='batch',steps=[dict(action='click',selector='#missing')]),dict(action='eval',js='window.toggles++')]))
                check('failed batch stops enclosing run', 'run stopped' in text and evaluate('window.toggles')==0)
                for action in ('wait','if','while'):
                    fresh()
                    destination=origin+'/?step='+str(identity)
                    text,error=call('run',dict(steps=[dict(action=action,condition='js',js='false' if action=='while' else 'true',timeout=0,url=destination,max=1,steps=[])]))
                    check('run '+action+' honors url before condition', not error and 'run stopped' not in text and evaluate('location.href')==destination)
                text,error=call('run',dict(steps=[dict(action='eval',js="'PAYLOAD_'+('z'.repeat(1700))+'_END'",format='json')]))
                check('run eval retains full returned data', not error and 'z'*1700 in text and '_END' in text and 'truncated' not in text)
                call('state',dict(op='set-ls',name='full',value='STATE_'+('q'*2500)+'_END'))
                text,error=call('run',dict(steps=[dict(action='state',op='ls')]))
                check('run state retains full read', not error and 'q'*2500 in text and '_END' in text)
                evaluate("(()=>{let b=document.createElement('button');b.id='read-value';b.textContent='READ_'+('r'.repeat(200))+'_END';document.body.append(b)})()")
                text,error=call('act',dict(action='batch',slim=True,steps=[dict(action='state',op='ls'),dict(action='read',selector='#read-value')]))
                check('batch retains state and element reads',not error and text.count('q'*2500)==1 and 'r'*200+'_END' in text)
                call('state',dict(op='set-cookie',name='long-cookie',value='c'*150))
                text,error=call('state',dict(op='cookies'))
                check('cookie values are full fidelity',not error and 'c'*150 in text)
                call('state',dict(op='set-ls',name='large',value='L'*15000))
                text,error=call('state',dict(op='ls'))
                artifact=text.split('full payload: ')[-1].split(' — ')[0]
                try:
                    chunk,chunk_error=call('see',dict(artifact=artifact,format='json',items=True))
                    rows=json.loads(chunk).get('items',[])
                    intact=any(row.get('key')=='large' and row.get('value')=='L'*15000 for row in rows)
                except (ValueError,AttributeError): intact=False;chunk_error=True
                check('large state reads offload usable full JSON',not error and not chunk_error and intact)
                text,error=call('act',dict(action='eval',js="(()=>{window.open('about:blank');return {popup:true}})()",format='json'))
                try:
                    popup=json.loads(text)
                    parseable=popup.get('data',{}).get('popup') is True and 'popup tab' in popup.get('note','')
                except (ValueError,AttributeError): parseable=False
                check('JSON popup result preserves value and tab notice',not error and parseable)
                call('act',dict(action='dialog',expect='prompt',prompt_text='answer'))
                text,error=call('act',dict(action='eval',js="prompt('contract question')",format='json'))
                try:
                    payload=json.loads(text)
                    valid=payload.get('data')=='answer' and 'dialogs handled' in payload.get('note','')
                except (ValueError,AttributeError): valid=False
                check('JSON stays parseable with dialog notes',not error and valid)
                evaluate("(()=>{let n=document.createElement('div');n.id='blade-marks';n.textContent='PRESERVE_SITE_NODE';document.body.append(n)})()")
                text,error=call('vision',dict(marks=True))
                check('vision marks preserve colliding site element',not error and evaluate("document.querySelector('#blade-marks')?.textContent")=='PRESERVE_SITE_NODE')
                text,error=call('vision',dict(marks='false'))
                check('vision refuses wrong marks type',error and 'marks' in text)
                text,error=call('act',dict(action='click',selector='#missing'))
                check('error contains one page state',error and text.count('--- current page state ---')==1)
                text,error=call('run',dict(steps=[dict(action='type',selector='#name',text='query',press='Enter')],slim=True))
                check('valid composed input still works',not error and 'run stopped' not in text and evaluate("document.querySelector('#name').value")=='query')
            finally:
                proc.stdin.close()
                try: proc.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    proc.terminate()
                    try: proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        proc.kill(); proc.wait(timeout=10)
                proc.stdout.close()
finally:
    server.shutdown()
    server.server_close()
    OUT.write_text(json.dumps(dict(binary=BINARY,sha256=hashlib.sha256(Path(BINARY).read_bytes()).hexdigest(),checks=checks,receipts=receipts),indent=2))
failed=sum(not c['passed'] for c in checks)
print(f"Agent contract: {len(checks)-failed}/{len(checks)} passed; {failed} failed",flush=True)
if failed and '--baseline' not in sys.argv: sys.exit(1)
