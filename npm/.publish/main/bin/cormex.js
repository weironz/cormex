#!/usr/bin/env node
// The npm entry point: find the platform's binary and hand over to it.
//
// # Why the platform packages are `optionalDependencies`
//
// npm installs an optional dependency only when it exists for this platform. So
// a Linux x64 machine installs `@cormex/cli-linux-x64` and skips the other
// five, and the main package carries no binaries of its own. The alternative —
// one package with every platform's binary — is a 40 MB download for a 4 MB
// tool.
//
// # Why this is a JS shim rather than a postinstall script
//
// A `postinstall` that downloads from GitHub fails behind a corporate proxy,
// fails when GitHub is slow, and fails silently in a way that leaves a
// half-installed package. The platform packages are ordinary npm packages, so
// `npm install` handles the download, the cache and the integrity check.

'use strict';

const { spawnSync } = require('node:child_process');
const path = require('node:path');

const platform = process.platform;
const arch = process.arch;

// npm's names for the platforms, not Node's: `win32` and `x64` are what the
// package names use
const pkg = `@cormex/cli-${platform}-${arch}`;

let binary;
try {
  // `require.resolve` finds the package's `package.json`, and the binary sits
  // beside it
  const pkgPath = require.resolve(`${pkg}/package.json`);
  const dir = path.dirname(pkgPath);
  binary = path.join(dir, platform === 'win32' ? 'cormex.exe' : 'cormex');
} catch {
  console.error(
    `cormex: no prebuilt binary for ${platform}-${arch}.\n` +
      `The package ${pkg} is optional and was not installed — either this\n` +
      `platform has no build, or npm skipped it (--no-optional, or an offline\n` +
      `install). Install the binary directly instead:\n` +
      `  https://github.com/weironz/cormex/releases`
  );
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  console.error(`cormex: could not run ${binary}: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
