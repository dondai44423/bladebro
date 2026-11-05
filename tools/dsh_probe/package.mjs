// Portable package-boundary checks; simulated platform resolution is explicitly
// separate from the real native host/browser probe in run.mjs.
import assert from 'node:assert/strict';
import { readFileSync, mkdtempSync, mkdirSync, writeFileSync, chmodSync, rmSync } from 'node:fs';
import { tmpdir, constants as osConstants } from 'node:os';
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
    if (!pkg) { assert.throws(() => resolveBinary(), /no prebuilt binary.*Build from source: https:/); checks++; }
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
  if (process.platform === 'linux' && ['x64', 'arm64'].includes(process.arch)) {
    // ENOENT also means a present executable has no usable interpreter/ELF
    // loader (the glibc prebuild on Alpine/musl). Reinstalling cannot fix it.
    const native = join(isolated, 'node_modules', 'bladebro-linux-' + process.arch);
    mkdirSync(native, { recursive: true });
    writeFileSync(join(native, 'package.json'), JSON.stringify({ name: 'bladebro-linux-' + process.arch }));
    writeFileSync(join(native, 'bladebro'), '#!/bladebro-fixture-missing-interpreter\n');
    chmodSync(join(native, 'bladebro'), 0o755);
    const loader = spawnSync(process.execPath, [join(isolated, 'bin.js'), '--version'], { encoding: 'utf8' });
    if (!loader.stderr.includes('glibc >= 2.28')) process.stderr.write(loader.stderr);
    check('missing Linux loader names glibc and source-build recovery', loader.status === 1 && loader.stdout === '' && loader.stderr.includes('glibc >= 2.28') && loader.stderr.includes('build from source'));
  }
  const host = process.platform + '-' + process.arch;
  if (['linux-x64', 'linux-arm64', 'darwin-x64', 'darwin-arm64'].includes(host)) {
    // The launcher must be invisible to whoever runs it: the binary's exit
    // status or death signal comes back unchanged, and termination sent to the
    // launcher reaches the binary (a harness stopping `npx bladebro mcp` with
    // SIGTERM must not orphan the MCP server and its browser).
    const native = join(isolated, 'node_modules', 'bladebro-' + host);
    mkdirSync(native, { recursive: true });
    writeFileSync(join(native, 'package.json'), JSON.stringify({ name: 'bladebro-' + host }));
    const fake = body => { writeFileSync(join(native, 'bladebro'), '#!/bin/sh\n' + body); chmodSync(join(native, 'bladebro'), 0o755); };
    const launch = () => spawnSync(process.execPath, [join(isolated, 'bin.js')], { encoding: 'utf8' });
    fake('exit 7\n');
    check('binary exit status passes through the launcher', launch().status === 7);
    // Node ignores SIGPIPE, so re-raising it on the launcher is a no-op: the
    // launcher must still not report success for `bladebro see | head`.
    fake('kill -s PIPE $$\n');
    const pipe = launch();
    if (pipe.status !== 128 + osConstants.signals.SIGPIPE) process.stderr.write(`SIGPIPE death came back as status=${pipe.status} signal=${pipe.signal}\n`);
    check('binary killed by SIGPIPE makes the launcher exit 128+SIGPIPE, not 0', pipe.status === 128 + osConstants.signals.SIGPIPE);
    fake('kill -s TERM $$\n');
    check('binary killed by SIGTERM kills the launcher by SIGTERM', launch().signal === 'SIGTERM');
  }
  console.log(`DSH package probe: ${checks} checks passed`);
} finally { rmSync(scratch, { recursive: true, force: true }); }
