#!/usr/bin/env node
'use strict';

const { spawnSync } = require('child_process');
const { existsSync } = require('fs');
const { dirname, join } = require('path');

// ── platform → npm package name mapping ──────────────────────────────
const PLATFORM_MAP = {
  'linux-x64':    'bladebro-linux-x64',
  'linux-arm64':  'bladebro-linux-arm64',
  'darwin-x64':   'bladebro-darwin-x64',
  'darwin-arm64': 'bladebro-darwin-arm64',
  'win32-x64':    'bladebro-windows-x64',
};

const key = process.platform + '-' + process.arch;
const pkgName = PLATFORM_MAP[key];

if (!pkgName) {
  process.stderr.write('bladebro: no prebuilt binary for ' + key + '\n');
  process.stderr.write('\nSupported platforms:\n');
  process.stderr.write('  linux-x64     Linux x86_64\n');
  process.stderr.write('  linux-arm64   Linux ARM64 (aarch64)\n');
  process.stderr.write('  darwin-arm64  macOS Apple Silicon\n');
  process.stderr.write('  darwin-x64    macOS Intel\n');
  process.stderr.write('  win32-x64     Windows x86_64\n');
  process.stderr.write('\nBuild from source: https://github.com/dondai44423/bladebro\n');
  process.exit(1);
}

// ── resolve the binary path from the platform package ─────────────────
let binPath;
try {
  const pkgJsonPath = require.resolve(pkgName + '/package.json');
  const pkgJson = require(pkgJsonPath);
  const binName = pkgJson.main || 'bladebro';
  binPath = join(dirname(pkgJsonPath), binName);
} catch (e) {
  process.stderr.write('bladebro: platform package "' + pkgName + '" not installed\n');
  process.stderr.write('Run: npm install bladebro\n');
  process.exit(1);
}

// ── verify the binary exists ──────────────────────────────────────────
if (!existsSync(binPath)) {
  process.stderr.write('bladebro: binary not found at ' + binPath + '\n');
  process.stderr.write('Reinstall: npm install bladebro\n');
  process.exit(1);
}

// ── spawn the Rust binary with inherited stdio ────────────────────────
const result = spawnSync(binPath, process.argv.slice(2), {
  stdio: 'inherit',
  cwd: process.cwd(),
  env: process.env,
});

// Forward exit code or signal
if (result.signal) {
  // Re-raise the signal so the parent process sees it
  process.kill(process.pid, result.signal);
} else {
  process.exit(result.status || 0);
}
