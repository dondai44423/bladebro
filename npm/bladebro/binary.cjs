'use strict';

const { accessSync, constants, statSync } = require('fs');
const { dirname, isAbsolute, join } = require('path');

const PLATFORMS = {
  'linux-x64': 'bladebro-linux-x64',
  'linux-arm64': 'bladebro-linux-arm64',
  'darwin-x64': 'bladebro-darwin-x64',
  'darwin-arm64': 'bladebro-darwin-arm64',
  'win32-x64': 'bladebro-windows-x64',
};

// Resolve beside this package, never through a shell or a desktop app's PATH.
// Both the CLI launcher and DSH use the very same optional binary dependency.
function resolveBinary(override) {
  let path = override;
  if (path !== undefined && (typeof path !== 'string' || !isAbsolute(path) || path.includes('\0'))) {
    throw new Error('bladebro: binaryPath must be an absolute executable path');
  }
  if (path === undefined) {
    const key = `${process.platform}-${process.arch}`;
    const pkg = PLATFORMS[key];
    if (!pkg) throw new Error(`bladebro: no prebuilt binary for ${key}. Build from source: https://github.com/dondai44423/bladebro#from-source`);
    try {
      const manifest = require.resolve(`${pkg}/package.json`);
      path = join(dirname(manifest), process.platform === 'win32' ? 'bladebro.exe' : 'bladebro');
    } catch {
      throw new Error(`bladebro: platform package "${pkg}" not installed. Reinstall bladebro with optional dependencies enabled.`);
    }
  }
  try {
    if (!statSync(path).isFile()) throw new Error('not a file');
    accessSync(path, process.platform === 'win32' ? constants.F_OK : constants.X_OK);
  } catch (error) {
    throw new Error(`bladebro: cannot execute ${path}: ${error.message}. Reinstall bladebro or set binaryPath.`);
  }
  return path;
}

module.exports = { resolveBinary };
