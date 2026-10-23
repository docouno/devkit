import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { fixture, put, release, seal } from './helpers/project-fixture.ts';
import { copyTree, digest, fileMap, identity, json, object, writeJson } from '../tooling/files.ts';
import { initProject } from '../tooling/project.ts';
import {
  addProjectComponent,
  recoverProject,
  registerProjectComponent,
  updateProject,
} from '../tooling/project-update.ts';
import { captureComposeInputs } from '../tooling/compose-inputs.ts';
import {
  assertNativeComponents,
  collectNativeInputClosure,
  deriveComponentDependencies,
} from '../tooling/native-workspace.ts';
import { regenerateWorkspaceLock } from '../tooling/native-graph.ts';
import {
  assertCompleteLockIdentities,
  hydrateLockIdentities,
  normalizeLegacyLockIdentities,
} from '../tooling/lock-identities.ts';
import { projectState, sourceSignature } from '../tooling/project-state.ts';
import type { ProjectStateV1 } from '../tooling/types.ts';

function composableRelease(root: string, version: string): string {
  const result = release(root, version);
  for (const name of ['react-client', 'electron-react']) {
    copyTree(join(result, 'templates/node-api'), join(result, 'templates', name));
    const manifest = object(json(join(result, 'templates', name, 'package.json')), 'manifest');
    if (name === 'react-client') {
      manifest.exports = { '.': './src/index.ts' };
      delete object(manifest.scripts, 'scripts').dev;
    }
    writeJson(join(result, 'templates', name, 'package.json'), manifest);
    const lock = object(json(join(result, 'templates', name, 'package-lock.json')), 'lock');
    object(lock.packages, 'packages')[''] = manifest;
    writeJson(join(result, 'templates', name, 'package-lock.json'), lock);
  }
  seal(result, version);
  return result;
}

await test('add-app integrates an edited consumer; collisions and inventory races write nothing', async () => {
  const f = fixture();
  try {
    const r = composableRelease(f.root, '0.2.0'),
      target = join(f.root, 'edited consumer');
    await initProject(r, target, {
      apps: [{ name: 'web', template: 'react-web' }],
      packageMode: 'local',
    });
    put(target, 'apps/web/src/user.ts', 'export const preserved = true;\n');
    put(
      target,
      'Makefile',
      'include .devkit/runtime/devkit.mk\n# user edge\nbuild-web: my-step\nmy-step:\n\t@true\n',
    );
    const make = readFileSync(join(target, 'Makefile'));
    const inventory = captureComposeInputs(target);
    inventory.services = ['web'];
    const next = await addProjectComponent(
      r,
      target,
      'app',
      { name: 'api', template: 'rust-api' },
      { composeInventory: inventory },
    );
    assert.ok(next.components['apps/api']);
    assert.deepEqual(readFileSync(join(target, 'Makefile')), make);
    assert.match(readFileSync(join(target, 'apps/web/src/user.ts'), 'utf8'), /preserved/);
    assert.match(readFileSync(join(target, '.devkit/project.mk'), 'utf8'), /api web/);
    const before = sourceSignature(target, next);
    await assert.rejects(
      addProjectComponent(
        r,
        target,
        'app',
        { name: 'api', template: 'rust-api' },
        { composeInventory: captureComposeInputs(target) },
      ),
      /already exists/,
    );
    assert.equal(sourceSignature(target, next), before);
    const stale = captureComposeInputs(target);
    put(target, 'compose.yaml', 'services:\n  desktop:\n    image: busybox\n');
    const edited = sourceSignature(target, next);
    await assert.rejects(
      addProjectComponent(
        r,
        target,
        'app',
        { name: 'desktop', template: 'electron-react' },
        { composeInventory: stale },
      ),
      /Compose admitted inputs changed/,
    );
    assert.equal(sourceSignature(target, next), edited);
    assert.equal(existsSync(join(target, 'apps/desktop')), false);
  } finally {
    f.close();
  }
});

