import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { gunzipSync } from 'node:zlib';
import { prepareNpmInputs, validatePack, verifyNpmInputs } from '../tooling/prepare-npm-inputs.mjs';
import {
  assertCanonicalLock,
  assertManifestImporter,
  encodeLock,
  parseLock,
  assertSelectedIdentities,
  canonicalizeLock,
  exactVersion,
  hydrateLockIdentities,
  prepareTemplate,
  registryUrl,
} from '../tooling/npm-inputs/locks.mjs';
import { npmEnvironment, runPnpm, withEnvRegistry } from '../tooling/npm-inputs/npm.mjs';

const names = ['node-api', 'react-web', 'electron-react', 'react-client', 'ts-library', 'react-ui'];
const integrity = `sha512-${Buffer.alloc(64, 7).toString('base64')}`;
const identity = (name, version = '1.2.3') => ({
  resolution: { tarball: registryUrl(name, version), integrity },
});
const lock = (packages = {}, importer = {}) => ({
  lockfileVersion: '9.0',
  settings: { autoInstallPeers: true, excludeLinksFromLockfile: false },
  importers: { '.': importer },
  packages,
  snapshots: Object.fromEntries(Object.keys(packages).map((key) => [key, {}])),
});
const dep = (specifier, version = specifier) => ({ specifier, version });

