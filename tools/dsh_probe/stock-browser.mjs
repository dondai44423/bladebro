// Separate stock-Chrome liveness from driver/bridge failures on native CI.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { once } from 'node:events';

assert.ok(process.env.CHROME_PATH, 'CHROME_PATH must identify this fixture browser');
const home = await mkdtemp(join(tmpdir(), 'bb-stock-cdp-'));
const args = [`--user-data-dir=${home}`, '--remote-debugging-port=0', '--no-first-run', '--no-default-browser-check', 'about:blank'];
const injectionPath = process.argv[2];
if (injectionPath) args.unshift('--enable-unsafe-swiftshader', '--ignore-gpu-blocklist');
if (process.platform === 'linux') args.unshift('--headless=new');
const child = spawn(process.env.CHROME_PATH, args, { stdio: ['ignore', 'ignore', 'pipe'] });
let stderr = '';
let spawnError;
let stopping = false;
child.on('error', error => { spawnError = error; });
child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-8000); });
let socket;
const bounded = (promise, ms, label) => {
  let timer;
  return Promise.race([promise, new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  })]).finally(() => clearTimeout(timer));
};
try {
  const port = await bounded((async () => {
    while (!stopping) {
      if (spawnError) throw spawnError;
      if (child.exitCode !== null) throw new Error(`Chrome exited: ${stderr}`);
      try { return Number((await readFile(join(home, 'DevToolsActivePort'), 'utf8')).split('\n')[0]); }
      catch { await new Promise(resolve => setTimeout(resolve, 50)); }
    }
    throw new Error('stock startup cancelled');
  })(), 15000, 'stock startup');
  const targets = await (await fetch(`http://127.0.0.1:${port}/json`, { signal: AbortSignal.timeout(5000) })).json();
  const target = targets.find(target => target.type === 'page');
  assert.ok(target?.webSocketDebuggerUrl, 'stock page target exists');
  socket = new WebSocket(target.webSocketDebuggerUrl);
  await bounded(once(socket, 'open'), 5000, 'stock CDP connect');
  let id = 0;
  const send = async (method, params) => {
    const callId = ++id;
    const started = Date.now();
    let listener;
    try {
      const reply = new Promise(resolve => {
        listener = event => { const value = JSON.parse(event.data); if (value.id === callId) resolve(value); };
        socket.addEventListener('message', listener);
      });
      socket.send(JSON.stringify({ id: callId, method, params }));
      const result = await bounded(reply, 10000, method);
      assert.equal(result.error, undefined, JSON.stringify(result));
      console.log('STOCK CDP', method, Date.now() - started, JSON.stringify(result));
      return result.result;
    } finally { socket.removeEventListener('message', listener); }
  };
  assert.equal((await send('Runtime.evaluate', { expression: '1+1', returnByValue: true })).result.value, 2);
  await send('Runtime.evaluate', { expression: "document.body.innerHTML='<h1>Stock browser fixture</h1>'", returnByValue: true });
  assert.equal((await send('Runtime.evaluate', { expression: 'document.body.innerText', returnByValue: true })).result.value, 'Stock browser fixture');
  await send('Runtime.evaluate', { expression: "(()=>{const g=document.createElement('canvas').getContext('webgl');return g?g.getParameter(g.RENDERER):null})()", returnByValue: true });
  console.log('STOCK CDP 4/4');
  if (injectionPath) {
    await send('Page.enable', {});
    await send('Page.addScriptToEvaluateOnNewDocument', { source: await readFile(injectionPath, 'utf8'), runImmediately: true });
    let listener;
    const loaded = new Promise(resolve => {
      listener = event => { if (JSON.parse(event.data).method === 'Page.loadEventFired') resolve(); };
      socket.addEventListener('message', listener);
    });
    try {
      await send('Page.navigate', { url: 'data:text/html,<h1>Injected browser fixture</h1>' });
      await bounded(loaded, 10000, 'injected load');
    } finally { socket.removeEventListener('message', listener); }
    assert.equal((await send('Runtime.evaluate', { expression: 'document.body.innerText', returnByValue: true })).result.value, 'Injected browser fixture');
    await send('Runtime.evaluate', { expression: "(()=>{const g=document.createElement('canvas').getContext('webgl');const e=g?.getExtension('WEBGL_debug_renderer_info');return e?g.getParameter(e.UNMASKED_RENDERER_WEBGL):null})()", returnByValue: true });
    console.log('INJECTED CDP 9/9');
  }
} catch (error) {
  console.error('Stock Chrome stderr:', stderr);
  throw error;
} finally {
  stopping = true;
  socket?.close();
  if (child.pid && child.exitCode === null) {
    child.kill('SIGTERM');
    try { await bounded(once(child, 'exit'), 5000, 'stock shutdown'); }
    catch { child.kill('SIGKILL'); await once(child, 'exit'); }
  }
  await rm(home, { recursive: true, force: true });
}