await test('new-component rollback checks unknown source before any restore', async () => {
  const f = fixture();
  try {
    const r = composableRelease(f.root, '0.2.0'),
      target = join(f.root, 'recover');
    await initProject(r, target, { template: 'node', packageMode: 'local' });
    const before = sourceSignature(target, projectState(target));
    await assert.rejects(
      addProjectComponent(
        r,
        target,
        'app',
        { name: 'extra', template: 'rust-api' },
        {
          composeInventory: captureComposeInputs(target),
          hooks: {
            afterWrite() {
              if (existsSync(join(target, 'apps/extra/Cargo.toml'))) {
                put(target, 'apps/extra/unknown.txt', 'keep this');
                throw Error('injected interruption');
              }
            },
          },
        },
      ),
      /automatic rollback was blocked/,
    );
    const partial = sourceSignature(target, projectState(target));
    assert.throws(() => recoverProject(target), /unknown file/);
    assert.equal(sourceSignature(target, projectState(target)), partial);
    rmSync(join(target, 'apps/extra/unknown.txt'));
    assert.equal(recoverProject(target), true);
    assert.equal(sourceSignature(target, projectState(target)), before);
    assert.equal(existsSync(join(target, 'apps/extra')), false);
  } finally {
    f.close();
  }
});

await test('native receipt includes transitive nonmember manifest, optional config/lock and tarball bytes', async () => {
  const f = fixture();
  try {
    const r = release(f.root, '0.2.0'),
      target = join(f.root, 'inputs');
    await initProject(r, target, { template: 'node', packageMode: 'local' });
    put(
      target,
      'shared/one/package.json',
      JSON.stringify({
        name: 'local-one',
        version: '1.0.0',
        dependencies: { 'local-two': 'file:../two' },
      }),
    );
    put(target, 'shared/two/package.json', JSON.stringify({ name: 'local-two', version: '1.0.0' }));
    const manifest = object(json(join(target, 'apps/main/package.json')), 'manifest');
    manifest.dependencies = { 'local-one': 'file:../../shared/one' };
    writeJson(join(target, 'apps/main/package.json'), manifest);
    await regenerateWorkspaceLock(target);
    const state = projectState(target),
      baseline = collectNativeInputClosure(target, state);
    assert.ok(Object.hasOwn(baseline.files, 'shared/two/.npmrc'));
    assert.ok(Object.hasOwn(baseline.files, 'shared/two/package-lock.json'));
    put(target, 'shared/two/.npmrc', 'fund=false\n');
    assert.notEqual(collectNativeInputClosure(target, state).digest, baseline.digest);
    rmSync(join(target, 'shared/two/.npmrc'));
    put(target, 'shared/two/package-lock.json', '{}');
    assert.notEqual(collectNativeInputClosure(target, state).digest, baseline.digest);
    rmSync(join(target, 'shared/two/package-lock.json'));
    const archive = join(target, '.devkit/npm/docouno-env-0.2.0.tgz');
    const bytes = readFileSync(archive);
    writeFileSync(archive, Buffer.concat([bytes, Buffer.from([0])]));
    assert.notEqual(collectNativeInputClosure(target, state).digest, baseline.digest);
    writeFileSync(archive, bytes);
    assert.equal(collectNativeInputClosure(target, state).digest, baseline.digest);
  } finally {
    f.close();
  }
});

await test('actual lock target distinguishes same-name registry dependency from workspace library', async () => {
  const f = fixture();
  try {
    const r = composableRelease(f.root, '0.2.0'),
      target = join(f.root, 'actual target');
    const state = await initProject(r, target, { template: 'node', packageMode: 'local' });
    state.components['packages/debug'] = {
      role: 'library',
      toolchain: 'node',
      toolingProfile: 'react-library',
      origin: { template: 'react-client', version: '0.2.0' },
      toolDependencies: {},
      dependencies: [],
    };
    put(
      target,
      'packages/debug/package.json',
      JSON.stringify({ name: 'debug', version: '999.0.0', exports: './index.js' }),
    );
    const manifest = object(json(join(target, 'apps/main/package.json')), 'manifest');
    manifest.dependencies = { debug: '^4.0.0' };
    writeJson(join(target, 'apps/main/package.json'), manifest);
    const lock = object(json(join(target, 'package-lock.json')), 'lock'),
      packages = object(lock.packages, 'packages');
    packages['packages/debug'] = { name: 'debug', version: '999.0.0' };
    packages['node_modules/debug'] = { link: true, resolved: 'packages/debug' };
    packages['apps/main/node_modules/debug'] = {
      version: '4.3.4',
      resolved: 'https://registry.npmjs.org/debug/-/debug-4.3.4.tgz',
      integrity: 'sha512-fixture',
    };
    writeJson(join(target, 'package-lock.json'), lock);
    assert.deepEqual(deriveComponentDependencies(target, state)['apps/main'], []);
    delete packages['apps/main/node_modules/debug'];
    writeJson(join(target, 'package-lock.json'), lock);
    assert.deepEqual(deriveComponentDependencies(target, state)['apps/main'], ['packages/debug']);
  } finally {
    f.close();
  }
});

