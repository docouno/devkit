import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import {
  copyTree,
  digest,
  fileMap,
  identity,
  json,
  object,
  safePath,
  writeJson,
} from '../tooling/files.ts';
import { initProject } from '../tooling/project.ts';
import { projectState, sourceSignature } from '../tooling/project-state.ts';
import { recoverProject, registerProjectApp, updateProject } from '../tooling/project-update.ts';
import type { Release, UpdateJournal } from '../tooling/types.ts';

const toolchain =
  'DEVKIT_NODE_IMAGE=node:24.18.0-bookworm-slim@sha256:6f7b03f7c2c8e2e784dcf9295400527b9b1270fd37b7e9a7285cf83b6951452d\nDEVKIT_NODE_VERSION=24.18.0\nDEVKIT_RUST_VERSION=1.99.1\nDEVKIT_RUST_RECIPE_HASH=' +
  '0'.repeat(64) +
  '\n';

function fixture(): { root: string; close: () => void } {
  const root = mkdtempSync(join(tmpdir(), 'devkit project tests '));
  return { root, close: () => rmSync(root, { recursive: true, force: true }) };
}

function put(root: string, path: string, bytes: string): void {
  const target = join(root, path);
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, bytes);
}

function release(root: string, version: string, options: { runtime?: string } = {}): string {
  const destination = join(root, `release-${version}-${options.runtime ?? 'normal'}`);
  mkdirSync(destination);
  for (const name of ['devkit.mk', 'devkit.sh', 'guard.sh', 'compose.tools.yaml']) {
    put(destination, `runtime/${name}`, `${options.runtime ?? 'runtime'} ${version} ${name}\n`);
  }
  put(destination, 'runtime/toolchains.env', `DEVKIT_VERSION=${version}\n${toolchain}`);
  const packageRoot = join(destination, 'packing');
  put(
    packageRoot,
    'package/package.json',
    JSON.stringify({
      name: '@docouno/env',
      version,
      type: 'module',
      exports: './index.js',
    }),
  );
  put(packageRoot, 'package/index.js', 'export const fixture = true;\n');
  mkdirSync(join(destination, 'npm'));
  const archive = `npm/docouno-env-${version}.tgz`;
  execFileSync('tar', ['-czf', join(destination, archive), '-C', packageRoot, 'package']);
  rmSync(packageRoot, { recursive: true });
  for (const template of ['node-app', 'react-app']) {
    const manifest = {
      name: `fixture-${template}`,
      version: '1.0.0',
      private: true,
      type: 'module',
      scripts: Object.fromEntries(
        ['check', 'fix', 'test', 'build', 'dev'].map((key) => [key, 'node --version']),
      ),
      devDependencies: { '@docouno/env': version },
    };
    put(
      destination,
      `templates/${template}/package.json`,
      `${JSON.stringify(manifest, null, 2)}\n`,
    );
    const integrity = `sha512-${createHash('sha512')
      .update(readFileSync(join(destination, archive)))
      .digest('base64')}`;
    put(
      destination,
      `templates/${template}/package-lock.json`,
      JSON.stringify({
        name: manifest.name,
        version: manifest.version,
        lockfileVersion: 3,
        requires: true,
        packages: {
          '': {
            name: manifest.name,
            version: manifest.version,
            devDependencies: manifest.devDependencies,
          },
          'node_modules/@docouno/env': {
            version,
            resolved: `https://registry.npmjs.org/@docouno/env/-/env-${version}.tgz`,
            integrity,
            dev: true,
          },
        },
      }),
    );
    put(destination, `templates/${template}/src/index.ts`, 'export const answer = 42;\n');
  }
  put(
    destination,
    'templates/react-app/src/main.tsx',
    readFileSync(new URL('../templates/react-app/src/main.tsx', import.meta.url), 'utf8'),
  );
  put(
    destination,
    'templates/rust-app/Cargo.toml',
    '[package]\nname = "fixture-api"\nversion = "1.0.0"\nedition = "2024"\n',
  );
  put(destination, 'templates/rust-app/Cargo.lock', '# fixture native lock\nversion = 4\n');
  put(destination, 'templates/rust-app/rust-toolchain.toml', '[toolchain]\nchannel = "1.99.1"\n');
  put(destination, 'templates/rust-app/src/main.rs', 'fn main() {}\n');
  seal(destination, version, archive);
  return destination;
}

