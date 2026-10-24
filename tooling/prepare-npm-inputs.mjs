#!/usr/bin/env node
// Author-only npm preparation. The consumer engine and release assembler are Rust.
import { createHash } from 'node:crypto';
import {
  chmodSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import {
  assertCanonicalLock,
  assertRegistryDependencies,
  assertSelectedIdentities,
  canonicalizeLock,
  exactVersion,
  hydrateLockIdentities,
  object,
  prepareTemplate,
} from './npm-inputs/locks.mjs';
import { npmEnvironment, runNpm, withEnvRegistry } from './npm-inputs/npm.mjs';

const templateNames = ['node-api', 'react-web', 'electron-react', 'react-client'];
const excluded = new Set(['node_modules', 'target', 'dist', '.artifacts', '.tmp', '.git']);
const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');
const sri = (bytes) => `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
const encode = (value) => `${JSON.stringify(value, null, 2)}\n`;
const json = (path) => JSON.parse(readFileSync(path, 'utf8'));

function stat(path) {
  try {
    return lstatSync(path);
  } catch (error) {
    if (error.code === 'ENOENT') {
      return null;
    }
    throw error;
  }
}

function noSymlinks(path) {
  let current = resolve(path);
  while (true) {
    const info = stat(current);
    if (info?.isSymbolicLink()) {
      throw new Error(`Refusing a symlink in input/output path: ${current}`);
    }
    if (current === dirname(current)) {
      break;
    }
    current = dirname(current);
  }
}

function beneath(root, path) {
  const suffix = relative(root, path);
  return !suffix || (suffix !== '..' && !suffix.startsWith(`..${sep}`) && !isAbsolute(suffix));
}

function outputPath(root, output) {
  noSymlinks(root);
  if (!stat(root)?.isDirectory()) {
    throw new Error('Source root must be an existing directory');
  }
  noSymlinks(output);
  if (stat(output)) {
    throw new Error('Output must be a new directory');
  }
  if (!stat(dirname(output))?.isDirectory()) {
    throw new Error('Output parent must already be a directory');
  }
  if (
    beneath(output, root) ||
    ['packages', 'templates', 'tooling'].some((name) => beneath(join(root, name), output))
  ) {
    throw new Error('Unsafe output overlaps source inputs');
  }
}

function regularBytes(path) {
  const info = lstatSync(path);
  if (!info.isFile() || info.isSymbolicLink()) {
    throw new Error(`Expected a regular input file: ${path}`);
  }
  return { bytes: readFileSync(path), mode: info.mode & 0o777 };
}

/** Snapshot actual bytes before staging, then re-read the same bounded source set. */
function sources(root) {
  const result = new Map();
  function read(name) {
    noSymlinks(join(root, name));
    result.set(name, regularBytes(join(root, name)));
  }
  function visit(name) {
    const path = join(root, name),
      info = lstatSync(path);
    if (info.isSymbolicLink()) {
      throw new Error(`Refusing source symlink: ${name}`);
    }
    if (info.isDirectory()) {
      for (const child of readdirSync(path).sort()) {
        if (!excluded.has(child)) {
          if (child === '.npmrc') {
            throw new Error('Package .npmrc is not an accepted npm preparation input');
          }
          visit(`${name}/${child}`);
        }
      }
    } else {
      read(name);
    }
  }
  read('VERSION');
  read('LICENSE');
  noSymlinks(join(root, 'packages/env'));
  visit('packages/env');
  for (const name of templateNames) {
    read(`templates/${name}/package.json`);
    read(`templates/${name}/package-lock.json`);
  }
  return result;
}

function signatures(snapshot) {
  return Object.fromEntries(
    [...snapshot]
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(([name, { bytes, mode }]) => [name, { sha256: sha256(bytes), mode }]),
  );
}

function write(path, bytes, mode = 0o644) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, bytes, { mode });
  chmodSync(path, mode);
}

function payloadFiles(root) {
  const files = new Map();
  function visit(path) {
    for (const entry of readdirSync(path, { withFileTypes: true }).sort((a, b) =>
      a.name < b.name ? -1 : 1,
    )) {
      const target = join(path, entry.name),
        name = relative(root, target).split(sep).join('/');
      if (entry.isSymbolicLink()) {
        throw new Error(`Symlink in npm inputs: ${name}`);
      }
      if (entry.isDirectory()) {
        visit(target);
      } else if (name !== 'inputs.json') {
        files.set(name, regularBytes(target));
      }
    }
  }
  visit(root);
  return signatures(files);
}

/** Match npm's reported name/version/file and SRI to the actual regular archive. */
export function validatePack(result, directory, version) {
  if (!Array.isArray(result) || result.length !== 1) {
    throw new Error('npm pack returned an unexpected result');
  }
  const entry = object(result[0], 'npm pack result');
  const expected = `docouno-env-${version}.tgz`;
  if (
    entry.name !== '@docouno/env' ||
    entry.version !== version ||
    entry.filename !== expected ||
    basename(entry.filename) !== entry.filename
  ) {
    throw new Error('npm pack returned a conflicting package identity/filename');
  }
  const path = join(directory, expected),
    { bytes } = regularBytes(path);
  const integrity = sri(bytes);
  if (
    entry.integrity !== integrity ||
    entry.shasum !== createHash('sha1').update(bytes).digest('hex') ||
    entry.size !== bytes.length
  ) {
    throw new Error('npm pack integrity/size differs from its actual archive');
  }
  if (JSON.stringify(readdirSync(directory).sort()) !== JSON.stringify([expected])) {
    throw new Error('npm pack produced unexpected extra files');
  }
  return { path, integrity, filename: expected };
}

/** The assembler can recheck this receipt immediately before using the bytes. */
export function verifyNpmInputs(directory) {
  noSymlinks(directory);
  const inputPath = join(directory, 'inputs.json');
  regularBytes(inputPath);
  const receipt = object(json(inputPath), 'npm inputs receipt');
  if (receipt.schemaVersion !== 1) {
    throw new Error('Unsupported npm inputs schemaVersion');
  }
  const version = exactVersion(receipt.version),
    environment = object(receipt.environment, 'npm environment');
  if (
    environment.name !== '@docouno/env' ||
    environment.version !== version ||
    environment.archive !== `npm/docouno-env-${version}.tgz`
  ) {
    throw new Error('npm inputs environment differs from the requested version');
  }
  if (JSON.stringify(payloadFiles(directory)) !== JSON.stringify(receipt.files)) {
    throw new Error('npm inputs file set/hash/mode differs from its receipt');
  }
  if (sri(readFileSync(join(directory, environment.archive))) !== environment.integrity) {
    throw new Error('npm inputs environment archive SRI mismatch');
  }
  const expectedFiles = [environment.archive];
  for (const name of templateNames) {
    const template = object(object(receipt.templates, 'npm templates')[name], name);
    if (
      template.manifest !== `templates/${name}/package.json` ||
      template.lock !== `templates/${name}/package-lock.json`
    ) {
      throw new Error(`Unexpected template snapshot paths: ${name}`);
    }
    expectedFiles.push(template.manifest, template.lock);
    const manifest = object(json(join(directory, template.manifest)), 'template manifest');
    const lock = json(join(directory, template.lock));
    assertCanonicalLock(lock);
    if (object(manifest.devDependencies, 'template devDependencies')['@docouno/env'] !== version) {
      throw new Error('Template accepted env version differs from npm inputs');
    }
    assertSelectedIdentities(lock, lock, version, environment.integrity);
  }
  if (JSON.stringify(Object.keys(receipt.files).sort()) !== JSON.stringify(expectedFiles.sort())) {
    throw new Error('Unexpected npm input files');
  }
  return receipt;
}

export async function prepareNpmInputs({
  root: rootInput,
  version: versionInput,
  output: outputInput,
  signal,
}) {
  const version = exactVersion(versionInput);
  if (
    typeof rootInput !== 'string' ||
    !rootInput ||
    typeof outputInput !== 'string' ||
    !outputInput
  ) {
    throw new Error('Explicit root and output paths are required');
  }
  const root = resolve(rootInput),
    output = resolve(outputInput);
  outputPath(root, output);
  signal?.throwIfAborted();
  const snapshot = sources(root),
    sourceSignatures = signatures(snapshot);
  const sourceJson = (name) => JSON.parse(snapshot.get(name).bytes.toString('utf8'));
  const sourceVersion = exactVersion(snapshot.get('VERSION').bytes.toString('utf8').trim());
  const envManifest = object(sourceJson('packages/env/package.json'), 'environment manifest');
  if (envManifest.name !== '@docouno/env') {
    throw new Error('Unexpected source environment package name');
  }
  const sourceEnvironmentVersion = exactVersion(envManifest.version);
  assertRegistryDependencies(envManifest);
  envManifest.version = version;
  const stage = mkdtempSync(join(dirname(output), '.npm-inputs-'));
  const deadline = new AbortController();
  const timer = setTimeout(
    () => deadline.abort(new Error('npm preparation exceeded 10 minutes')),
    600_000,
  );
  signal = signal ? AbortSignal.any([signal, deadline.signal]) : deadline.signal;
  try {
    // Stop npm's project-config search here, even if output is inside a checkout.
    write(join(stage, 'package.json'), encode({ private: true }));
    const payload = join(stage, 'payload'),
      envRoot = join(stage, 'environment');
    for (const [name, { bytes, mode }] of snapshot) {
      if (name.startsWith('packages/env/')) {
        write(join(envRoot, name.slice('packages/env/'.length)), bytes, mode);
      }
    }
    write(join(envRoot, 'package.json'), encode(envManifest));
    if (!existsSync(join(envRoot, 'LICENSE'))) {
      write(join(envRoot, 'LICENSE'), snapshot.get('LICENSE').bytes);
    }
    const npmRoot = join(payload, 'npm');
    mkdirSync(npmRoot, { recursive: true });
    const npmDirectory = join(stage, 'npm');
    const packed = await runNpm(
      ['pack', '--ignore-scripts', '--json', '--pack-destination', npmRoot],
      {
        cwd: envRoot,
        env: npmEnvironment(npmDirectory),
        signal,
      },
    );
    const archive = validatePack(JSON.parse(packed.stdout), npmRoot, version);
    chmodSync(archive.path, 0o644);
    const templates = {};
    await withEnvRegistry(envManifest, archive.path, async (registry) => {
      const env = npmEnvironment(npmDirectory, registry);
      const metadata = new Map();
      const fetchDist = async (name, selected) => {
        const key = `${name}@${selected}`;
        if (!metadata.has(key)) {
          const result = await runNpm(['view', key, 'dist', '--json', '--prefer-offline'], {
            cwd: stage,
            env,
            signal,
          });
          metadata.set(key, JSON.parse(result.stdout));
        }
        return metadata.get(key);
      };
      for (const name of templateNames) {
        signal?.throwIfAborted();
        const template = prepareTemplate(
          sourceJson(`templates/${name}/package.json`),
          sourceJson(`templates/${name}/package-lock.json`),
          sourceEnvironmentVersion,
          version,
        );
        const seeded = await hydrateLockIdentities(template.lock, fetchDist);
        const appRoot = join(payload, 'templates', name);
        const paths = {
          manifest: `templates/${name}/package.json`,
          lock: `templates/${name}/package-lock.json`,
        };
        write(join(payload, paths.manifest), encode(template.manifest));
        write(join(payload, paths.lock), encode(seeded));
        await runNpm(
          [
            'install',
            '--package-lock-only',
            '--ignore-scripts',
            '--include=dev',
            '--strict-peer-deps',
          ],
          { cwd: appRoot, env, signal },
        );
        const canonical = canonicalizeLock(json(join(payload, paths.lock)), registry);
        assertSelectedIdentities(seeded, canonical, version, archive.integrity);
        if (readFileSync(join(payload, paths.manifest), 'utf8') !== encode(template.manifest)) {
          throw new Error(`npm changed the template manifest: ${name}`);
        }
        write(join(payload, paths.lock), encode(canonical));
        templates[name] = paths;
      }
    });
    const receipt = {
      schemaVersion: 1,
      version,
      sourceVersion,
      sourceEnvironmentVersion,
      environment: {
        name: '@docouno/env',
        version,
        archive: `npm/${archive.filename}`,
        integrity: archive.integrity,
      },
      templates,
      sources: sourceSignatures,
      files: payloadFiles(payload),
    };
    write(join(payload, 'inputs.json'), encode(receipt));
    verifyNpmInputs(payload);
    signal?.throwIfAborted();
    if (JSON.stringify(signatures(sources(root))) !== JSON.stringify(sourceSignatures)) {
      throw new Error('Source inputs changed during npm preparation');
    }
    outputPath(root, output);
    renameSync(payload, output);
    return receipt;
  } finally {
    clearTimeout(timer);
    rmSync(stage, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const controller = new AbortController();
  const interrupt = () => controller.abort(new Error('npm preparation cancelled'));
  process.once('SIGINT', interrupt);
  process.once('SIGTERM', interrupt);
  try {
    const { values } = parseArgs({
      options: {
        root: { type: 'string' },
        version: { type: 'string' },
        output: { type: 'string' },
      },
      strict: true,
      allowPositionals: false,
    });
    const receipt = await prepareNpmInputs({ ...values, signal: controller.signal });
    process.stdout.write(`${JSON.stringify(receipt)}\n`);
  } catch (error) {
    process.stderr.write(`prepare-npm-inputs: ${error.message}\n`);
    process.exitCode = controller.signal.aborted ? 130 : 1;
  } finally {
    process.removeListener('SIGINT', interrupt);
    process.removeListener('SIGTERM', interrupt);
  }
}