await test('schema1 local Node consumer migrates transactionally without touching product Make/Compose', async () => {
  const f = fixture();
  try {
    const r1 = release(f.root, '0.1.0'),
      r2 = release(f.root, '0.2.0'),
      target = join(f.root, 'legacy');
    copyTree(join(r1, 'templates/node-api'), join(target, 'apps/main'));
    copyTree(join(r1, 'runtime'), join(target, '.devkit/runtime'));
    copyTree(join(r1, 'npm'), join(target, '.devkit/npm'));
    const manifest = object(json(join(target, 'apps/main/package.json')), 'native');
    object(manifest.devDependencies, 'deps')['@docouno/env'] =
      'file:../../.devkit/npm/docouno-env-0.1.0.tgz';
    writeJson(join(target, 'apps/main/package.json'), manifest);
    execFileSync(
      'npm',
      [
        'install',
        '--package-lock-only',
        '--ignore-scripts',
        '--strict-peer-deps',
        '--no-audit',
        '--no-fund',
      ],
      { cwd: join(target, 'apps/main'), stdio: 'ignore' },
    );
    // Real 0.1.0 locks retained exact versions while omitting registry URL/SRI.
    manifest.dependencies = { zod: '4.6.7' };
    writeJson(join(target, 'apps/main/package.json'), manifest);
    const legacyLockPath = join(target, 'apps/main/package-lock.json');
    const legacyLock = object(json(legacyLockPath), 'legacy lock');
    const legacyPackages = object(legacyLock.packages, 'legacy packages');
    object(legacyPackages[''], 'legacy root').dependencies = manifest.dependencies;
    legacyPackages['node_modules/zod'] = { version: '4.6.7', license: 'MIT' };
    writeJson(legacyLockPath, legacyLock);
    const legacyLockHash = digest(readFileSync(legacyLockPath));
    const oldArchiveHash = digest(readFileSync(join(target, '.devkit/npm/docouno-env-0.1.0.tgz')));
    const managed = Object.fromEntries(
      Object.entries(fileMap(join(target, '.devkit'))).map(([name, value]) => [
        `.devkit/${name}`,
        value,
      ]),
    );
    put(
      target,
      '.devkit/runtime.sha256',
      Object.entries(managed)
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([name, value]) => `${value.sha256}  ${name}\n`)
        .join(''),
    );
    managed['.devkit/runtime.sha256'] = identity(join(target, '.devkit/runtime.sha256'))!;
    const legacy: ProjectStateV1 = {
      schemaVersion: 1,
      version: '0.1.0',
      releaseIdentity: digest(readFileSync(join(r1, 'release.json'))),
      generation: randomUUID(),
      packageMode: 'local',
      managed,
      apps: {
        main: {
          kind: 'node',
          template: 'node-app',
          toolDependencies: { '@docouno/env': 'file:../../.devkit/npm/docouno-env-0.1.0.tgz' },
        },
      },
    };
    writeJson(join(target, '.devkit/manifest.json'), legacy);
    put(
      target,
      'Makefile',
      'DEVKIT_APPS := main\ninclude .devkit/runtime/devkit.mk\n# custom edge\n',
    );
    put(target, 'compose.yaml', 'custom YAML deliberately outside supported inventory syntax');
    const source = readFileSync(join(target, 'apps/main/src/index.ts')),
      make = readFileSync(join(target, 'Makefile')),
      compose = readFileSync(join(target, 'compose.yaml'));
    put(target, 'apps/main/npm-shrinkwrap.json', JSON.stringify(legacyLock));
    await assert.rejects(updateProject(r2, target), /Legacy npm-shrinkwrap.json conflicts/);
    rmSync(join(target, 'apps/main/npm-shrinkwrap.json'));
    const updated = await updateProject(r2, target);
    assert.equal(updated.schemaVersion, 2);
    assert.equal(updated.components['apps/main']?.development?.owner, 'project');
    assert.deepEqual(readFileSync(join(target, 'Makefile')), make);
    assert.deepEqual(readFileSync(join(target, 'compose.yaml')), compose);
    assert.deepEqual(readFileSync(join(target, 'apps/main/src/index.ts')), source);
    assert.equal(existsSync(join(target, 'apps/main/package-lock.json')), false);
    assert.equal(existsSync(join(target, 'package-lock.json')), true);
    const receipt = object(updated.legacyIdentityNormalization, 'normalization receipt');
    assert.equal(receipt.basis, 'exact-version-registry-metadata-observed-at-update');
    assert.equal(receipt.normalizedRegistryEntries, 1);
    assert.equal(
      object(
        object(object(receipt.source, 'source').locks, 'source locks')[
          'apps/main/package-lock.json'
        ],
        'source lock',
      ).sha256,
      legacyLockHash,
    );
    assert.equal(
      object(
        object(receipt.localArchiveInputs, 'source archives')['.devkit/npm/docouno-env-0.1.0.tgz'],
        'old archive',
      ).sha256,
      oldArchiveHash,
    );
  } finally {
    f.close();
  }
});