function seal(
  destination: string,
  version: string,
  archive = `npm/docouno-env-${version}.tgz`,
): void {
  rmSync(join(destination, 'release.json'), { force: true });
  const manifest: Release = {
    schemaVersion: 1,
    version,
    rustVersion: '1.99.1',
    files: fileMap(destination),
    environment: {
      name: '@docouno/env',
      version,
      archive,
      integrity: `sha512-${createHash('sha512')
        .update(readFileSync(join(destination, archive)))
        .digest('base64')}`,
    },
  };
  writeJson(join(destination, 'release.json'), manifest);
}

await test('init validates before target writes and preserves existing Git metadata', async () => {
  const f = fixture();
  try {
    const r = release(f.root, '0.1.0');
    const target = join(f.root, 'project with spaces');
    put(target, '.git', 'gitdir: /unrelated/worktree\n');
    const state = await initProject(r, target, {
      template: 'rust',
      packageMode: 'local',
    });
    assert.equal(readFileSync(join(target, '.git'), 'utf8'), 'gitdir: /unrelated/worktree\n');
    assert.equal(state.packageMode, 'local');
    assert.ok(existsSync(join(target, '.devkit/npm/docouno-env-0.1.0.tgz')));
    const checksum = readFileSync(join(target, '.devkit/runtime.sha256'), 'utf8');
    assert.ok(checksum.includes('apps/api/rust-toolchain.toml'));
    assert.ok(!checksum.includes('runtime.sha256'));
    execFileSync('sha256sum', ['--check', '--status', '.devkit/runtime.sha256'], { cwd: target });
    const invalid = join(f.root, 'invalid');
    put(r, 'runtime/devkit.sh', 'tampered\n');
    await assert.rejects(
      initProject(r, invalid, { template: 'node', packageMode: 'local' }),
      /integrity mismatch/,
    );
    assert.equal(existsSync(invalid), false);
    const nonempty = join(f.root, 'nonempty');
    put(nonempty, 'mine', 'retain me');
    seal(r, '0.1.0');
    await assert.rejects(
      initProject(r, nonempty, { template: 'rust', packageMode: 'local' }),
      /new or empty/,
    );
    assert.equal(readFileSync(join(nonempty, 'mine'), 'utf8'), 'retain me');
  } finally {
    f.close();
  }
});

await test('local Node update preserves native/product edits and resolves full staged path dependencies', async () => {
  const f = fixture();
  try {
    const r1 = release(f.root, '0.1.0');
    const r2 = release(f.root, '0.2.0');
    const target = join(f.root, 'node project');
    await initProject(r1, target, { template: 'node', packageMode: 'local' });
    const manifestPath = join(target, 'apps/main/package.json');
    const native = object(json(manifestPath), 'native');
    native.dependencies = { 'local-extra': 'file:../../shared/my package' };
    object(native.devDependencies, 'devDependencies')['local-user-tool'] =
      'file:../../shared/my tool';
    object(native.scripts, 'scripts').custom = 'echo preserve-user-script';
    object(native.scripts, 'scripts').postinstall =
      'node -e \'require("node:fs").writeFileSync("scripts-ran","bad")\'';
    native.productMetadata = { untouched: [1, 2, 3] };
    writeJson(manifestPath, native);
    put(
      target,
      'shared/my package/package.json',
      JSON.stringify({ name: 'local-extra', version: '3.0.0' }),
    );
    put(
      target,
      'shared/my tool/package.json',
      JSON.stringify({ name: 'local-user-tool', version: '4.0.0' }),
    );
    put(target, 'apps/main/src/свой файл.ts', 'export const product = true;\n');
    symlinkSync('свой файл.ts', join(target, 'apps/main/src/alias.ts'));
    put(
      target,
      'Makefile',
      'DEVKIT_APPS := main\ninclude .devkit/runtime/devkit.mk\n# my build edges\n',
    );
    put(target, '.env', 'TOKEN=retain-real-env\n');
    const stateNative = object(json(join(target, '.devkit/manifest.json')), 'state');
    stateNative.productMetadata = { keep: true };
    writeJson(join(target, '.devkit/manifest.json'), stateNative);
    const before = readFileSync(join(target, 'Makefile'));
    const result = await updateProject(r2, target);
    assert.equal(result.version, '0.2.0');
    assert.equal(result.packageMode, 'local');
    const after = object(json(manifestPath), 'updated manifest');
    assert.deepEqual(after.dependencies, native.dependencies);
    assert.deepEqual(after.scripts, native.scripts);
    assert.deepEqual(after.productMetadata, native.productMetadata);
    assert.equal(
      object(after.devDependencies, 'deps')['local-user-tool'],
      'file:../../shared/my tool',
    );
    assert.equal(
      object(after.devDependencies, 'deps')['@docouno/env'],
      'file:../../.devkit/npm/docouno-env-0.2.0.tgz',
    );
    assert.deepEqual(readFileSync(join(target, 'Makefile')), before);
    assert.equal(readFileSync(join(target, '.env'), 'utf8'), 'TOKEN=retain-real-env\n');
    assert.deepEqual(object(json(join(target, '.devkit/manifest.json')), 'state').productMetadata, {
      keep: true,
    });
    assert.equal(existsSync(join(target, 'apps/main/scripts-ran')), false);
    assert.equal(existsSync(join(target, '.devkit/npm/docouno-env-0.1.0.tgz')), false);
    const lock = JSON.stringify(json(join(target, 'apps/main/package-lock.json')));
    assert.ok(lock.includes('shared/my package'));
    assert.ok(!lock.includes(r1) && !lock.includes(r2) && !lock.includes('localhost'));
    execFileSync('sha256sum', ['--check', '--status', '.devkit/runtime.sha256'], { cwd: target });
    const signature = sourceSignature(target);
    const unchanged = await updateProject(r2, target);
    assert.equal(unchanged.generation, result.generation);
    assert.equal(sourceSignature(target), signature);
    const older = await updateProject(r1, target);
    assert.notEqual(older.generation, result.generation);
  } finally {
    f.close();
  }
});

