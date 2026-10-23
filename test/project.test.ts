import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import { copyTree, digest, identity, json, object, safePath, writeJson } from '../tooling/files.ts';
import { initProject } from '../tooling/project.ts';
import { regenerateWorkspaceLock } from '../tooling/native-graph.ts';
import { projectState, sourceSignature } from '../tooling/project-state.ts';
import { recoverProject, registerProjectApp, updateProject } from '../tooling/project-update.ts';
import type { UpdateJournal } from '../tooling/types.ts';

import { fixture, put, release, seal } from './helpers/project-fixture.ts';

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
    await regenerateWorkspaceLock(target);
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
    const lock = JSON.stringify(json(join(target, 'package-lock.json')));
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
    const original = object(json(join(r, 'templates/node-api/package-lock.json')), 'lock');
    await initProject(r, target, {
      template: 'node',
      packageMode: 'registry',
      name: 'renamed',
    });
    const updated = object(json(join(target, 'package-lock.json')), 'lock');
    assert.equal(updated.name, 'renamed');
    assert.deepEqual(
      object(updated.packages, 'packages')['apps/main/node_modules/@docouno/env'],
      object(original.packages, 'packages')['node_modules/@docouno/env'],
    );
    assert.equal(existsSync(join(target, '.devkit/npm')), false);
    assert.equal(
      object(json(join(target, 'apps/main/package.json')), 'package').name,
      '@renamed/app-main',
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
    copyTree(join(r1, 'templates/rust-api'), join(target, 'apps/extra'));
    const makefile = readFileSync(join(target, 'Makefile'));
    await assert.rejects(updateProject(r2, target), /Unregistered native component/);
    await assert.rejects(registerProjectApp(r2, target, 'extra', 'rust'), /exact accepted release/);
    const registered = await registerProjectApp(r1, target, 'extra', 'rust');
    assert.ok(registered.components['apps/extra']);
    assert.ok(registered.managed['apps/extra/rust-toolchain.toml']);
    assert.deepEqual(readFileSync(join(target, 'Makefile')), makefile);
    const updated = await updateProject(r2, target);
    assert.ok(updated.components['apps/extra']);
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
    await assert.rejects(updateProject(r2, target), /native|dependency|ENOENT/);
    assert.equal(sourceSignature(target), beforeResolution);
    assert.equal(existsSync(join(target, '.devkit/update-pending.json')), false);
  } finally {
    f.close();
  }
});

await test('mixed composes independent apps without editing their source', async () => {
  const f = fixture();
  try {
    const r1 = release(f.root, '0.1.0'),
      r2 = release(f.root, '0.2.0');
    const target = join(f.root, 'mixed');
    const state = await initProject(r1, target, { template: 'mixed', packageMode: 'local' });
    assert.deepEqual(Object.keys(state.components).sort(), ['apps/api', 'apps/web']);
    assert.equal(state.components['apps/web']?.development?.api, 'apps/api');
    const file = join(target, 'apps/web/src/main.tsx');
    const original = readFileSync(file, 'utf8');
    const edited = `// Product-owned entry\n${original}`;
    writeFileSync(file, edited);
    await updateProject(r2, target);
    assert.equal(readFileSync(file, 'utf8'), edited);
  } finally {
    f.close();
  }
});