await test('final native tooling resolution cannot upgrade an existing product edge', async () => {
  const f = fixture();
  try {
    const r1 = release(f.root, '0.1.0'),
      r2 = release(f.root, '0.2.0'),
      r3 = release(f.root, '0.3.0');
    const target = join(f.root, 'product graph');
    await initProject(r1, target, { template: 'node', packageMode: 'local' });
    const packagePath = join(target, 'apps/main/package.json');
    const manifest = object(json(packagePath), 'manifest');
    manifest.dependencies = { debug: '4.3.4' };
    writeJson(packagePath, manifest);
    await regenerateWorkspaceLock(target);
    manifest.dependencies = { debug: '^4.0.0' };
    writeJson(packagePath, manifest);
    const lock = object(json(join(target, 'package-lock.json')), 'lock');
    object(object(lock.packages, 'packages')['apps/main'], 'app').dependencies =
      manifest.dependencies;
    writeJson(join(target, 'package-lock.json'), lock);
    for (const [releaseRoot, peer] of [
      [r2, true],
      [r3, false],
    ] as const) {
      const packageName = peer ? 'peer-tool' : 'neutral-tool';
      put(
        target,
        `shared/${packageName}/package.json`,
        JSON.stringify({
          name: packageName,
          version: '1.0.0',
          ...(peer ? { peerDependencies: { debug: '4.3.5' } } : {}),
        }),
      );
      put(target, `shared/${packageName}/index.js`, 'module.exports = true;\n');
      execFileSync(
        'npm',
        ['pack', '--ignore-scripts', '--pack-destination', join(target, 'shared')],
        { cwd: join(target, 'shared', packageName), stdio: 'ignore' },
      );
      const profilePath = join(releaseRoot, 'templates/node-api/package.json');
      const profile = object(json(profilePath), 'profile');
      object(profile.devDependencies, 'tools')[packageName] =
        `file:../../shared/${packageName}-1.0.0.tgz`;
      writeJson(profilePath, profile);
      seal(releaseRoot, peer ? '0.2.0' : '0.3.0');
    }
    const before = sourceSignature(target, projectState(target));
    await assert.rejects(updateProject(r2, target), /Product artifact changed/);
    assert.equal(sourceSignature(target, projectState(target)), before);
    assert.equal(existsSync(join(target, '.devkit/update-pending.json')), false);
    assert.equal((await updateProject(r3, target)).version, '0.3.0');
  } finally {
    f.close();
  }
});