await test('registry init keeps canonical locks and only rewrites root metadata', async () => {
  const f = fixture();
  try {
    const r = release(f.root, '0.1.0');
    const target = join(f.root, 'registry');
    const original = object(json(join(r, 'templates/node-app/package-lock.json')), 'lock');
    await initProject(r, target, {
      template: 'node',
      packageMode: 'registry',
      name: 'renamed',
    });
    const updated = object(json(join(target, 'apps/main/package-lock.json')), 'lock');
    assert.equal(updated.name, 'renamed-main');
    assert.deepEqual(
      object(updated.packages, 'packages')['node_modules/@docouno/env'],
      object(original.packages, 'packages')['node_modules/@docouno/env'],
    );
    assert.equal(existsSync(join(target, '.devkit/npm')), false);
    assert.equal(
      object(json(join(target, 'apps/main/package.json')), 'package').name,
      'renamed-main',
    );
  } finally {
    f.close();
  }
});

await test('managed/dependency conflicts and concurrent edits fail before applying payload', async () => {
  const f = fixture();
  try {
    const r1 = release(f.root, '0.1.0');
    const r2 = release(f.root, '0.2.0');
    const target = join(f.root, 'conflicts');
    await initProject(r1, target, { template: 'node', packageMode: 'local' });
    put(target, '.devkit/runtime/devkit.sh', 'product edit\n');
    const baseline = sourceSignature(target);
    await assert.rejects(updateProject(r2, target), /Managed file conflict/);
    assert.equal(sourceSignature(target), baseline);
    writeFileSync(
      join(target, '.devkit/runtime/devkit.sh'),
      readFileSync(join(r1, 'runtime/devkit.sh')),
    );
    const native = object(json(join(target, 'apps/main/package.json')), 'manifest');
    object(native.devDependencies, 'deps')['@docouno/env'] = '9.9.9';
    writeJson(join(target, 'apps/main/package.json'), native);
    await assert.rejects(updateProject(r2, target), /Tool dependency conflict/);
    object(native.devDependencies, 'deps')['@docouno/env'] =
      'file:../../.devkit/npm/docouno-env-0.1.0.tgz';
    writeJson(join(target, 'apps/main/package.json'), native);
    const runtime = readFileSync(join(target, '.devkit/runtime/devkit.sh'));
    await assert.rejects(
      updateProject(r2, target, {
        hooks: {
          afterStage: () => put(target, 'apps/main/src/user.ts', 'concurrent input\n'),
        },
      }),
      /changed while staging/,
    );
    assert.deepEqual(readFileSync(join(target, '.devkit/runtime/devkit.sh')), runtime);
    assert.equal(projectState(target).version, '0.1.0');
    assert.equal(existsSync(join(target, '.devkit/update-pending.json')), false);
  } finally {
    f.close();
  }
});

