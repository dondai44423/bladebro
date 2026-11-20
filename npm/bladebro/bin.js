#!/usr/bin/env node
'use strict';

const { spawn } = require('child_process');
const { constants } = require('os');
const { resolveBinary } = require('./binary.cjs');

const useColor =
  Boolean(process.stderr && process.stderr.isTTY) &&
  !process.env.BLADE_PLAIN &&
  !process.env.NO_COLOR &&
  process.env.TERM !== 'dumb';

const c = {
  crimson: (s) => (useColor ? `\x1b[1;31m${s}\x1b[0m` : s),
  gold: (s) => (useColor ? `\x1b[33m${s}\x1b[0m` : s),
  dim: (s) => (useColor ? `\x1b[2m${s}\x1b[0m` : s),
};

function fail(headline, hint) {
  if (useColor) {
    process.stderr.write(`  ${c.crimson('✖')} ${c.dim('bladebro:')} ${headline}\n`);
    if (hint) process.stderr.write(`    ${c.gold('›')} ${c.dim(hint)}\n`);
  } else {
    process.stderr.write(headline + '\n');
    if (hint) process.stderr.write(hint + '\n');
  }
  process.exit(1);
}

let binPath;
try {
  binPath = resolveBinary();
} catch (error) {
  fail(error.message);
}

function launchFailed(error) {
  // The binary could not be spawned (missing exec bit, wrong arch, deleted
  // file). Without this the shim exited 0 while nothing ran — a silent no-op
  // that scripts and agents read as success.
  const headline = `bladebro: failed to launch ${binPath}: ${error.message}`;
  if (process.platform === 'linux' && error.code === 'ENOENT') {
    // The kernel reports ENOENT for a present ELF whose dynamic loader is
    // missing too. Reinstalling the same glibc prebuild cannot help musl hosts.
    fail(
      headline,
      'The executable or its loader is missing. Prebuilt Linux binaries require glibc >= 2.28; on Alpine/musl, build from source: https://github.com/dondai44423/bladebro#from-source',
    );
  } else if (error.code === 'ENOEXEC') {
    fail(
      headline,
      `Binary architecture mismatch for ${process.platform}-${process.arch}. Reinstall with matching Node architecture: npm install -g bladebro`,
    );
  } else if (error.code === 'EACCES') {
    fail(
      headline,
      `Permission denied. Run chmod +x "${binPath}" or reinstall: npm install -g bladebro`,
    );
  } else {
    fail(headline, 'Reinstall: npm install bladebro');
  }
}

// ── spawn the Rust binary with inherited stdio ────────────────────────
// Asynchronous, not spawnSync: a blocked launcher cannot handle signals, so a
// SIGTERM from a harness killed only Node and orphaned the binary (an MCP
// server then kept its browser until its stdin closed).
let child;
try {
  child = spawn(binPath, process.argv.slice(2), {
    stdio: 'inherit',
    cwd: process.cwd(),
    env: process.env,
  });
} catch (error) {
  // Spawn errors other than ENOENT/EACCES-style ones throw here instead of
  // arriving as an 'error' event.
  launchFailed(error);
}
child.on('error', launchFailed);

// Pass termination on and wait for the binary, so its graceful shutdown runs
// and its exit status is what the caller sees. A duplicate (Ctrl+C reaches the
// whole foreground group) is harmless: the binary acts on the first. Not on
// Windows: the console delivers Ctrl+C/close to the binary itself, and
// child.kill() there is TerminateProcess, which would skip that shutdown.
const FORWARDED = process.platform === 'win32' ? [] : ['SIGTERM', 'SIGINT', 'SIGHUP'];
function forward(signal) {
  child.kill(signal);
}
for (const signal of FORWARDED) process.on(signal, forward);

// Forward exit code or signal
child.on('exit', (code, signal) => {
  if (signal) {
    // Re-raise the signal so the parent process sees it. Drop the forwarders
    // first, or a re-raised SIGTERM/SIGINT/SIGHUP would land in them.
    for (const s of FORWARDED) process.removeListener(s, forward);
    process.kill(process.pid, signal);
    // Still running: Node ignores SIGPIPE, so that re-raise was a no-op and the
    // launcher fell through to exit 0 — `npx bladebro see | head` reported
    // success after the binary died. Report the death the way a shell does.
    process.exit(128 + (constants.signals[signal] || 0));
  }
  process.exit(code ?? 0);
});
