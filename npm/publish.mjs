// Build and publish the npm packages from the release artefacts.
//
//   node npm/publish.mjs v0.1.11 ../dist
//
// # Why one script rather than six `npm publish` steps in the workflow
//
// The platform packages are generated: each needs the right binary copied in
// and a `package.json` whose `os`/`cpu` fields make npm skip it on the wrong
// platform. That is six nearly identical files, and writing them by hand in YAML
// is how one of them ends up with the wrong `cpu` — which npm then installs on
// the wrong machine, and the failure is "exec format error" at runtime.
//
// # What it publishes
//
// `@cormex/cli-<platform>` for each artefact present, then the main `cormex`
// package pinned to the same version. The main package must go **last**: it
// depends on the platform packages, and npm refuses to install a dependency
// that does not exist yet.

import { execFileSync } from 'node:child_process';
import { cpSync, mkdirSync, readdirSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

const version = process.argv[2];
const distDir = process.argv[3];
if (!version || !distDir) {
  console.error('usage: node npm/publish.mjs <version> <dist-dir>');
  process.exit(2);
}
// npm versions have no leading `v`; the tag does
const npmVersion = version.replace(/^v/, '');

/** Rust target → npm's `os`/`cpu`. */
const TARGETS = {
  'x86_64-unknown-linux-musl': { os: 'linux', cpu: 'x64' },
  'aarch64-unknown-linux-musl': { os: 'linux', cpu: 'arm64' },
  'x86_64-apple-darwin': { os: 'darwin', cpu: 'x64' },
  'aarch64-apple-darwin': { os: 'darwin', cpu: 'arm64' },
  'x86_64-pc-windows-msvc': { os: 'win32', cpu: 'x64' },
  'aarch64-pc-windows-msvc': { os: 'win32', cpu: 'arm64' },
};

const artefacts = readdirSync(distDir);

function registryIntegrity(name, version) {
  try {
    const value = JSON.parse(execFileSync('npm', ['view', `${name}@${version}`, 'dist.integrity', '--json'], {
      encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'],
    }));
    if (typeof value !== 'string' || !value.startsWith('sha512-')) {
      throw new Error(`registry has no supported integrity for ${name}@${version}`);
    }
    return value;
  } catch (error) {
    // An unavailable registry or bad token is not evidence that a version is free.
    let body;
    try { body = JSON.parse(String(error.stdout)); } catch { /* fail closed below */ }
    if (body?.error?.code === 'E404') return null;
    throw error;
  }
}

function publish(dir) {
  const pkg = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'));
  // Publish the same tarball whose integrity was checked, not a second npm pack.
  const packed = JSON.parse(execFileSync('npm', ['pack', '--json', '--ignore-scripts'], {
    cwd: dir, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'],
  }));
  if (packed.length !== 1 || packed[0].filename.includes('/') || packed[0].filename.includes('\\')) {
    throw new Error('npm pack returned an unexpected tarball');
  }
  const tarball = join(dir, packed[0].filename);
  const integrity = 'sha512-' + createHash('sha512').update(readFileSync(tarball)).digest('base64');
  const existing = registryIntegrity(pkg.name, pkg.version);
  if (existing !== null) {
    if (existing !== integrity) throw new Error(`refusing to replace different bytes at ${pkg.name}@${pkg.version}`);
    console.log(`already published with matching integrity: ${pkg.name}@${pkg.version}`);
    return;
  }
  // `--access public`: scoped packages otherwise default to private.
  execFileSync('npm', ['publish', tarball, '--access', 'public', '--ignore-scripts'], { cwd: dir, stdio: 'inherit' });
  if (registryIntegrity(pkg.name, pkg.version) !== integrity) {
    throw new Error(`published integrity differs for ${pkg.name}@${pkg.version}; inspect the registry before retrying`);
  }
}

const published = [];
for (const [target, { os, cpu }] of Object.entries(TARGETS)) {
  const name = `cormex-${npmVersion}-${target}`;
  const archive = artefacts.find((f) => f.startsWith(name + '.'));
  if (!archive) {
    console.log(`skipping ${target}: no artefact named ${name}.*`);
    continue;
  }

  const dir = join(here, '.publish', `cli-${os}-${cpu}`);
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });

  // Extract the archive and take the binary out of it
  const tmp = join(here, '.publish', '_extract');
  rmSync(tmp, { recursive: true, force: true });
  mkdirSync(tmp, { recursive: true });
  if (archive.endsWith('.zip')) {
    execFileSync('unzip', ['-q', join(distDir, archive), '-d', tmp]);
  } else {
    execFileSync('tar', ['-xzf', join(distDir, archive), '-C', tmp]);
  }
  const inner = readdirSync(tmp)[0];
  const binary = join(tmp, inner, os === 'win32' ? 'cormex.exe' : 'cormex');
  cpSync(binary, join(dir, os === 'win32' ? 'cormex.exe' : 'cormex'));

  writeFileSync(
    join(dir, 'package.json'),
    JSON.stringify(
      {
        name: `@cormex/cli-${os}-${cpu}`,
        version: npmVersion,
        description: `The Cormex capture CLI binary for ${os}-${cpu}`,
        // These two fields are what make npm skip the package on the wrong
        // platform. Getting one wrong installs a binary that cannot run, and
        // the error is "exec format error" at runtime
        os: [os],
        cpu: [cpu],
        license: 'Apache-2.0',
        repository: { type: 'git', url: 'https://github.com/weironz/cormex' },
        files: [os === 'win32' ? 'cormex.exe' : 'cormex'],
      },
      null,
      2
    )
  );
  publish(dir);
  published.push(`@cormex/cli-${os}-${cpu}`);
  rmSync(tmp, { recursive: true, force: true });
}

if (published.length === 0) {
  console.error('no platform packages were published; refusing to publish the main package');
  process.exit(1);
}

// The main package last: it depends on the platform packages, and npm refuses
// to install a dependency that does not exist yet
const main = join(here, '.publish', 'main');
rmSync(main, { recursive: true, force: true });
mkdirSync(main, { recursive: true });
cpSync(join(here, 'bin'), join(main, 'bin'), { recursive: true });

const pkg = JSON.parse(readFileSync(join(here, 'package.json'), 'utf8'));
pkg.version = npmVersion;
for (const dep of Object.keys(pkg.optionalDependencies ?? {})) {
  pkg.optionalDependencies[dep] = npmVersion;
}
writeFileSync(join(main, 'package.json'), JSON.stringify(pkg, null, 2));
publish(main);

console.log(`published ${published.length} platform packages and cormex@${npmVersion}`);