await test('accepted profile cannot silently lose a native tooling dependency', async () => {
  const f = fixture();
  try {
    const r = release(f.root, '0.2.0'),
      target = join(f.root, 'profile ownership');
    await initProject(r, target, { template: 'node', packageMode: 'local' });
    const accepted = object(json(join(target, '.devkit/manifest.json')), 'state');
    const components = object(accepted.components, 'components');
    object(object(components['apps/main'], 'main').toolDependencies, 'tools')['@docouno/env'] =
      'tampered';
    writeJson(join(target, '.devkit/manifest.json'), accepted);
    await assert.rejects(updateProject(r, target), /Accepted tooling profile differs/);
  } finally {
    f.close();
  }
});

await test('native closure follows project-local Cargo manifests without a host Rust installation', async () => {
  const f = fixture();
  try {
    const r = release(f.root, '0.2.0'),
      target = join(f.root, 'cargo inputs');
    const state = await initProject(r, target, { template: 'rust', packageMode: 'local' });
    put(target, 'shared/rust-lib/Cargo.toml', '[package]\nname="shared-native"\nversion="0.1.0"\n');
    put(
      target,
      'apps/api/Cargo.toml',
      '[package]\nname="api"\nversion="0.1.0"\n[dependencies]\nshared-native={path="../../shared/rust-lib"}\n',
    );
    const before = collectNativeInputClosure(target, state);
    assert.ok(Object.hasOwn(before.files, 'shared/rust-lib/Cargo.toml'));
    assert.ok(Object.hasOwn(before.files, 'shared/rust-lib/.cargo/config.toml'));
    put(target, 'shared/rust-lib/.cargo/config.toml', '[build]\njobs=1\n');
    assert.notEqual(collectNativeInputClosure(target, state).digest, before.digest);
  } finally {
    f.close();
  }
});

await test('register adopts an existing native workspace member and rejects incompatible profiles', async () => {
  const f = fixture();
  try {
    const r = release(f.root, '0.2.0'),
      target = join(f.root, 'adopt workspace');
    await initProject(r, target, { template: 'node', packageMode: 'local' });
    copyTree(join(target, 'apps/main'), join(target, 'apps/existing'));
    const manifest = object(json(join(target, 'apps/existing/package.json')), 'existing package');
    manifest.name = '@adopt/existing';
    writeJson(join(target, 'apps/existing/package.json'), manifest);
    const root = object(json(join(target, 'package.json')), 'workspace root');
    root.workspaces = ['apps/existing', 'apps/main'];
    writeJson(join(target, 'package.json'), root);
    await regenerateWorkspaceLock(target);
    const before = readFileSync(join(target, 'apps/existing/src/index.ts'));
    const adopted = await registerProjectComponent(r, target, 'app', 'existing', 'node');
    assert.equal(adopted.components['apps/existing']?.origin.template, 'adopted');
    assert.deepEqual(readFileSync(join(target, 'apps/existing/src/index.ts')), before);
    await assert.rejects(
      registerProjectComponent(r, target, 'library', 'invalid', 'node'),
      /cannot register/,
    );
  } finally {
    f.close();
  }
});