await test('same-version different identities and state symlinks are refused', async () => {
  const f = fixture();
  try {
    const r1 = release(f.root, '0.1.0');
    const altered = release(f.root, '0.1.0', { runtime: 'altered' });
    const target = join(f.root, 'safe');
    await initProject(r1, target, { template: 'rust', packageMode: 'local' });
    await assert.rejects(updateProject(altered, target), /same version/);
    const outside = join(f.root, 'outside');
    mkdirSync(outside);
    mkdirSync(join(target, '.devkit/state'), { recursive: true });
    symlinkSync(join(outside, 'lock'), join(target, '.devkit/state/task.lock'));
    await assert.rejects(updateProject(r1, target), /symlink/);
    assert.throws(() => recoverProject(target), /symlink/);
    assert.equal(existsSync(join(outside, 'lock')), false);
    const linked = join(f.root, 'linked');
    symlinkSync(outside, linked);
    await assert.rejects(
      initProject(r1, join(linked, 'new'), {
        template: 'rust',
        packageMode: 'local',
      }),
      /symlink/,
    );
  } finally {
    f.close();
  }
});

await test('apply errors roll back and real interruption retains a conditionally recoverable journal', async () => {
  const f = fixture();
  try {
    const r1 = release(f.root, '0.1.0');
    const r2 = release(f.root, '0.2.0');
    const target = join(f.root, 'recovery');
    await initProject(r1, target, { template: 'rust', packageMode: 'local' });
    const before = sourceSignature(target);
    await assert.rejects(
      updateProject(r2, target, {
        hooks: {
          afterWrite: () => {
            throw new Error('injected write failure');
          },
        },
      }),
      /injected write failure/,
    );
    assert.equal(sourceSignature(target), before);
    const source = fileURLToPath(new URL('../tooling/project-update.ts', import.meta.url));
    const child = spawnSync(
      process.execPath,
      [
        '--input-type=module',
        '--eval',
        `import {updateProject} from ${JSON.stringify(source)}; await updateProject(${JSON.stringify(r2)},${JSON.stringify(target)},{hooks:{afterWrite(){process.exit(86)}}});`,
      ],
      { encoding: 'utf8' },
    );
    assert.equal(child.status, 86, child.stderr);
    const pendingPath = join(target, '.devkit/update-pending.json');
    const pending = json(pendingPath) as UpdateJournal;
    assert.ok(pending.files.length > 1);
    const backedUp = pending.files.find((file) => file.backup !== null)!;
    const savedBackup = readFileSync(safePath(target, backedUp.backup!));
    const protectedState = readFileSync(join(target, '.devkit/manifest.json'));
    writeFileSync(safePath(target, backedUp.backup!), 'corrupted backup');
    assert.throws(() => recoverProject(target), /backup integrity mismatch/);
    assert.deepEqual(readFileSync(join(target, '.devkit/manifest.json')), protectedState);
    writeFileSync(safePath(target, backedUp.backup!), savedBackup);
    const changed = pending.files[0]!;
    assert.notEqual(identity(join(target, changed.path))?.sha256, changed.before?.sha256);
    await assert.rejects(updateProject(r2, target), /incomplete/);
    const appliedBytes =
      changed.staged === null ? null : readFileSync(safePath(target, changed.staged));
    writeFileSync(join(target, changed.path), 'unknown user bytes\n');
    const protectedBytes = readFileSync(join(target, '.devkit/manifest.json'));
    assert.throws(() => recoverProject(target), /Recovery conflict/);
    assert.deepEqual(readFileSync(join(target, '.devkit/manifest.json')), protectedBytes);
    assert.ok(existsSync(pendingPath));
    if (appliedBytes === null) {
      rmSync(join(target, changed.path));
    } else {
      writeFileSync(join(target, changed.path), appliedBytes);
    }
    assert.equal(recoverProject(target), true);
    assert.equal(recoverProject(target), false);
    assert.equal(sourceSignature(target), before);
    assert.equal(existsSync(join(target, pending.stage)), false);
    assert.equal((await updateProject(r2, target)).version, '0.2.0');
  } finally {
    f.close();
  }
});

