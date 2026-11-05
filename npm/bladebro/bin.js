#!/usr/bin/env node
'use strict';

const { spawnSync } = require('child_process');
const { constants } = require('os');
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
  // Still running: Node ignores SIGPIPE, so that re-raise was a no-op and the
  // launcher fell through to exit 0 — `npx bladebro see | head` reported
  // success after the binary died. Report the death the way a shell does.
  process.exit(128 + (constants.signals[result.signal] || 0));
} else if (result.error) {
  // The binary could not be spawned (missing exec bit, wrong arch, deleted
  // file). Without this branch the shim exited 0 while nothing ran — a
  // silent no-op that scripts and agents read as success.
  process.stderr.write('bladebro: failed to launch ' + binPath + ': ' + result.error.message + '\n');
  if (process.platform === 'linux' && result.error.code === 'ENOENT') {
    // The kernel reports ENOENT for a present ELF whose dynamic loader is
    // missing too. Reinstalling the same glibc prebuild cannot help musl hosts.
    process.stderr.write('The executable or its loader is missing. Prebuilt Linux binaries require glibc >= 2.28; on Alpine/musl, build from source: https://github.com/dondai44423/bladebro#from-source\n');
  } else {
    process.stderr.write('Reinstall: npm install bladebro\n');
  }
  process.exit(1);
} else {
  process.exit(result.status || 0);
}