await test('canonical identity hydration preserves exact versions and core rejects incomplete seeds', async () => {
  const calls: string[] = [];
  const entry = { version: '4.6.7' };
  const input = {
    lockfileVersion: 3,
    packages: {
      '': {},
      'node_modules/zod': entry,
      'node_modules/other/node_modules/zod': { ...entry },
    },
  };
  const dist = {
    tarball: 'https://registry.npmjs.org/zod/-/zod-4.6.7.tgz',
    integrity: `sha512-${Buffer.alloc(64, 2).toString('base64')}`,
  };
  const hydrated = await hydrateLockIdentities(input, {
    cwd: '.',
    cache: '.',
    fetchDist: (name, version) => {
      calls.push(`${name}@${version}`);
      return Promise.resolve(dist);
    },
  });
  assert.deepEqual(calls, ['zod@4.6.7']);
  assertCompleteLockIdentities(hydrated);
  assert.deepEqual(input.packages['node_modules/zod'], { version: '4.6.7' });
  const data = object(object(hydrated.packages, 'packages')['node_modules/zod'], 'zod');
  assert.equal(data.version, '4.6.7');
  assert.equal(data.resolved, dist.tarball);
  assert.equal(data.integrity, dist.integrity);
  await assert.rejects(
    hydrateLockIdentities(
      {
        lockfileVersion: 3,
        packages: {
          '': {},
          'node_modules/zod': { version: '4.6.7', resolved: 'https://invalid.test/zod.tgz' },
        },
      },
      { cwd: '.', cache: '.', fetchDist: () => Promise.resolve(dist) },
    ),
    /identity conflict/,
  );
  const f = fixture();
  try {
    const r = release(f.root, '0.2.0');
    const manifest = object(json(join(r, 'templates/node-api/package.json')), 'package');
    manifest.dependencies = { zod: '4.6.7' };
    writeJson(join(r, 'templates/node-api/package.json'), manifest);
    const lock = object(json(join(r, 'templates/node-api/package-lock.json')), 'lock');
    const packages = object(lock.packages, 'packages');
    object(packages[''], 'root').dependencies = manifest.dependencies;
    packages['node_modules/zod'] = { version: '4.6.7' };
    writeJson(join(r, 'templates/node-api/package-lock.json'), lock);
    seal(r, '0.2.0');
    const target = join(f.root, 'incomplete');
    await assert.rejects(
      initProject(r, target, { template: 'node', packageMode: 'local' }),
      /Unprovable native artifact identity/,
    );
    assert.equal(existsSync(target), false);
  } finally {
    f.close();
  }
});

await test('same-version update synchronizes product package name/version and native workspace links', async () => {
  const f = fixture();
  try {
    const r = release(f.root, '0.2.0'),
      target = join(f.root, 'identity');
    const before = await initProject(r, target, { template: 'node', packageMode: 'local' });
    const file = join(target, 'apps/main/package.json'),
      manifest = object(json(file), 'package');
    const oldName = String(manifest.name);
    manifest.name = '@review/renamed';
    manifest.version = '2.0.0';
    writeJson(file, manifest);
    assert.throws(
      () => assertNativeComponents(target, projectState(target)),
      /identity projection is stale/,
    );
    const next = await updateProject(r, target);
    assert.notEqual(next.generation, before.generation);
    const lock = object(json(join(target, 'package-lock.json')), 'lock'),
      packages = object(lock.packages, 'packages');
    assert.equal(object(packages['apps/main'], 'member').name, '@review/renamed');
    assert.equal(object(packages['apps/main'], 'member').version, '2.0.0');
    assert.equal(object(packages['node_modules/@review/renamed'], 'link').resolved, 'apps/main');
    assert.equal(packages[`node_modules/${oldName}`], undefined);
    execFileSync(
      'npm',
      ['ci', '--ignore-scripts', '--include=dev', '--strict-peer-deps', '--no-audit', '--no-fund'],
      { cwd: target, stdio: 'ignore' },
    );
    assertNativeComponents(target, next);
  } finally {
    f.close();
  }
});

await test('app and package namespaces can use the same component name in either creation order', async () => {
  const f = fixture();
  try {
    const r = composableRelease(f.root, '0.2.0');
    for (const packageFirst of [false, true]) {
      const target = join(f.root, packageFirst ? 'package-first' : 'app-first');
      await initProject(r, target, {
        apps: [{ name: packageFirst ? 'starter' : 'client', template: 'node-api' }],
        packageMode: 'local',
      });
      const priorApp = object(
        json(join(target, 'apps', packageFirst ? 'starter' : 'client', 'package.json')),
        'app',
      ).name;
      await addProjectComponent(r, target, 'library', { name: 'client', template: 'react-client' });
      if (packageFirst) {
        await addProjectComponent(
          r,
          target,
          'app',
          { name: 'client', template: 'node-api' },
          { composeInventory: captureComposeInputs(target) },
        );
      }
      const app = object(json(join(target, 'apps/client/package.json')), 'app'),
        library = object(json(join(target, 'packages/client/package.json')), 'library');
      assert.notEqual(app.name, library.name);
      assert.equal(
        object(
          json(join(target, 'apps', packageFirst ? 'starter' : 'client', 'package.json')),
          'original app',
        ).name,
        priorApp,
      );
      assertNativeComponents(target, projectState(target));
    }
  } finally {
    f.close();
  }
});

