#!/usr/bin/env node
'use strict';

const { spawnSync } = require('child_process');
const { resolveBinary } = require('./binary.cjs');

let binPath;
try {
  binPath = resolveBinary();
} catch (error) {
  process.stderr.write(error.message + '\n');
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
} else if (result.error) {
  // The binary could not be spawned (missing exec bit, wrong arch, deleted
  // file). Without this branch the shim exited 0 while nothing ran — a
  // silent no-op that scripts and agents read as success.
  process.stderr.write('bladebro: failed to launch ' + binPath + ': ' + result.error.message + '\n');
  process.stderr.write('Reinstall: npm install bladebro\n');
  process.exit(1);
} else {
  process.exit(result.status || 0);
}
