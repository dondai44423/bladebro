// Portable package-boundary checks; simulated platform resolution is explicitly
// separate from the real native host/browser probe in run.mjs.
import assert from 'node:assert/strict';
import { readFileSync, mkdtempSync, mkdirSync, writeFileSync, chmodSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { createRequire } from 'node:module';
import { runInNewContext } from 'node:vm';
import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';

const root = resolve('npm/bladebro');
const manifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
const require = createRequire(join(root, 'package.json'));
const scratch = mkdtempSync(join(tmpdir(), 'bb-package-'));
let checks = 0;
function check(label, value) { assert.ok(value, label); checks++; console.log(`PASS ${label}`); }
try {
  check('no package install scripts or new runtime dependencies', !manifest.scripts && !manifest.dependencies);
  check('native bundle is included in published files', manifest.dsh.bundle.patch === './cordis.patch.yml' && ['binary.cjs','dsh-plugin.mjs','cordis.patch.yml'].every(f => manifest.files.includes(f)));
  const source = readFileSync(join(root, 'binary.cjs'), 'utf8');
  for (const [platform, arch, pkg] of [
    ['linux','x64','bladebro-linux-x64'], ['linux','arm64','bladebro-linux-arm64'],
    ['darwin','x64','bladebro-darwin-x64'], ['darwin','arm64','bladebro-darwin-arm64'],
    ['win32','x64','bladebro-windows-x64'], ['win32','arm64',null],
  ]) {
    const base = join(scratch, `${platform} ${arch} Unicode 日本語`);
    mkdirSync(base);
    const binary = join(base, platform === 'win32' ? 'bladebro.exe' : 'bladebro');
    writeFileSync(binary, 'fixture'); chmodSync(binary, 0o755);
    const customRequire = name => require(name);
    customRequire.resolve = name => { assert.equal(name, pkg + '/package.json'); return join(base, 'package.json'); };
    const module = { exports: {} };
    runInNewContext(source, { require: customRequire, module, process: { platform, arch } });
    const { resolveBinary } = module.exports;
    if (!pkg) { assert.throws(() => resolveBinary(), /no prebuilt binary/); checks++; }
    else check(`${platform}-${arch} resolves exact native package with spaces and Unicode`, resolveBinary() === binary);
    check(`${platform}-${arch} accepts explicit absolute source build`, resolveBinary(binary) === binary);
    for (const invalid of ['', 'relative', null, 4, true, {}, `${binary}\0`]) {
      assert.throws(() => resolveBinary(invalid), /absolute executable/); checks++;
    }
    assert.throws(() => resolveBinary(base), /cannot execute/); checks++;
    assert.throws(() => resolveBinary(join(base, 'missing')), /cannot execute/); checks++;
  }
  const plugin = await import(pathToFileURL(join(root, 'dsh-plugin.mjs')).href);
  for (const value of [0, -1, 999, 2147483648, Infinity, NaN, '1000', null]) {
    await assert.rejects(() => plugin.apply({}, { toolCallTimeoutMs: value }), /integer/); checks++;
  }
  const isolated = join(scratch, 'launcher'); mkdirSync(isolated);
  for (const f of ['bin.js','binary.cjs']) writeFileSync(join(isolated, f), readFileSync(join(root, f)));
  const result = spawnSync(process.execPath, [join(isolated, 'bin.js'), 'mcp'], { encoding: 'utf8' });
  check('missing optional dependency exits nonzero with recovery guidance', result.status === 1 && result.stderr.includes('optional dependencies enabled') && result.stdout === '');
  console.log(`DSH package probe: ${checks} checks passed`);
} finally { rmSync(scratch, { recursive: true, force: true }); }