function temporary(t) {
  const root = mkdtempSync(join(tmpdir(), 'devkit-npm-inputs-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  return root;
}

function put(path, value) {
  mkdirSync(join(path, '..'), { recursive: true });
  writeFileSync(path, typeof value === 'string' ? value : `${JSON.stringify(value, null, 2)}\n`);
}

function fixture(t) {
  const parent = temporary(t),
    root = join(parent, 'source with spaces');
  put(join(root, 'VERSION'), '0.4.1\n');
  put(join(root, 'LICENSE'), 'MIT test fixture\n');
  put(join(root, 'packages/env/package.json'), {
    name: '@docouno/env',
    version: '0.4.1',
    type: 'module',
    files: ['index.js'],
    scripts: {
      prepack: 'node lifecycle-must-not-run.js',
      prepare: 'node lifecycle-must-not-run.js',
    },
  });
  put(join(root, 'packages/env/index.js'), 'export const version = "source";\n');
  for (const name of names) {
    const manifest = {
      name: `fixture-${name}`,
      version: '1.0.0',
      private: true,
      devDependencies: { '@docouno/env': '0.4.1' },
    };
    put(join(root, `templates/${name}/package.json`), manifest);
    put(
      join(root, `templates/${name}/pnpm-lock.yaml`),
      encodeLock(
        lock(
          { '@docouno/env@0.4.1': identity('@docouno/env', '0.4.1') },
          { devDependencies: { '@docouno/env': dep('0.4.1') } },
        ),
      ),
    );
  }
  return { root, parent, output: join(parent, 'prepared with spaces') };
}

test('release authority must be one exact SemVer, including numeric prerelease rules', () => {
  for (const version of ['0.5.0', '0.5.0-rc.1', '1.2.3+Build.123', '1.2.3-rc-A']) {
    assert.equal(exactVersion(version), version);
  }
  for (const version of [
    undefined,
    '',
    'latest',
    '^0.5.0',
    'v0.5.0',
    '0.5',
    '00.5.0',
    '1.2.3-01',
    '1.2.3-rc..1',
    '1.2.3\n',
    '1.2.3;echo x',
  ]) {
    assert.throws(() => exactVersion(version), /Invalid exact version/);
  }
});

test('unsafe output and symlinked inputs fail before npm or staging', async (t) => {
  const f = fixture(t);
  const options = { root: f.root, version: '0.5.0', output: f.output };
  await assert.rejects(prepareNpmInputs({ ...options, version: '^0.5.0' }), /Invalid exact/);
  for (const output of [
    f.root,
    join(f.root, 'templates/new'),
    join(f.root, 'packages/new'),
    join(f.root, 'tooling/new'),
  ]) {
    mkdirSync(join(output, '..'), { recursive: true });
    await assert.rejects(prepareNpmInputs({ ...options, output }), /new directory|overlaps/);
  }
  mkdirSync(f.output);
  put(join(f.output, 'keep'), 'existing data');
  await assert.rejects(prepareNpmInputs(options), /new directory/);
  assert.equal(readFileSync(join(f.output, 'keep'), 'utf8'), 'existing data');
  const alias = join(f.parent, 'alias');
  symlinkSync(f.root, alias);
  await assert.rejects(
    prepareNpmInputs({ ...options, root: alias, output: join(f.parent, 'other') }),
    /symlink/,
  );
  rmSync(join(f.root, 'packages/env/index.js'));
  symlinkSync(join(f.root, 'LICENSE'), join(f.root, 'packages/env/index.js'));
  await assert.rejects(
    prepareNpmInputs({ ...options, output: join(f.parent, 'other') }),
    /symlink/,
  );
  assert.equal(
    readdirSync(f.parent).some((name) => name.startsWith('.npm-inputs-')),
    false,
  );
});

test('pack result is checked against actual bytes, filename, package/version, size and SHA512', (t) => {
  const root = temporary(t),
    version = '0.5.0',
    filename = `docouno-env-${version}.tgz`;
  const bytes = Buffer.from('unit identity bytes, not an npm pack integration proof');
  writeFileSync(join(root, filename), bytes);
  const record = {
    name: '@docouno/env',
    version,
    filename,
    size: bytes.length,
    integrity: `sha512-${createHash('sha512').update(bytes).digest('base64')}`,
    shasum: createHash('sha1').update(bytes).digest('hex'),
  };
  assert.equal(validatePack([record], root, version).filename, filename);
  for (const replacement of [
    { version: '0.4.1' },
    { name: '@docouno/other' },
    { filename: '../escape.tgz' },
    { filename: 'env.tgz' },
    { integrity },
    { size: 0 },
  ]) {
    assert.throws(
      () => validatePack([{ ...record, ...replacement }], root, version),
      /conflicting|integrity/,
    );
  }
  writeFileSync(join(root, filename), Buffer.from('corrupted'));
  assert.throws(() => validatePack([record], root, version), /actual archive/);
  rmSync(join(root, filename));
  put(join(root, 'archive'), bytes.toString());
  symlinkSync(join(root, 'archive'), join(root, filename));
  assert.throws(() => validatePack([record], root, version), /regular input file/);
});

test('hydration fills only missing SRI at already selected versions', async () => {
  const stable = identity('stable', '7.8.9');
  const input = lock({ 'stable@7.8.9': stable, 'zod@1.2.3': { resolution: {} } });
  const calls = [];
  const hydrated = await hydrateLockIdentities(input, async (name, version) => {
    calls.push(`${name}@${version}`);
    return { tarball: registryUrl(name, version), integrity };
  });
  assert.deepEqual(calls, ['zod@1.2.3']);
  assert.deepEqual(hydrated.packages['stable@7.8.9'], stable);
  assert.deepEqual(input.packages['zod@1.2.3'], { resolution: {} });
  assertCanonicalLock(hydrated);
});

test('nonexact, local, mismatched, alias YAML and malformed identities fail closed', async () => {
  for (const [key, entry] of [
    ['zod@^1.2.3', { resolution: {} }],
    ['zod@1.2.3', { resolution: { tarball: 'file:source.tgz' } }],
    ['zod@1.2.3', { resolution: { tarball: registryUrl('zod', '1.2.4') } }],
    ['zod@1.2.3', { resolution: { integrity: 'sha512-broken' } }],
    ['zod@1.2.3', { resolution: { directory: '../source' } }],
    ['../zod@1.2.3', { resolution: {} }],
  ]) {
    let fetched = false;
    await assert.rejects(
      hydrateLockIdentities(lock({ [key]: entry }), async () => {
        fetched = true;
        return {};
      }),
    );
    assert.equal(fetched, false);
  }
  for (const dist of [
    {},
    { tarball: registryUrl('zod', '1.2.4'), integrity },
    { tarball: registryUrl('zod', '1.2.3'), integrity: 'broken' },
  ]) {
    await assert.rejects(
      hydrateLockIdentities(lock({ 'zod@1.2.3': { resolution: {} } }), async () => dist),
      /Unsupported registry/,
    );
  }
  assert.throws(() => parseLock('a: &anchor [1]\nb: *anchor\n'), /alias|Alias/);
  assert.throws(() => parseLock('a: 1\na: 2\n'), /unique|unique keys/);
});

function templateFixture() {
  const manifest = {
    name: 'app',
    version: '9.8.7',
    devDependencies: { '@docouno/env': '0.4.1' },
    dependencies: { zod: '^1.2.3' },
    peerDependencies: { zod: '>=1.0.0' },
    custom: { untouched: true },
  };
  const input = lock(
    { '@docouno/env@0.4.1': identity('@docouno/env', '0.4.1'), 'zod@1.2.3': identity('zod') },
    {
      dependencies: { zod: dep('^1.2.3', '1.2.3') },
      devDependencies: { '@docouno/env': dep('0.4.1') },
    },
  );
  return { manifest, input };
}
test('templates replace only env, preserve peer contexts and resolve peers through physical importer fields', () => {
  const { manifest, input } = templateFixture();
  const next = prepareTemplate(manifest, input, '0.4.1', '0.5.0');
  assert.deepEqual(next.manifest, { ...manifest, devDependencies: { '@docouno/env': '0.5.0' } });
  assert.deepEqual(next.lock.packages['@docouno/env@0.5.0'].resolution, {});
  assert.deepEqual(next.lock.packages['zod@1.2.3'], input.packages['zod@1.2.3']);
  assert.equal(manifest.devDependencies['@docouno/env'], '0.4.1');
  assertManifestImporter(next.manifest, next.lock);
  for (const spec of [
    'file:../zod',
    'workspace:*',
    'https://example.test/x.tgz',
    'npm:pkg@file:../x',
    'npm:a@npm:b@1.0.0',
    'npm:',
    'npm:pkg@',
  ]) {
    assert.throws(
      () => prepareTemplate({ ...manifest, dependencies: { zod: spec } }, input, '0.4.1', '0.5.0'),
      /Source\/local|alias/,
    );
  }
  assert.throws(
    () =>
      prepareTemplate({ ...manifest, dependencies: { zod: '^2.0.0' } }, input, '0.4.1', '0.5.0'),
    /disagree/,
  );
  assert.throws(() => prepareTemplate(manifest, input, '0.4.0', '0.5.0'), /exact source/);
  assert.throws(
    () => assertManifestImporter({ ...manifest, peerDependencies: { missing: '*' } }, input),
    /peer resolution/,
  );
});

test('canonicalization removes only the fixture origin and rejects external selection drift', () => {
  const { manifest, input } = templateFixture();
  const { lock: seeded } = prepareTemplate(manifest, input, '0.4.1', '0.5.0');
  seeded.packages['@docouno/env@0.5.0'].resolution = { integrity };
  const next = structuredClone(seeded),
    registry = 'http://127.0.0.1:4321/';
  next.packages['@docouno/env@0.5.0'].resolution.tarball =
    `${registry}@docouno/env/-/env-0.5.0.tgz`;
  const canonical = canonicalizeLock(next, registry);
  assert.equal(canonical.packages['@docouno/env@0.5.0'].resolution.tarball, undefined);
  assertSelectedIdentities(seeded, canonical, '0.5.0', integrity);
  const changed = structuredClone(canonical);
  changed.packages['zod@1.2.3'].resolution.integrity =
    `sha512-${Buffer.alloc(64, 8).toString('base64')}`;
  assert.throws(
    () => assertSelectedIdentities(seeded, changed, '0.5.0', integrity),
    /selected external/,
  );
  delete changed.packages['zod@1.2.3'];
  assert.throws(
    () => assertSelectedIdentities(seeded, changed, '0.5.0', integrity),
    /external identities/,
  );
  next.packages['zod@1.2.3'].resolution.tarball = 'http://localhost:123/zod.tgz';
  assert.throws(() => canonicalizeLock(next, registry), /identity conflict/);
});

test('npm aliases resolve by target identity and cannot redirect the selected snapshot', async () => {
  const input = lock(
    { 'string-width@4.2.3': { resolution: {} }, '@acme/core@1.0.2': { resolution: {} } },
    {
      dependencies: {
        'string-width-cjs': dep('npm:string-width@^4.2.0', 'string-width@4.2.3'),
        'scoped-alias': dep('npm:@acme/core@^1.0.0', '@acme/core@1.0.2'),
      },
    },
  );
  const calls = [];
  const hydrated = await hydrateLockIdentities(input, async (name, version) => {
    calls.push(`${name}@${version}`);
    return { tarball: registryUrl(name, version), integrity };
  });
  assert.deepEqual(calls, ['string-width@4.2.3', '@acme/core@1.0.2']);
  assertCanonicalLock(hydrated);
  hydrated.importers['.'].dependencies['string-width-cjs'].version = 'evil@4.2.3';
  assert.throws(
    () => assertCanonicalLock(hydrated),
    /Missing canonical snapshot|mismatched package/,
  );
});

test('npm environment strips inherited npm and Node injection and isolates both configs', (t) => {
  const directory = temporary(t);
  const env = npmEnvironment(directory, undefined, {
    PATH: '/bin',
    NPM_TOKEN: 'secret',
    npm_config_registry: 'http://evil',
    PNPM_CONFIG_REGISTRY: 'http://evil',
    pnpm_injection: 'evil',
    NODE_OPTIONS: '--import evil',
    NODE_PATH: '/evil',
    NODE_EXTRA_CA_CERTS: '/evil',
    HOME: '/private',
    ordinary: 'retained',
  });
  assert.equal(env.NPM_TOKEN, undefined);
  assert.equal(env.PNPM_CONFIG_REGISTRY, 'https://registry.npmjs.org/');
  assert.equal(env.pnpm_injection, undefined);
  assert.equal(env.NODE_OPTIONS, undefined);
  assert.equal(env.NODE_PATH, undefined);
  assert.equal(env.NODE_EXTRA_CA_CERTS, undefined);
  assert.equal(env.npm_config_registry, 'https://registry.npmjs.org/');
  assert.equal(env.HOME, directory);
  assert.equal(env.ordinary, 'retained');
  assert.notEqual(env.npm_config_userconfig, env.npm_config_globalconfig);
  assert.equal(readFileSync(env.npm_config_userconfig, 'utf8'), '');
  assert.equal(readFileSync(env.npm_config_globalconfig, 'utf8'), '');
});

test('bounded registry serves those exact bytes, rejects own-scope misses and closes on failure', async (t) => {
  const directory = temporary(t),
    archive = join(directory, 'env.tgz'),
    bytes = Buffer.from('transport unit bytes');
  writeFileSync(archive, bytes);
  let origin;
  await assert.rejects(
    withEnvRegistry({ name: '@docouno/env', version: '0.5.0' }, archive, async (registry) => {
      origin = registry;
      const response = await fetch(`${registry}@docouno%2fenv`);
      const metadata = await response.json();
      const dist = metadata.versions['0.5.0'].dist;
      assert.deepEqual(Buffer.from(await (await fetch(dist.tarball)).arrayBuffer()), bytes);
      assert.equal(dist.integrity, `sha512-${createHash('sha512').update(bytes).digest('base64')}`);
      assert.equal((await fetch(`${registry}@docouno%2fmissing`)).status, 404);
      const publicRequest = await fetch(`${registry}is-number`, { redirect: 'manual' });
      assert.equal(publicRequest.headers.get('location'), 'https://registry.npmjs.org/is-number');
      assert.equal((await fetch(registry, { method: 'POST' })).status, 405);
      throw new Error('deliberate action failure');
    }),
    /deliberate action/,
  );
  await assert.rejects(fetch(origin), /fetch failed/);
});

// Real npm is deliberately opt-in: CI/author runs this inside the pinned Node image.
const npmTest = (name, fn) => test(name, { skip: process.env.DEVKIT_TEST_NPM !== '1' }, fn);

function tarFile(archive, filename) {
  const bytes = gunzipSync(archive);
  for (let offset = 0; offset + 512 < bytes.length;) {
    const name = bytes
      .subarray(offset, offset + 100)
      .toString()
      .replace(/\0.*$/, '');
    const size = parseInt(
      bytes
        .subarray(offset + 124, offset + 136)
        .toString()
        .replace(/\0.*$/, '')
        .trim() || '0',
      8,
    );
    if (name === filename) {
      return bytes.subarray(offset + 512, offset + 512 + size);
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  throw new Error(`File absent from npm tar: ${filename}`);
}

npmTest(
  'real npm preparation stages exact archive/template bytes, leaves sources intact and detects corruption',
  async (t) => {
    const f = fixture(t);
    const original = readFileSync(join(f.root, 'packages/env/package.json'), 'utf8');
    const receipt = await prepareNpmInputs({ ...f, version: '0.5.0' });
    assert.equal(receipt.sourceVersion, '0.4.1');
    assert.equal(receipt.version, '0.5.0');
    assert.deepEqual(verifyNpmInputs(f.output), receipt);
    const archive = join(f.output, receipt.environment.archive),
      bytes = readFileSync(archive);
    assert.equal(JSON.parse(tarFile(bytes, 'package/package.json')).version, '0.5.0');
    assert.equal(
      tarFile(bytes, 'package/index.js').toString(),
      'export const version = "source";\n',
    );
    assert.equal(
      receipt.environment.integrity,
      `sha512-${createHash('sha512').update(bytes).digest('base64')}`,
    );
    assert.equal(readFileSync(join(f.root, 'packages/env/package.json'), 'utf8'), original);
    assert.equal(
      readdirSync(f.parent).some((name) => name.startsWith('.npm-inputs-')),
      false,
    );
    writeFileSync(archive, 'corrupted archive');
    assert.throws(() => verifyNpmInputs(f.output), /file set\/hash\/mode/);
    writeFileSync(archive, bytes);
    chmodSync(archive, 0o600);
    assert.throws(() => verifyNpmInputs(f.output), /file set\/hash\/mode/);
    chmodSync(archive, 0o644);
    put(join(f.output, 'unexpected'), 'extra');
    assert.throws(() => verifyNpmInputs(f.output), /file set\/hash\/mode/);
  },
);

npmTest('native npm failure and input changes publish no output and clean staging', async (t) => {
  const f = fixture(t);
  const manifestPath = join(f.root, 'packages/env/package.json');
  const manifest = JSON.parse(readFileSync(manifestPath));
  manifest.peerDependencies = { '@docouno/missing': '1.0.0' };
  put(manifestPath, manifest);
  await assert.rejects(prepareNpmInputs({ ...f, version: '0.5.0' }), /pnpm install failed/);
  assert.equal(existsSync(f.output), false);
  assert.equal(
    readdirSync(f.parent).some((name) => name.startsWith('.npm-inputs-')),
    false,
  );
  delete manifest.peerDependencies;
  put(manifestPath, manifest);
  const pending = prepareNpmInputs({ ...f, version: '0.5.0' });
  put(join(f.root, 'packages/env/index.js'), 'changed while npm runs\n');
  await assert.rejects(pending, /Source inputs changed/);
  assert.equal(existsSync(f.output), false);
  assert.equal(
    readdirSync(f.parent).some((name) => name.startsWith('.npm-inputs-')),
    false,
  );
  assert.equal(
    readFileSync(join(f.root, 'packages/env/index.js'), 'utf8'),
    'changed while npm runs\n',
  );
});

npmTest('cancellation reaps native npm before staging cleanup', async (t) => {
  const directory = temporary(t),
    controller = new AbortController();
  const pending = runPnpm(['--version'], {
    cwd: directory,
    env: npmEnvironment(directory),
    signal: controller.signal,
  });
  controller.abort(new Error('test cancellation'));
  await assert.rejects(pending, /test cancellation/);
  const f = fixture(t),
    abort = new AbortController();
  const preparing = prepareNpmInputs({ ...f, version: '0.5.0', signal: abort.signal });
  abort.abort(new Error('test prepare cancellation'));
  await assert.rejects(preparing, /test prepare cancellation/);
  assert.equal(existsSync(f.output), false);
  assert.equal(
    readdirSync(f.parent).some((name) => name.startsWith('.npm-inputs-')),
    false,
  );
});