await test('root shrinkwrap cannot select an unvalidated native graph', async () => {
  const f = fixture();
  try {
    const r = release(f.root, '0.2.0'),
      target = join(f.root, 'shrinkwrap');
    const state = await initProject(r, target, { template: 'node', packageMode: 'local' });
    for (const content of [
      readFileSync(join(target, 'package-lock.json'), 'utf8'),
      '{"broken":true}',
    ]) {
      put(target, 'npm-shrinkwrap.json', content);
      const before = sourceSignature(target, state);
      assert.throws(() => assertNativeComponents(target, state), /npm-shrinkwrap.json conflicts/);
      await assert.rejects(updateProject(r, target), /npm-shrinkwrap.json conflicts/);
      assert.equal(sourceSignature(target, state), before);
      assert.equal(existsSync(join(target, '.devkit/update-pending.json')), false);
    }
  } finally {
    f.close();
  }
});

await test('rollback removes only newly created namespace parents, including after manifest write', async () => {
  const f = fixture();
  try {
    const r = composableRelease(f.root, '0.2.0');
    for (const existingParent of [false, true]) {
      for (const phase of ['source', 'manifest']) {
        const target = join(f.root, `${existingParent}-${phase}`);
        const state = await initProject(r, target, { template: 'node', packageMode: 'local' });
        if (existingParent) {
          mkdirSync(join(target, 'packages'));
        }
        const before = sourceSignature(target, state);
        await assert.rejects(
          addProjectComponent(
            r,
            target,
            'library',
            { name: 'client', template: 'react-client' },
            {
              hooks: {
                afterWrite() {
                  const reached =
                    phase === 'source'
                      ? existsSync(join(target, 'packages/client/package.json'))
                      : Boolean(projectState(target).components['packages/client']);
                  if (reached) {
                    throw new Error('rollback checkpoint');
                  }
                },
              },
            },
          ),
          /rollback checkpoint/,
        );
        assert.equal(sourceSignature(target, state), before);
        assert.equal(existsSync(join(target, 'packages')), existingParent);
        assert.equal(existsSync(join(target, 'packages/client')), false);
      }
    }
  } finally {
    f.close();
  }
});

await test('native admission and explicit update reject competing manifests on either toolchain', async () => {
  const f = fixture();
  try {
    const r = composableRelease(f.root, '0.2.0');
    for (const template of ['node', 'rust'] as const) {
      const target = join(f.root, `ambiguous-${template}`);
      const state = await initProject(r, target, { template, packageMode: 'local' });
      const component = template === 'node' ? 'apps/main' : 'apps/api';
      const competitor = template === 'node' ? 'Cargo.toml' : 'package.json';
      put(
        target,
        `${component}/${competitor}`,
        template === 'node' ? '[package]\nname="embedded"\nversion="0.1.0"\n' : '{}\n',
      );
      const before = sourceSignature(target, state);
      assert.throws(() => assertNativeComponents(target, state), /Ambiguous native manifests/);
      await assert.rejects(updateProject(r, target), /Ambiguous native manifests/);
      assert.equal(sourceSignature(target, state), before);
      rmSync(join(target, component, competitor));
      assertNativeComponents(target, state);
      if (template === 'node') {
        put(
          target,
          `${component}/nested/Cargo.toml`,
          '[package]\nname="nested"\nversion="0.1.0"\n',
        );
        assertNativeComponents(target, state);
      }
    }
    const target = join(f.root, 'adopt-ambiguous');
    await initProject(r, target, { template: 'rust', packageMode: 'local' });
    copyTree(join(r, 'templates/rust-api'), join(target, 'apps/extra'));
    put(target, 'apps/extra/package.json', '{}\n');
    await assert.rejects(
      registerProjectComponent(r, target, 'app', 'extra', 'rust'),
      /Ambiguous native manifests/,
    );
    rmSync(join(target, 'apps/api/Cargo.toml'));
    await assert.rejects(updateProject(r, target), /Missing expected native rust manifest/);
  } finally {
    f.close();
  }
});