await test('added apps need explicit registration with exact accepted native tools', async () => {
  const f = fixture();
  try {
    const r1 = release(f.root, '0.1.0');
    const r2 = release(f.root, '0.2.0');
    const target = join(f.root, 'registration');
    await initProject(r1, target, { template: 'rust', packageMode: 'local' });
    copyTree(join(r1, 'templates/rust-app'), join(target, 'apps/extra'));
    const makefile = readFileSync(join(target, 'Makefile'));
    await assert.rejects(updateProject(r2, target), /Unregistered native app/);
    await assert.rejects(registerProjectApp(r2, target, 'extra', 'rust'), /exact accepted release/);
    const registered = await registerProjectApp(r1, target, 'extra', 'rust');
    assert.ok(registered.apps.extra);
    assert.ok(registered.managed['apps/extra/rust-toolchain.toml']);
    assert.deepEqual(readFileSync(join(target, 'Makefile')), makefile);
    const updated = await updateProject(r2, target);
    assert.ok(updated.apps.extra);
    await assert.rejects(registerProjectApp(r2, target, 'extra', 'rust'), /already registered/);
    assert.ok(/^[a-f0-9]{64}$/.test(digest(readFileSync(join(target, '.devkit/runtime.sha256')))));
  } finally {
    f.close();
  }
});

await test('new runtime ownership conflicts and native resolution failure leave project payload unchanged', async () => {
  const f = fixture();
  try {
    const r1 = release(f.root, '0.1.0');
    const r2 = release(f.root, '0.2.0');
    put(r2, 'runtime/new-helper.sh', 'release helper\n');
    seal(r2, '0.2.0');
    const target = join(f.root, 'new managed conflict');
    await initProject(r1, target, { template: 'node', packageMode: 'local' });
    put(target, '.devkit/runtime/new-helper.sh', 'project helper\n');
    const before = sourceSignature(target);
    await assert.rejects(updateProject(r2, target), /New managed file conflicts/);
    assert.equal(sourceSignature(target), before);
    rmSync(join(target, '.devkit/runtime/new-helper.sh'));
    const nativePath = join(target, 'apps/main/package.json');
    const native = object(json(nativePath), 'native');
    native.dependencies = { 'missing-native-input': 'file:../../shared/absent-package.tgz' };
    writeJson(nativePath, native);
    const beforeResolution = sourceSignature(target);
    await assert.rejects(updateProject(r2, target), /npm failed/);
    assert.equal(sourceSignature(target), beforeResolution);
    assert.equal(existsSync(join(target, '.devkit/update-pending.json')), false);
  } finally {
    f.close();
  }
});

await test('mixed init enables the API in its owned starter entry and fails closed on template contract changes', async () => {
  const f = fixture();
  try {
    const r1 = release(f.root, '0.1.0');
    const r2 = release(f.root, '0.2.0');
    const mixed = join(f.root, 'mixed');
    const standalone = join(f.root, 'standalone');
    const original = readFileSync(join(r1, 'templates/react-app/src/main.tsx'), 'utf8');
    await initProject(r1, mixed, { template: 'mixed', packageMode: 'local' });
    await initProject(r1, standalone, { template: 'react', packageMode: 'local' });
    const mixedEntry = join(mixed, 'apps/web/src/main.tsx');
    assert.equal(
      readFileSync(mixedEntry, 'utf8'),
      original.replace('    <App />\n', '    <App apiEnabled />\n'),
    );
    assert.equal(readFileSync(join(standalone, 'apps/web/src/main.tsx'), 'utf8'), original);
    assert.equal(Object.hasOwn(projectState(mixed).managed, 'apps/web/src/main.tsx'), false);
    const edited = `// Product-owned entry\n${readFileSync(mixedEntry, 'utf8')}`;
    writeFileSync(mixedEntry, edited);
    await updateProject(r2, mixed);
    assert.equal(readFileSync(mixedEntry, 'utf8'), edited);
    const rejected = join(f.root, 'changed-template');
    for (const changed of [
      original.replace('    <App />\n', '    <App changed />\n'),
      original.replace('    <App />\n', '    <App />\n    <App />\n'),
    ]) {
      put(r2, 'templates/react-app/src/main.tsx', changed);
      seal(r2, '0.2.0');
      await assert.rejects(
        initProject(r2, rejected, { template: 'mixed', packageMode: 'local' }),
        /Mixed template contract changed/,
      );
      assert.equal(existsSync(rejected), false);
    }
  } finally {
    f.close();
  }
});
