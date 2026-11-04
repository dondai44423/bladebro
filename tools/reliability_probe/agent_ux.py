#!/usr/bin/env python3
"""Live agent read/sequence contracts. Run under capped; --baseline records gaps."""
import http.server
import hashlib
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time

BINARY = os.environ.get('BLADEBRO', str(Path(__file__).resolve().parents[2] / 'target/release/bladebro'))
OUT = Path(os.environ.get('POLISH_OUT', 'work/agent-ux.json'))
OUT.parent.mkdir(parents=True, exist_ok=True)
checks, receipts, workflows = [], [], []

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = '''<!doctype html><meta charset=utf-8><title>Agent UX</title>
        <input id="name" aria-label="Name" value="initial">
        <textarea id="notes" aria-label="Notes">default text</textarea>
        <select id="country" aria-label="Country"><option value="np">Nepal</option><option value="jp" selected>Japan</option><optgroup label="Disabled" disabled><option value="xx">Unavailable</option></optgroup></select>
        <input id="consent" aria-label="Consent" type="checkbox" checked>
        <button id="switch" role="switch" aria-checked="mixed">Power</button>
        <button id="effect" onclick="window.effects++">Effect</button>
        <input id="large" aria-label="Large"><select id="many" aria-label="Many"></select>
        <input id="empty" aria-label="Empty" value="" readonly required>
        <fieldset disabled><input id="disabled" aria-label="Disabled" value="inherited disabled"></fieldset>
        <select id="labelled" aria-label="Labelled"><option value="first">First</option><option value="code" label="Shown label">Different inner text</option></select>
        <select id="multi" multiple aria-label="Multi"><option value="a" selected>A</option><option value="b" selected>B</option></select>
        <div id="shadow"></div><iframe srcdoc="&lt;input aria-label='Framed' value='frame live'&gt;"></iframe>
        <script>window.effects=0;nameField=document.querySelector('#name');nameField.value='live Unicode 日本';
        document.querySelector('#notes').value='live notes';document.querySelector('#consent').indeterminate=true;
        document.querySelector('#large').value='日'.repeat(9000)+'TAIL';
        document.querySelector('#shadow').attachShadow({mode:'open'}).innerHTML='<input aria-label="Shadow" value="shadow live">';
        for(let i=0;i<90;i++)document.querySelector('#many').add(new Option('Choice '+i,''+i));document.querySelector('#many').selectedIndex=89;</script>'''.encode()
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

