import { createHash } from 'node:crypto';
import { lstatSync, readFileSync, readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const checksumPath = '.devkit/runtime.sha256';
const hashPattern = /^[a-f0-9]{64}$/;
const versionPattern = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[a-z0-9][a-z0-9.-]*)?$/;
const slugPattern = /^[a-z][a-z0-9]*(-[a-z0-9]+)*$/;
const digest = (bytes) => createHash('sha256').update(bytes).digest('hex');
const fail = (message) => {
  throw new Error(message);
};
const record = (value, label) => {
  if (value === null || typeof value !== 'object' || Array.isArray(value))
    fail(`${label} must be an object`);
  return value;
};
const text = (value, label) => {
  if (typeof value !== 'string' || value.length === 0) fail(`${label} must be a nonempty string`);
  return value;
};
const slug = (value) => {
  if (!slugPattern.test(value) || value.length > 63) fail(`invalid registered app: ${value}`);
};

function relativePath(name) {
  text(name, 'managed path');
  if (
    !/^[A-Za-z0-9._/-]+$/.test(name) ||
    name
      .split('/')
      .some((part) => ['', '.', '..', '__proto__', 'prototype', 'constructor'].includes(part))
  )
    fail(`unsafe managed path: ${name}`);
  return name;
}

function managedPath(name) {
  relativePath(name);
  if (
    name.startsWith('.devkit/runtime/') ||
    name === checksumPath ||
    /^\.devkit\/npm\/[a-z0-9.-]+\.tgz$/.test(name) ||
    /^apps\/[a-z][a-z0-9-]*\/rust-toolchain\.toml$/.test(name)
  )
    return name;
  fail(`unsupported managed path: ${name}`);
}

function regularFile(root, name) {
  let current = root;
  const parts = relativePath(name).split('/');
  for (const [index, part] of parts.entries()) {
    current = join(current, part);
    const info = lstatSync(current);
    if (info.isSymbolicLink()) fail(`symlink in managed/input path: ${name}`);
    if (index < parts.length - 1 ? !info.isDirectory() : !info.isFile())
      fail(`expected regular file: ${name}`);
  }
  return current;
}

function fileIdentity(root, name) {
  const path = regularFile(root, name);
  return { sha256: digest(readFileSync(path)), mode: lstatSync(path).mode & 0o777 };
}

function jsonFile(root, name) {
  try {
    return JSON.parse(readFileSync(regularFile(root, name), 'utf8'));
  } catch (error) {
    fail(`cannot read ${name}: ${error.message}`);
  }
}

