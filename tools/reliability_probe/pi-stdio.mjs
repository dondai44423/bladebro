// The pi path, end to end: load the shipped TypeScript extension against a mock
// ExtensionAPI, then speak MCP stdio to the exact command it registered — the
// same path a pi session takes. Requires Node >=23 for stripTypeScriptTypes;
// no npm installs, no pi install, no copied protocol code to babysit.
import assert from 'node:assert/strict';
import {
  copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { spawn } from 'node:child_process';
import { stripTypeScriptTypes } from 'node:module';

const root = mkdtempSync(join(tmpdir(), 'blade-pi-test-'));
const previousHome = process.env.BLADE_HOME;
process.env.BLADE_HOME = join(root, 'blade-home');
let checks = 0;
const pass = (name) => { checks++; console.log('PASS', name); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Load `npm/bladebro/pi-extension.ts` the way pi does: types stripped, host APIs
 *  injected. `resolvePackageJson(name)` returns the platform package's package.json
 *  path — omitted to simulate a package that is not installed. The extension keeps
 *  its imports single-line; this harness supplies them. */
function loadExtension(resolvePackageJson) {
  const source = readFileSync(new URL('../../npm/bladebro/pi-extension.ts', import.meta.url), 'utf8');
  const body = stripTypeScriptTypes(
    source.replace(/^import[^\n]*\n/gm, '').replace(/^export default /m, ''),
    { mode: 'strip' });
  const requireShim = Object.assign(
    (path) => JSON.parse(readFileSync(path, 'utf8')),
    {
      resolve: (name) => {
        if (!resolvePackageJson) throw new Error(`not installed: ${name}`);
        return resolvePackageJson(name);
      },
    },
  );
  const factory = new Function('require', 'existsSync', 'dirname', 'join', `${body}\nreturn bladebroExtension;`);
  return factory(requireShim, existsSync, dirname, join);
}

/** Mock pi ExtensionAPI recording registrations and event handlers. */
function mockPi() {
  const servers = [];
  const events = {};
  return {
    servers,
    events,
    api: {
      registerMcpServer: (name, config) => servers.push({ name, config }),
      on: (event, handler) => { events[event] = handler; },
    },
  };
}

/** Minimal MCP stdio client — the probe's own, so the binary's protocol is under test. */
function mcpClient(command, args) {
  const child = spawn(command, args, { stdio: ['pipe', 'pipe', 'pipe'], env: { ...process.env } });
  let buffer = '';
  let nextId = 1;
  let stderrText = '';
  const pending = new Map();
  child.stderr.on('data', (d) => { stderrText += d.toString(); });
  child.stdout.on('data', (d) => {
    buffer += d.toString('utf8');
    let i;
    while ((i = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, i).trim();
      buffer = buffer.slice(i + 1);
      if (!line) continue;
      const msg = JSON.parse(line);
      const entry = pending.get(msg.id);
      if (!entry) continue;
      pending.delete(msg.id);
      if (msg.error) entry.reject(new Error(msg.error.message || 'MCP error'));
      else entry.resolve(msg.result);
    }
  });
  return {
    child,
    stderr: () => stderrText,
    exited: new Promise((r) => child.once('exit', (code, signal) => r({ code, signal }))),
    request(method, params) {
      const id = nextId++;
      return new Promise((res, rej) => {
        const timer = setTimeout(() => { pending.delete(id); rej(new Error(`timeout: ${method}`)); }, 15000);
        pending.set(id, {
          resolve: (v) => { clearTimeout(timer); res(v); },
          reject: (e) => { clearTimeout(timer); rej(e); },
        });
        child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
      });
    },
    notify(method, params) { child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method, params })}\n`); },
  };
}

try {
  const binary = resolve(process.env.BLADEBRO || 'target/release/bladebro');
  assert(existsSync(binary), `binary to exercise: ${binary}`);
  const osName = { linux: 'linux', darwin: 'darwin', win32: 'windows' }[process.platform];
  const cpuName = { x64: 'x64', arm64: 'arm64' }[process.arch];
  const expectedPackage = `bladebro-${osName}-${cpuName}`;
  const pkgDir = join(root, expectedPackage);
  mkdirSync(pkgDir, { recursive: true });
  writeFileSync(join(pkgDir, 'package.json'), JSON.stringify({ name: expectedPackage, main: 'bladebro' }));
  if (process.platform === 'win32') copyFileSync(binary, join(pkgDir, 'bladebro'));
  else symlinkSync(binary, join(pkgDir, 'bladebro'));

  // 1. Loading the extension registers exactly one direct MCP server, aimed at the real binary.
  const asked = [];
  const mock = mockPi();
  loadExtension((name) => { asked.push(name); return join(pkgDir, 'package.json'); })(mock.api);
  assert.equal(mock.servers.length, 1, 'exactly one server');
  const { name, config } = mock.servers[0];
  assert.equal(name, 'bladebro');
  assert.deepEqual(asked, [`${expectedPackage}/package.json`], 'resolves the platform package, nothing else');
  assert.equal(config.command, join(pkgDir, 'bladebro'));
  assert.deepEqual(config.args, ['mcp']);
  assert.equal(config.exposure, 'direct', 'tools must be declared to the model');
  assert.ok(config.timeout >= 180, "timeout must cover the driver's long operations");
  pass(`extension registers one direct MCP server (${config.command} ${config.args.join(' ')})`);

  // 2. The registered command initializes and carries the work-loop instructions.
  const client = mcpClient(config.command, config.args);
  const init = await client.request('initialize', {
    protocolVersion: '2025-06-18',
    capabilities: {},
    clientInfo: { name: 'pi-probe', version: '1' },
  });
  client.notify('notifications/initialized', {});
  assert.ok(init.protocolVersion, 'server answers with a protocol version');
  assert.equal(typeof init.instructions, 'string');
  assert.ok(
    init.instructions.length > 200 && /WORK LOOP/.test(init.instructions),
    `instructions must carry the loop: ${JSON.stringify(init.instructions?.slice(0, 80))}`,
  );
  pass(`initialize carries ${init.instructions.length} chars of instructions`);

  // 3. Five tools, each with a model-facing description and an object schema.
  const tools = (await client.request('tools/list', {})).tools;
  assert.deepEqual(tools.map((t) => t.name).sort(), ['act', 'run', 'see', 'state', 'vision']);
  for (const tool of tools) {
    assert.ok(tool.description && tool.description.length > 50, `${tool.name} needs a model-facing description`);
    assert.equal(tool.inputSchema?.type, 'object', `${tool.name} needs an object schema`);
  }
  pass('actual binary: five tools with descriptions and schemas');

  // 4. stdin EOF (the client is gone) ends the process — no orphaned server.
  client.child.stdin.end();
  const closed = await Promise.race([client.exited, sleep(5000).then(() => null)]);
  assert.ok(closed, `server must exit on stdin EOF; stderr: ${client.stderr()}`);
  await sleep(150);
  assert.throws(() => process.kill(client.child.pid, 0), /ESRCH/, 'process is reaped, not left running');
  pass(`stdin EOF ends the server (exit ${closed.code})`);

  // 5. Missing binary: nothing registered, and the reason is visible at session start.
  const missing = mockPi();
  loadExtension(null)(missing.api);
  assert.equal(missing.servers.length, 0, 'no registration without a binary');
  const notices = [];
  missing.events.session_start({}, { ui: { notify: (message, level) => notices.push({ message, level }) } });
  assert.equal(notices.length, 1);
  assert.equal(notices[0].level, 'error');
  assert.match(notices[0].message, /npm install bladebro/);
  pass('missing binary: no registration, visible reason at session start');

  console.log(`PI STDIO ${checks}/${checks}`);
} finally {
  if (previousHome === undefined) delete process.env.BLADE_HOME;
  else process.env.BLADE_HOME = previousHome;
  rmSync(root, { recursive: true, force: true });
}
