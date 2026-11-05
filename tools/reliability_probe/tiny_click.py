#!/usr/bin/env python3
"""Native hit accuracy, single dispatch and trust on tiny/subpixel controls."""
import hashlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading

BINARY = os.environ.get('BLADEBRO', str(Path(__file__).resolve().parents[2] / 'target/release/bladebro'))
OUT = Path(os.environ.get('POLISH_OUT', 'work/tiny-click.json'))
OUT.parent.mkdir(parents=True, exist_ok=True)
checks = []

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'''<!doctype html><title>Tiny targets</title><style>
        #tiny,#check{position:fixed;padding:0;border:0;margin:0;appearance:none;background:red;}
        #tiny{left:100px;top:100px;width:2px;height:2px;}
        #check{left:200.75px;top:100.75px;width:.5px;height:.5px;}
        </style><button id="tiny" type="button" aria-label="Tiny button"></button>
        <input id="check" type="checkbox" aria-label="Tiny consent">
        <script>window.hits=0;window.changes=0;window.events=[];
        tiny.onclick=e=>{hits++;events.push({target:'button',trusted:e.isTrusted});};
        check.onchange=()=>{changes++;};
        check.onclick=e=>{events.push({target:'checkbox',trusted:e.isTrusted});};
        </script>'''
        self.send_response(200)
        self.send_header('Content-Type','text/html; charset=utf-8')
        self.send_header('Content-Length',str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self,*_):
        pass

def check(name, passed):
    checks.append(dict(name=name,passed=bool(passed)))
    print(('PASS ' if passed else 'FAIL ')+name,flush=True)

server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
threading.Thread(target=server.serve_forever,daemon=True).start()
try:
    with tempfile.TemporaryDirectory(prefix='blade-tiny-click-') as home:
        env=dict(os.environ,BLADE_HOME=home,BLADE_NO_WARMING='1',BLADE_NO_UPDATE_CHECK='1',BLADE_LANE='agent')
        for key in ('BLADE_PROFILE_DIR','BLADE_TRANSPORT','BLADE_PROXY','BLADE_FRESH'):
            env.pop(key,None)
        args=[dict(action='navigate',url='http://127.0.0.1:'+str(server.server_port))]
        for _ in range(10):
            args += [dict(action='click',selector='#tiny',slim=True),dict(action='fill',fields=[dict(selector='#check')],slim=True)]
        args += [dict(action='eval',js='({hits,changes,checked:check.checked,events})',format='json')]
        requests=[dict(jsonrpc='2.0',id=i+1,method='tools/call',params=dict(name='act',arguments=a)) for i,a in enumerate(args)]
        result=subprocess.run([BINARY,'mcp'],input=''.join(json.dumps(r)+'\n' for r in requests),capture_output=True,text=True,encoding='utf-8',env=env,timeout=180)
        replies=[json.loads(line) for line in result.stdout.splitlines()]
        good=result.returncode==0 and len(replies)==len(requests) and all('error' not in r and not r.get('result',{}).get('isError') for r in replies)
        check('all native clicks accepted and clean EOF',good)
        data={}
        if replies:
            text='\n'.join(c.get('text','') for c in replies[-1].get('result',{}).get('content',[]))
            try:
                data=json.loads(text)
                if isinstance(data,dict) and set(data)=={'data','note'}:
                    data=data['data']
            except ValueError:
                pass
        check('ten exact button hits without replay',data.get('hits')==10)
        check('ten native subpixel checkbox toggles without replay',data.get('changes')==10 and data.get('checked') is False)
        check('all twenty activation events remain trusted',len(data.get('events',[]))==20 and all(e['trusted'] for e in data['events']))
        OUT.with_suffix('.stderr.log').write_text(result.stderr,encoding='utf-8')
        OUT.write_text(json.dumps(dict(binary=BINARY,sha256=hashlib.sha256(Path(BINARY).read_bytes()).hexdigest(),checks=checks,requests=requests,replies=replies,readback=data),ensure_ascii=False,indent=2),encoding='utf-8')
finally:
    server.shutdown()
    server.server_close()
failed=sum(not c['passed'] for c in checks)
print(f'TINY CLICKS {len(checks)-failed}/{len(checks)}; failures {failed}',flush=True)
if '--baseline' not in sys.argv and failed:
    raise SystemExit(1)