await test('legacy normalization fills only absent exact-version identities and binds local archive bytes', async () => {
  const f = fixture();
  try {
    const bytes = Buffer.from('old accepted local archive bytes');
    put(f.root, 'shared/archive.tgz', bytes.toString());
    put(f.root, 'shared/library/package.json', '{"name":"legacy-directory","version":"1.0.0"}');
    const sri = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
    const input = {
      lockfileVersion: 3,
      packages: {
        '': {
          dependencies: {
            zod: '4.6.7',
            archive: 'file:shared/archive.tgz',
            'legacy-directory': 'file:shared/library',
          },
        },
        'node_modules/zod': { version: '4.6.7', license: 'MIT', optional: true, os: ['linux'] },
        'node_modules/archive': {
          version: '0.1.0',
          resolved: 'file:shared/archive.tgz',
          integrity: sri,
        },
        'node_modules/legacy-directory': { link: true, resolved: 'shared/library' },
        'shared/library': { name: 'legacy-directory', version: '1.0.0' },
      },
    };
    const original = structuredClone(input),
      seen: string[] = [];
    const dist = {
      tarball: 'https://registry.npmjs.org/zod/-/zod-4.6.7.tgz',
      integrity: `sha512-${Buffer.alloc(64, 3).toString('base64')}`,
    };
    const result = await normalizeLegacyLockIdentities(input, {
      projectRoot: f.root,
      cwd: f.root,
      cache: f.root,
      fetchDist: (name, version) => {
        seen.push(`${name}@${version}`);
        return Promise.resolve(dist);
      },
    });
    assert.deepEqual(input, original);
    assert.deepEqual(seen, ['zod@4.6.7']);
    assert.equal(result.normalizedRegistryEntries, 1);
    const packages = object(result.lock.packages, 'normalized packages');
    assert.deepEqual(Object.keys(packages), Object.keys(original.packages));
    assert.deepEqual(packages['node_modules/archive'], original.packages['node_modules/archive']);
    assert.deepEqual(
      packages['node_modules/legacy-directory'],
      original.packages['node_modules/legacy-directory'],
    );
    assert.deepEqual(packages['shared/library'], original.packages['shared/library']);
    assert.deepEqual(packages['node_modules/zod'], {
      ...original.packages['node_modules/zod'],
      resolved: dist.tarball,
      integrity: dist.integrity,
    });
    assert.equal(result.localArchives['shared/archive.tgz']?.sha256, digest(bytes));
    assert.deepEqual(readFileSync(join(f.root, 'shared/archive.tgz')), bytes);
    const conflicting = structuredClone(input);
    conflicting.packages['node_modules/archive'].integrity =
      `sha512-${Buffer.alloc(64, 4).toString('base64')}`;
    await assert.rejects(
      normalizeLegacyLockIdentities(conflicting, {
        projectRoot: f.root,
        cwd: f.root,
        cache: f.root,
        fetchDist: () => Promise.resolve(dist),
      }),
      /local archive integrity conflict/,
    );
  } finally {
    f.close();
  }
});

await test('ordinary schema2 update and admission never normalize absent artifact identities', async () => {
  const f = fixture();
  try {
    const r = release(f.root, '0.2.0'),
      target = join(f.root, 'strict-v2');
    const state = await initProject(r, target, { template: 'node', packageMode: 'local' });
    const file = join(target, 'package-lock.json'),
      lock = object(json(file), 'lock');
    const packages = object(lock.packages, 'packages');
    const key = Object.keys(packages).find((name) => name.endsWith('/@docouno/env'))!;
    delete object(packages[key], 'artifact').integrity;
    writeJson(file, lock);
    const before = sourceSignature(target, state);
    assert.throws(
      () => assertNativeComponents(target, state),
      /Unprovable native artifact identity/,
    );
    await assert.rejects(updateProject(r, target), /Unprovable native artifact identity/);
    assert.equal(sourceSignature(target, state), before);
  } finally {
    f.close();
  }
});
