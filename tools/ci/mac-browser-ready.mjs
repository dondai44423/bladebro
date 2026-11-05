// Gate native macOS browser tests on a headful Chrome that can actually take input.
// On cold macOS CI VMs the first headful Chrome's main thread can wedge in a synchronous
// IconServices XPC call (AppKit populating the Share menu; seen in process samples), so any
// Input.* call times out for minutes. A throwaway Chrome absorbs that here, bounded: if input
// does not complete, kill it and try a fresh one. This only waits for the machine; the real
// tests still run unchanged with their own assertions.
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const chromePath = process.env.CHROME_PATH;
if (!chromePath) throw new Error('CHROME_PATH must identify the browser under test');
const budgetMs = Number(process.env.READY_BUDGET_MS || 600000);
const inputTimeoutMs = Number(process.env.READY_INPUT_TIMEOUT_MS || 15000);
const started = Date.now();
const log = message => console.log(`[ready +${((Date.now() - started) / 1000).toFixed(0)}s] ${message}`);
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const bounded = (promise, ms, label) => {
  let timer;
  return Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms); })]).finally(() => clearTimeout(timer));
};

async function attempt(number) {
  const home = await mkdtemp(join(tmpdir(), 'bb-ready-'));
  const child = spawn(chromePath, [`--user-data-dir=${home}`, '--remote-debugging-port=0', '--no-first-run', '--no-default-browser-check', 'about:blank'], { stdio: 'ignore' });
  try {
    const port = await bounded((async () => {
      for (;;) {
        try { return Number((await readFile(join(home, 'DevToolsActivePort'), 'utf8')).split('\n')[0]); } catch { await sleep(100); }
      }
    })(), 30000, 'startup');
    const targets = await (await fetch(`http://127.0.0.1:${port}/json`, { signal: AbortSignal.timeout(5000) })).json();
    const url = targets.find(target => target.type === 'page')?.webSocketDebuggerUrl;
    if (!url) throw new Error('no page target');
    const socket = new WebSocket(url);
    await bounded(new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = () => reject(new Error('websocket error')); }), 5000, 'websocket');
    let id = 0;
    const pending = new Map();
    socket.onmessage = event => { const message = JSON.parse(event.data); pending.get(message.id)?.(message); };
    const send = (method, params) => new Promise(resolve => { const mine = ++id; pending.set(mine, resolve); socket.send(JSON.stringify({ id: mine, method, params })); });
    // The calls that wedged: real browser-process input, not renderer-only evaluation.
    await bounded(send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: 20, y: 20 }), inputTimeoutMs, 'mouse input');
    await bounded(send('Input.dispatchKeyEvent', { type: 'char', text: 'a' }), inputTimeoutMs, 'key input');
    socket.close();
    log(`attempt ${number}: browser input answered`);
    return true;
  } catch (error) {
    log(`attempt ${number}: ${error.message}`);
    return false;
  } finally {
    child.kill('SIGKILL');
    await sleep(500);
    await rm(home, { recursive: true, force: true }).catch(() => {});
  }
}

for (let number = 1; Date.now() - started < budgetMs; number++) {
  if (await attempt(number)) process.exit(0);
  await sleep(2000);
}
log('browser input never answered within the budget; continuing so the real tests report the failure');
process.exit(0);
