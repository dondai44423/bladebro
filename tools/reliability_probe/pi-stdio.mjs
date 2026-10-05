// Exercise the shipped TypeScript transport with real child processes.
// Requires Node >=23 for its built-in TypeScript parser; no npm installs.
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawn } from 'node:child_process';
import { StringDecoder } from 'node:string_decoder';
import { stripTypeScriptTypes } from 'node:module';
const source = readFileSync(new URL('../../npm/bladebro/pi-extension.ts', import.meta.url), 'utf8');
const start = source.indexOf('interface PendingReq');
const end = source.indexOf('// ── Extension', start);
assert(start > 0 && end > start);
const js = stripTypeScriptTypes(source.slice(start, end), {mode: 'strip'});
const McpStdio = new Function('spawn', 'StringDecoder', js + '\nreturn McpStdio;')(spawn, StringDecoder);
const root = mkdtempSync(join(tmpdir(), 'blade-pi-test-'));
const previousHome = process.env.BLADE_HOME;
process.env.BLADE_HOME = join(root, 'blade-home');
let checks = 0;
const pass = name => {checks++; console.log('PASS', name);};
try {
  const fixture = join(root, 'fixture');
  writeFileSync(fixture, `#!/usr/bin/env python3
import sys,json,time,os
open(os.environ['PI_TEST_PID'],'w').write(str(os.getpid()))
for line in sys.stdin:
 r=json.loads(line)
 if 'id' not in r:continue
 if os.environ.get('PI_TEST_FAIL')=='1':
  print(json.dumps({'id':r['id'],'error':{'message':'handshake rejected'}}),flush=True);continue
 v={'id':r['id'],'result':{'tools':[{'name':'π🦀東京'}]}}
 b=(json.dumps(v,ensure_ascii=False)+'\\n').encode()
 i=b.index('🦀'.encode())+2
 sys.stdout.buffer.write(b[:i]);sys.stdout.buffer.flush();time.sleep(.03)
 sys.stdout.buffer.write(b[i:]);sys.stdout.buffer.flush()
`, {mode: 0o700});
  process.env.PI_TEST_PID = join(root, 'pid');
  const client = new McpStdio();
  await client.start(fixture);
  assert(client.isAlive());
  assert.equal((await client.listTools())[0].name, 'π🦀東京');
  pass('split UTF-8 survives actual subprocess frames');
  await client.stop();
  assert(!client.isAlive());
  const started = Date.now();
  await assert.rejects(client.listTools(), /not running/);
  assert(Date.now() - started < 500);
  pass('dead subprocess fails immediately without dangling request');
  process.env.PI_TEST_FAIL = '1';
  const failed = new McpStdio();
  await assert.rejects(failed.start(fixture));
  assert(!failed.isAlive());
  const pid = Number(readFileSync(process.env.PI_TEST_PID, 'utf8'));
  await new Promise(resolve => setTimeout(resolve, 150));
  assert.throws(() => process.kill(pid, 0), /ESRCH/);
  pass('rejected handshake cleans up its child');
  delete process.env.PI_TEST_FAIL;
  // The actual binary must initialize and discover exactly the supported tools.
  const live = new McpStdio();
  await live.start(resolve(process.env.BLADEBRO || 'target/release/bladebro'));
  assert.deepEqual((await live.listTools()).map(tool => tool.name).sort(), ['act','run','see','state','vision']);
  await live.stop();
  assert(!live.isAlive());
  pass('actual binary handshake, five tools and clean shutdown');
  console.log(`PI STDIO ${checks}/${checks}`);
} finally {
  if (previousHome === undefined) delete process.env.BLADE_HOME;
  else process.env.BLADE_HOME = previousHome;
  delete process.env.PI_TEST_PID;
  delete process.env.PI_TEST_FAIL;
  rmSync(root, {recursive:true,force:true});
}