/** Read-only accepted-state consistency check; unknown noncritical fields survive. */
export function verifyManifest(root, { version, apps }) {
  root = resolve(root);
  const state = record(jsonFile(root, '.devkit/manifest.json'), 'project manifest');
  if (state.schemaVersion !== 1) fail('unsupported project schemaVersion');
  if (!versionPattern.test(text(state.version, 'version')) || state.version !== version)
    fail('manifest version differs from accepted toolchains.env');
  if (!hashPattern.test(text(state.releaseIdentity, 'releaseIdentity')))
    fail('invalid releaseIdentity');
  if (
    !/^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(
      text(state.generation, 'generation'),
    )
  )
    fail('invalid generation UUID');
  if (!['local', 'registry'].includes(state.packageMode)) fail('invalid packageMode');
  const managed = record(state.managed, 'managed files');
  for (const [name, value] of Object.entries(managed)) {
    managedPath(name);
    const expected = record(value, `identity of ${name}`);
    if (
      !hashPattern.test(expected.sha256) ||
      !Number.isInteger(expected.mode) ||
      expected.mode < 0 ||
      expected.mode > 0o777
    )
      fail(`invalid file identity: ${name}`);
    const actual = fileIdentity(root, name);
    if (actual.sha256 !== expected.sha256 || actual.mode !== expected.mode)
      fail(`managed file conflict: ${name}`);
  }
  for (const name of [
    checksumPath,
    ...[
      'devkit.mk',
      'devkit.sh',
      'guard.sh',
      'compose.tools.yaml',
      'toolchains.env',
      'verify-manifest.mjs',
    ].map((name) => `.devkit/runtime/${name}`),
  ]) {
    if (!Object.hasOwn(managed, name))
      fail(`manifest is missing required managed identity: ${name}`);
  }
  function runtimeFiles(directory, prefix) {
    for (const name of readdirSync(directory).sort()) {
      const relative = `${prefix}/${name}`;
      const path = join(directory, name);
      const info = lstatSync(path);
      if (info.isDirectory() && !info.isSymbolicLink()) runtimeFiles(path, relative);
      else if (!info.isFile() || info.isSymbolicLink() || !Object.hasOwn(managed, relative))
        fail(`unlisted or nonregular runtime file: ${relative}`);
    }
  }
  runtimeFiles(join(root, '.devkit/runtime'), '.devkit/runtime');
  const checksums = new Map();
  const checksumText = readFileSync(regularFile(root, checksumPath), 'utf8');
  for (const line of checksumText.split('\n')) {
    if (!line) continue;
    const match = /^([a-f0-9]{64})  ([A-Za-z0-9._/-]+)$/.exec(line);
    if (!match || checksums.has(match[2])) fail('invalid or duplicate runtime.sha256 record');
    const name = managedPath(match[2]);
    if (name === checksumPath || !Object.hasOwn(managed, name) || managed[name].sha256 !== match[1])
      fail(`checksum entry differs from manifest: ${name}`);
    checksums.set(name, match[1]);
  }
  if (Object.keys(managed).some((name) => name !== checksumPath && !checksums.has(name)))
    fail('runtime.sha256 omits a managed file');

  const registered = record(state.apps, 'registered apps');
  if (Object.keys(registered).length === 0) fail('project has no registered apps');
  for (const [name, value] of Object.entries(registered)) {
    slug(name);
    const app = record(value, `app ${name}`);
    if (
      !['node', 'rust'].includes(app.kind) ||
      !['node-app', 'react-app', 'rust-app'].includes(app.template) ||
      (app.kind === 'rust') !== (app.template === 'rust-app')
    )
      fail(`invalid app kind/template: ${name}`);
    const dependencies = record(app.toolDependencies, `tool dependencies of ${name}`);
    for (const [dependency, spec] of Object.entries(dependencies)) {
      if (!/^(?:@[a-z0-9-]+\/)?[a-z0-9][a-z0-9._-]*$/.test(dependency))
        fail(`invalid tool dependency: ${dependency}`);
      text(spec, dependency);
    }
    if (app.kind === 'rust') {
      if (
        Object.keys(dependencies).length !== 0 ||
        [
          'apps/' + name + '/rust-toolchain.toml',
          '.devkit/runtime/rust.Dockerfile',
          '.devkit/runtime/rust-check.sh',
        ].some((path) => !Object.hasOwn(managed, path))
      )
        fail(`invalid Rust managed tooling: ${name}`);
      regularFile(root, `apps/${name}/Cargo.toml`);
      regularFile(root, `apps/${name}/Cargo.lock`);
    } else {
      const manifest = record(
        jsonFile(root, `apps/${name}/package.json`),
        `package.json of ${name}`,
      );
      const actual =
        manifest.devDependencies === undefined
          ? {}
          : record(manifest.devDependencies, `devDependencies of ${name}`);
      for (const [dependency, spec] of Object.entries(dependencies)) {
        if (actual[dependency] !== spec)
          fail(`tool dependency differs from accepted manifest: ${name}/${dependency}`);
      }
      regularFile(root, `apps/${name}/package-lock.json`);
    }
  }
  for (const name of apps) {
    slug(name);
    if (!Object.hasOwn(registered, name))
      fail(`Make app is not registered in the release manifest: ${name}; use devkit register`);
    const node = (() => {
      try {
        regularFile(root, `apps/${name}/package.json`);
        return true;
      } catch {
        return false;
      }
    })();
    const rust = (() => {
      try {
        regularFile(root, `apps/${name}/Cargo.toml`);
        return true;
      } catch {
        return false;
      }
    })();
    if (node === rust || registered[name].kind !== (node ? 'node' : 'rust'))
      fail(`native app kind differs from registration: ${name}`);
  }
  const makeApps = new Set(apps);
  for (const name of readdirSync(join(root, 'apps')).sort()) {
    const directory = join(root, 'apps', name);
    const info = lstatSync(directory);
    if (info.isSymbolicLink()) fail(`symlink application directory is unsupported: apps/${name}`);
    if (!info.isDirectory()) continue;
    const native = ['package.json', 'Cargo.toml'].some((file) => {
      try {
        lstatSync(join(directory, file));
        return true;
      } catch (error) {
        if (error.code === 'ENOENT') return false;
        throw error;
      }
    });
    if (!native) continue;
    if (!Object.hasOwn(registered, name))
      fail(`native app is not registered: ${name}; use devkit register and add DEVKIT_APPS`);
    if (!makeApps.has(name))
      fail(
        `registered native app is missing from DEVKIT_APPS: ${name}; add the root Make declaration`,
      );
  }
  return state;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const [root, version, ...apps] = process.argv.slice(2);
    if (!root || !version || apps.length === 0)
      fail('usage: verify-manifest.mjs ROOT VERSION APP...');
    verifyManifest(root, { version, apps });
  } catch (error) {
    console.error(`devkit: ${error.message}`);
    process.exitCode = 1;
  }
}
