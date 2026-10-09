// Real Cordis + native MCP bridge + real Bladebro/Chromium; no model requests.
// DSH_RUNTIME points at an installed harness project (containing node_modules).
import assert from 'node:assert/strict';
import childProcess from 'node:child_process';
import { createRequire, syncBuiltinESMExports } from 'node:module';
import { mkdtemp, rm, readFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { pathToFileURL } from 'node:url';
import { createServer } from 'node:http';

const runtime = resolve(process.env.DSH_RUNTIME || 'work/dsh/runtime');
const host = createRequire(join(runtime, 'package.json'));
const load = name => import(pathToFileURL(host.resolve(name)).href);
const scratch = await mkdtemp(join(tmpdir(), 'bb-dsh-'));
process.env.BLADE_HOME = join(scratch, 'browser data');
process.env.BLADE_NO_WARMING = '1';
process.env.BLADE_PACE = 'off';
const children = [];
const originalSpawn = childProcess.spawn;
childProcess.spawn = function (command, args, options) {
  const child = originalSpawn(command, args, options);
  if (args?.[0] === 'mcp') children.push(child);
  return child;
};
syncBuiltinESMExports();
const { Context } = await load('@deepseek-ai/cordis');
const { default: Tools } = await load('@deepseek-ai/dsh-tools');
const { default: SystemPrompt } = await load('@deepseek-ai/dsh-system-prompt');
const { default: Attachments } = await load('@deepseek-ai/dsh-attachment-local');
const { LlmRuntime, LlmAdapter } = await load('@deepseek-ai/dsh-llm');
const plugin = await import(pathToFileURL(host.resolve('bladebro/dsh-plugin.mjs')).href);
const { resolveBinary } = host('bladebro/binary.cjs');
let checks = 0;
function check(label, value) { assert.ok(value, label); checks++; console.log(`PASS ${label}`); }
async function until(predicate) {
  const deadline = Date.now() + 15000;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error('condition did not settle');
    await new Promise(resolve => setTimeout(resolve, 25));
  }
}
const ctx = new Context();
let posts = [];
const unicode = 'नमस्ते 日本語 🦀 e\u0301';
const server = createServer(async (req, res) => {
  if (req.url === '/save') {
    let body = '';
    for await (const chunk of req) body += chunk;
    posts.push(body);
    res.end('DSH_RECEIPT_' + posts.length);
  } else {
    res.setHeader('Content-Type', 'text/html; charset=utf-8');
    res.end(`<h1>Native DSH browser fixture</h1><p hidden>HIDDEN_SENTINEL</p>
      <form><label>Name<input id="name"></label><button>Save</button></form><p id="receipt"></p>
      <script>document.querySelector('form').onsubmit=async e=>{e.preventDefault();
      document.querySelector('#receipt').textContent=await(await fetch('/save',{method:'POST',body:document.querySelector('#name').value})).text()};</script>`);
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const url = `http://127.0.0.1:${server.address().port}/`;
let id = 0;
const call = (tool, args, extra = {}) => ctx.tools.execute({
  signal: new AbortController().signal, callId: `probe-${++id}`,
  name: `mcp__bladebro__${tool}`, arguments: args, ...extra,
});
const text = r => r.content.filter(c => c.type === 'text').map(c => c.text).join('\n');
const config = process.env.BLADEBRO ? { binaryPath: resolve(process.env.BLADEBRO) } : {};
try {
  await ctx.plugin(SystemPrompt);
  await ctx.plugin(Tools);
  await ctx.plugin(Attachments, { dshHome: join(scratch, 'harness') });
  await ctx.plugin(LlmRuntime);
  // Only image-capability metadata is supplied. Any attempted inference fails.
  class ImageMetadata extends LlmAdapter {
    resolveModel(provider, model) { return Promise.resolve({ provider, id: model, name: model, inputModalities: ['text', 'image'] }); }
    stream() { throw new Error('this probe must never call a model'); }
  }
  ctx.llm.registerAdapter(['fixture'], new ImageMetadata());
  check('fresh registry has zero tools', ctx.tools.schemas().length === 0);
  check('packaged binary resolves independently of PATH', resolveBinary().includes('bladebro-'));
  await assert.rejects(() => plugin.apply(ctx, { binaryPath: 'relative' }), /absolute/); checks++;
  await assert.rejects(() => plugin.apply(ctx, { wrong: true }), /unknown/); checks++;
  await assert.rejects(() => plugin.apply(ctx, []), /object/); checks++;
  const broken = ctx.plugin(plugin, { binaryPath: process.execPath, cwd: scratch });
  await assert.rejects(async () => await broken, /initial connection/); checks++;
  await broken.dispose();
  check('failed handshake leaves no registered tools', ctx.tools.schemas().length === 0);
  const fiber = await ctx.plugin(plugin, config);
  const names = ['act', 'see', 'state', 'run', 'vision'].map(n => `mcp__bladebro__${n}`).sort();
  check('exact five tools registered through native bridge', JSON.stringify(ctx.tools.schemas().map(t => t.name).sort()) === JSON.stringify(names));
  const manual = JSON.parse(childProcess.execFileSync(config.binaryPath || resolveBinary(), ['help', '--json'], { encoding: 'utf8' }));
  const definitions = manual.tools;
  for (const schema of ctx.tools.schemas()) {
    const original = definitions.find(t => 'mcp__bladebro__' + t.name === schema.name);
    assert.deepEqual(schema.parameters, original.inputSchema ?? original.input_schema);
    check(`live schema and description parity: ${schema.name}`, schema.description === original.description);
  }
  check('one MCP child during activation', children.filter(c => c.exitCode === null && c.signalCode === null).length === 1);
  let result = await call('act', { action: 'navigate', url });
  check('real navigation reaches fixture', !result.isError && text(result).includes('Native DSH browser fixture'));
  result = await call('see', { mode: 'content' });
  check('native reading excludes hidden content', !result.isError && text(result).includes('Native DSH browser fixture') && !text(result).includes('HIDDEN_SENTINEL'));
  result = await call('state', { op: 'set-ls', name: 'native-dsh', value: unicode });
  check('state mutation accepted', !result.isError);
  result = await call('act', { action: 'eval', js: "localStorage.getItem('native-dsh')" });
  check('exact Unicode storage readback', !result.isError && text(result).includes(unicode));
  result = await call('act', { action: 'fill', fields: [{ selector: '#name', text: unicode }], submit: 'Save' });
  check('native form fill succeeds', !result.isError);
  await until(() => posts.length === 1);
  check('server received exact Unicode exactly once', posts[0] === unicode && posts.length === 1);
  result = await call('run', { steps: [{ action: 'see', mode: 'content' }] });
  check('run surfaces server-side receipt', !result.isError && text(result).includes('DSH_RECEIPT_1'));
  const agent = { options: { provider: 'fixture', model: 'vision' }, session: { requestHeader: () => undefined } };
  result = await call('vision', { marks: true }, { agent });
  const image = result.content.find(c => c.type === 'image');
  check('vision becomes a native durable image attachment', !result.isError && image?.attachment?.mediaType === 'image/png');
  const saved = await ctx.attachments.readImage(image.attachment);
  check('stored attachment contains real PNG bytes', Buffer.from(saved.data).subarray(0, 8).equals(Buffer.from([137,80,78,71,13,10,26,10])));
  result = await call('state', { op: 'not-an-operation' });
  check('invalid operation is a native tool error', result.isError === true && text(result).length > 0);
  const abort = new AbortController(); abort.abort();
  result = await call('act', { action: 'click', text: 'Save' }, { signal: abort.signal });
  check('pre-aborted mutation fails before dispatch', result.isError === true && posts.length === 1);
  const deny = ctx.on('tools/pre-execute', async () => ({ kind: 'deny', reason: 'fixture permission denied' }));
  result = await call('act', { action: 'click', text: 'Save' });
  check('native permission denial blocks mutation', result.isError === true && text(result).includes('fixture permission denied') && posts.length === 1);
  deny();
  const duplicate = ctx.plugin(plugin, config);
  await assert.rejects(async () => await duplicate, /already in use/); checks++;
  await duplicate.dispose();
  check('duplicate does not disturb existing tools', ctx.tools.schemas().length === 5);
  const midflight = new AbortController();
  const waiting = call('act', { action: 'wait', condition: 'js', js: 'false', timeout: 2 }, { signal: midflight.signal });
  const cancelStart = Date.now();
  setTimeout(() => midflight.abort(), 100);
  result = await waiting;
  check('in-flight observation cancellation returns promptly', result.isError === true && Date.now() - cancelStart < 1500);
  result = await call('see', { mode: 'content' });
  check('next observation works after cancellation', !result.isError && text(result).includes('DSH_RECEIPT_1'));
  const old = children.find(c => c.exitCode === null && c.signalCode === null);
  const generationCount = children.length;
  const oldDefinition = ctx.tools.get('mcp__bladebro__act');
  old.kill();
  await until(() => children.length > generationCount && ctx.tools.get('mcp__bladebro__act') !== oldDefinition && ctx.tools.schemas().length === 5 && children.at(-1).exitCode === null);
  result = await call('act', { action: 'navigate', url });
  check('native supervisor reconnects a killed MCP child', !result.isError && text(result).includes('Native DSH browser fixture'));
  check('crash recovery does not replay a submitted mutation', posts.length === 1);
  await fiber.dispose();
  check('unload unregisters all five tools', ctx.tools.schemas().length === 0);
  await until(() => children.every(c => c.exitCode !== null || c.signalCode !== null));
  check('unload leaves no owned MCP children', children.every(c => c.exitCode !== null || c.signalCode !== null));
  const reload = await ctx.plugin(plugin, config);
  check('re-enable restores exactly five tools', ctx.tools.schemas().length === 5);
  await reload.dispose();
  check('second unload is clean', ctx.tools.schemas().length === 0);
  console.log(`DSH native probe: ${checks} checks passed`);
} catch (error) {
  // Windows can keep a profile file locked after a killed server. Report the
  // assertion before cleanup so that a later EBUSY cannot hide its cause.
  console.error(error);
  throw error;
} finally {
  await ctx.fiber.dispose();
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
  childProcess.spawn = originalSpawn;
  syncBuiltinESMExports();
  for (const child of children) if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
  await rm(scratch, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
