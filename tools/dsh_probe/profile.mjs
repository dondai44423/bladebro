// Full real Web profile, installed bundle and standard agent presets. No model calls.
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { createRequire } from 'node:module';
import { resolve, join } from 'node:path';
import { mkdir } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';

if (!process.env.DSH_HOME || !process.env.BLADE_HOME) throw new Error('explicit isolated DSH_HOME and BLADE_HOME required');
process.env.DSH_TELEMETRY_MODE = 'DISABLED';
await mkdir(process.env.BLADE_HOME, { recursive:true, mode:0o700 });
const host = createRequire(join(resolve(process.env.DSH_RUNTIME || 'work/dsh/runtime'), 'package.json'));
const load = name => import(pathToFileURL(host.resolve(name)).href);
const { runProfile } = await load('@deepseek-ai/dsh/profile-boot');
const { loadLayeredEnv } = await load('@deepseek-ai/dsh-app-boot');
const { LlmAdapter } = await load('@deepseek-ai/dsh-llm');
const { ctx, shutdown } = await runProfile({ environment: loadLayeredEnv('dsh'), profile: 'web', patchFiles: [], args: ['--no-open', '--port', '0'] });
const watchdog = setTimeout(() => { console.error('FAIL profile probe did not finish'); process.exit(1); }, 20000);
let checks = 0;
function check(label, value) { assert.ok(value, label); checks++; console.log(`PASS ${label}`); }
const names = ['act','see','state','run','vision'].map(n => `mcp__bladebro__${n}`);
try {
  const bundle = (await ctx.pluginManager.listBundles()).find(b => b.name === 'bladebro');
  check('Plugins page inventory has an installed enabled removable bundle', bundle?.enabled && bundle.installed && bundle.removable && !bundle.error);
  check('bundle inserts one owned row with no unrelated overrides', bundle.rows.length === 1 && bundle.rows[0].rowId === 'bladebro' && bundle.overrides.length === 0);
  check('full Web host registers all five browser tools', names.every(name => ctx.tools.get(name)));
  const baseUrl = `http://127.0.0.1:${ctx.webServer.port}/`;
  let response = await fetch(ctx.connection.authenticatedUrl(baseUrl), { redirect:'manual', signal:AbortSignal.timeout(5000) });
  if (response.status >= 300 && response.status < 400) {
    const cookie = response.headers.getSetCookie().map(c => c.split(';')[0]).join('; ');
    response = await fetch(new URL(response.headers.get('location'), baseUrl), { headers:{ cookie }, signal:AbortSignal.timeout(5000) });
  }
  const html = await response.text();
  check('actual Web frontend serves HTML', response.ok && html.includes('<html'));
  class OfflineMetadata extends LlmAdapter {
    resolveModel(provider, model) { return Promise.resolve({ provider, id:model, name:model, inputModalities:['text','image'] }); }
    stream() { throw new Error('profile probe must never call a model'); }
  }
  ctx.llm.registerAdapter(['fixture'], new OfflineMetadata());
  for (const preset of ['standard','ptc','cordis']) {
    const handle = await ctx.agents.create({ sessionId:`bladebro-probe-${preset}-${randomUUID()}`, meta:{ cwd:resolve(process.env.BLADE_HOME), agentPreset:preset }, agentOptions:{ provider:'fixture', model:'offline' } });
    try {
      check(`${preset} agent inherits all five browser capabilities`, names.every(name => handle.agent.ctx.tools.get(name)));
    } finally { await handle.dispose(); }
  }
  // Keep an independent real MCP connection alive across bundle reloads.
  // DSH's generic resource helper tools legitimately disappear with its LAST
  // MCP server; a second server makes this a meaningful unrelated-tool check.
  const profileRequire = createRequire(join(resolve(process.env.DSH_HOME), 'profiles/web/package.json'));
  const { resolveBinary } = profileRequire('bladebro/binary.cjs');
  await ctx.plugin(await load('@deepseek-ai/dsh-mcp-client'), {
    transport:'stdio', serverName:'unrelated_fixture', command:resolveBinary(), args:['mcp'],
    env:{ BLADE_HOME:join(resolve(process.env.BLADE_HOME), 'other-mcp') }, failOnStartupError:true,
  });
  check('independent MCP connection registers its own five tools', ['act','see','state','run','vision'].every(n => ctx.tools.get(`mcp__unrelated_fixture__${n}`)));
  const others = ctx.tools.schemas().filter(t => !names.includes(t.name)).map(t => t.name).sort();
  for (const enabled of [false, true]) {
    const change = await ctx.pluginManager.setBundleEnabled('bladebro', enabled);
    check(`Plugins bundle toggle ${enabled} persists honestly`, change.changed && ['applied','restart-required'].includes(change.application));
    const current = (await ctx.pluginManager.listBundles()).find(b => b.name === 'bladebro');
    check(`Plugins inventory reflects enabled=${enabled}`, current.enabled === enabled && current.installed);
    if (change.application === 'applied') {
      check(`Plugins toggle ${enabled} reaches actual tool registration`, names.every(name => Boolean(ctx.tools.get(name)) === enabled));
    }
    assert.deepEqual(ctx.tools.schemas().filter(t => !names.includes(t.name)).map(t => t.name).sort(), others); checks++;
  }
  console.log(`DSH full profile probe: ${checks} checks passed`);
} finally { clearTimeout(watchdog); await shutdown.shutdown(checks >= 14 ? 0 : 1); }