def payload(text):
    try:
        value = json.loads(text)
        return value['data'] if isinstance(value, dict) and set(value)=={'data','note'} else value
    except (ValueError, TypeError):
        return {}

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
try:
    with tempfile.TemporaryDirectory(prefix='blade-agent-ux-') as home:
        env = dict(os.environ, BLADE_HOME=home, BLADE_NO_WARMING='1', BLADE_NO_UPDATE_CHECK='1', BLADE_LANE='agent')
        for key in ('BLADE_PROFILE_DIR', 'BLADE_TRANSPORT', 'BLADE_PROXY', 'BLADE_FRESH'):
            env.pop(key, None)
        if os.environ.get('CONTRACT_TRANSPORT'):
            env['BLADE_TRANSPORT'] = os.environ['CONTRACT_TRANSPORT']
        with OUT.with_suffix('.stderr.log').open('w') as stderr:
            proc = subprocess.Popen([BINARY, 'mcp'], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, encoding='utf-8')
            replies = queue.Queue()
            def reader():
                for line in proc.stdout:
                    replies.put(line)
                replies.put('')
            threading.Thread(target=reader, daemon=True).start()
            def rpc(method, params):
                request = dict(jsonrpc='2.0', id=len(receipts)+1, method=method, params=params)
                start = time.monotonic()
                proc.stdin.write(json.dumps(request)+'\n')
                proc.stdin.flush()
                response = json.loads(replies.get(timeout=90))
                assert response['id'] == request['id'], response
                receipts.append(dict(request=request, response=response, seconds=time.monotonic()-start))
                return response
            def call(tool, **args):
                response = rpc('tools/call', dict(name=tool, arguments=args))
                result = response.get('result', {})
                return '\n'.join(c.get('text', '') for c in result.get('content', [])), bool(result.get('isError') or 'error' in response)
            def read(**args):
                text, error = call('act', action='read', format='json', **args)
                check('read accepted '+str(args), not error)
                return payload(text)
            try:
                defs = rpc('tools/list', {})['result']['tools']
                check('five tools', {t['name'] for t in defs} == {'act','see','state','run','vision'})
                check('run JSON discoverable', 'format' in next(t for t in defs if t['name']=='run')['inputSchema']['properties'])
                text, error = call('act', action='navigate', url='http://127.0.0.1:'+str(server.server_port))
                assert not error and 'Agent UX' in text, text
                check('input live value', read(selector='#name').get('value') == 'live Unicode 日本')
                check('textarea property beats default text', read(label='Notes').get('value') == 'live notes')
                workflow_start=time.monotonic(); workflow_first=len(receipts)
                country = read(selector='#country')
                inspected=country
                if not country.get('options'):
                    text,error=call('act',action='eval',format='json',js="(()=>{const e=document.querySelector('#country');return {value:e.value,options:Array.from(e.options,o=>({label:o.label,value:o.value,selected:o.selected,disabled:o.disabled||(o.parentElement.tagName==='OPTGROUP'&&o.parentElement.disabled)}))}})()")
                    inspected=payload(text)
                expected_options=[dict(label='Nepal',value='np',selected=False,disabled=False),dict(label='Japan',value='jp',selected=True,disabled=False),dict(label='Unavailable',value='xx',selected=False,disabled=True)]
                workflow_correct=inspected.get('value')=='jp' and inspected.get('options')==expected_options
                workflows.append(dict(name='inspect select current value and available choices',calls=len(receipts)-workflow_first,seconds=time.monotonic()-workflow_start,correct=workflow_correct))
                check('select workflow exact readback', workflow_correct)
                check('select current value', country.get('value') == 'jp')
                options = country.get('options', [])
                check('select choices and selection in one call', len(options)==3 and options[1]==dict(label='Japan',value='jp',selected=True,disabled=False))
                check('optgroup disabled inherited', len(options)==3 and options[2].get('disabled') is True)
                consent = read(selector='#consent')
                check('native checked and indeterminate live', consent.get('checked') is True and consent.get('indeterminate') is True)
                check('ARIA mixed state retained', read(selector='#switch').get('aria-checked') == 'mixed')
                check('shadow field read', read(label='Shadow').get('value') == 'shadow live')
                check('frame field read', read(label='Framed').get('value') == 'frame live')
                check('multiple selected values', read(selector='#multi').get('values') == ['a','b'])
                empty=read(selector='#empty')
                check('empty value and readonly required state explicit', empty.get('value')=='' and empty.get('readonly') is True and empty.get('required') is True)
                check('fieldset disabled state inherited', read(selector='#disabled').get('disabled') is True)
                text,error=call('act',action='eval',js='document.activeElement.tagName',format='json')
                check('inspection does not move focus', not error and payload(text)=='BODY')
                labelled=read(selector='#labelled'); choices=labelled.get('options',[])
                check('native option visible label retained', len(choices)==2 and choices[1].get('label')=='Shown label')
                text,error=call('act',action='select',selector='#labelled',option=choices[1]['label'] if len(choices)==2 else 'Shown label')
                check('discovered visible label selects successfully', not error)
                check('selection by visible label exact readback', read(selector='#labelled').get('value')=='code')
                many = read(selector='#many')
                check('choice clipping explicit', len(many.get('options', []))==80 and many.get('options_total')==90 and many.get('options_truncated') is True)
                check('current selection survives choice clipping', many.get('value')=='89')
                large = read(selector='#large')
                artifact = large.get('artifact')
                check('oversized read offloaded', bool(artifact) and large.get('next_offset')==0)
                if artifact:
                    text,error = call('see', artifact=artifact, format='json', limit=40000)
                    chunk=payload(text)
                    full=payload(chunk.get('data',''))
                    check('artifact complete Unicode value', not error and full.get('value')=='日'*9000+'TAIL' and chunk.get('next_offset') is None)
                text,error=call('act',action='read',selector='#notes')
                check('text read uses live textarea', not error and 'live notes' in text and 'default text' not in text)
                text,error=call('run',format='json',slim=True,steps=[dict(action='read',selector='#name',format='json'),dict(action='eval',js='window.effects',format='json')])
                run=payload(text); results=run.get('results',[])
                check('structured run complete status', not error and run.get('status')=='completed' and len(results)==2)
                if len(results)==2:
                    check('step IDs and statuses', [r.get('step') for r in results]==[1,2] and all(r.get('status')=='ok' for r in results))
                    check('structured payloads retain read data', 'live Unicode' in results[0].get('output','') and '0' in results[1].get('output',''))
                text,error=call('run',format='json',slim=True,steps=[dict(action='click',selector='#effect'),dict(action='read',selector='#missing'),dict(action='click',selector='#effect')])
                stopped=payload(text)
                check('stopped run explicit failure index', not error and stopped.get('status')=='stopped' and stopped.get('stopped_at')==2 and len(stopped.get('results',[]))==2)
                check('stopped run recovery state and guidance', bool(stopped.get('page')) and 'remain applied' in stopped.get('note','') and bool(stopped.get('error')))
                text,error=call('act',action='eval',js='window.effects',format='json')
                check('stop dispatches exactly one mutation', not error and payload(text)==1)
                text,error=call('run',format='json',slim=True,steps=[dict(action='read',selector='#missing',optional=True),dict(action='read',selector='#name',format='json')])
                optional=payload(text); results=optional.get('results',[])
                check('optional failure status separate from completion', not error and optional.get('status')=='completed' and len(results)==2 and results[0].get('status')=='optional_failed' and results[1].get('status')=='ok')
                text,error=call('run',format='nonsense',steps=[dict(action='click',selector='#effect')])
                check('invalid result format refused', error and 'format' in text)
                text,error=call('act',action='eval',js='window.effects',format='json')
                check('invalid format has no side effects', not error and payload(text)==1)
                text,error=call('run',format='json',steps=[])
                check('empty run explicit completion', not error and payload(text)==dict(status='completed',results=[]))
                text,error=call('run',format='json',steps=[dict(action='if',condition='js',js='true',then=[dict(action='read',selector='#missing',optional=True),dict(action='read',selector='#name',format='json')])])
                nested=payload(text)
                check('nested optional observations preserved', not error and nested.get('status')=='completed' and 'optional' in nested.get('results',[{}])[0].get('output','') and 'live Unicode' in nested.get('results',[{}])[0].get('output',''))
                text,error=call('run',format='json',steps=[dict(action='read',selector='#country',format='json') for _ in range(20)])
                large_run=payload(text)
                check('large structured run offloaded', not error and bool(large_run.get('artifact')))
                if large_run.get('artifact'):
                    text,error=call('see',artifact=large_run['artifact'],format='json',limit=100000)
                    full=payload(payload(text).get('data',''))
                    check('all run results survive offload', not error and full.get('status')=='completed' and len(full.get('results',[]))==20 and all('Japan' in r.get('output','') for r in full.get('results',[])))
            finally:
                proc.stdin.close()
                proc.wait(timeout=30)
                check('MCP clean EOF',proc.returncode==0)
finally:
    server.shutdown()
    OUT.write_text(json.dumps(dict(binary=BINARY,sha256=hashlib.sha256(Path(BINARY).read_bytes()).hexdigest(),checks=checks,workflows=workflows,receipts=receipts),ensure_ascii=False,indent=2),encoding='utf-8')
print(str(sum(c['passed'] for c in checks))+'/'+str(len(checks))+' passed')
if '--baseline' not in sys.argv and not all(c['passed'] for c in checks):
    raise SystemExit(1)
