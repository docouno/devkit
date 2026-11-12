import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { packComponent, verifyPackableComponent } from '../src/pack.js';

async function withPackFixture(callback) {
  const temp = await fs.mkdtemp(path.join(os.tmpdir(), 'devkit-pack-unit-'));
  const pkgDir = path.join(temp, 'packages', 'ui');
  await fs.mkdir(path.join(pkgDir, 'dist'), { recursive: true });
  await fs.mkdir(path.join(pkgDir, 'src'), { recursive: true });

  const write = async (rel, content) => {
    const target = path.join(pkgDir, rel);
    await fs.mkdir(path.dirname(target), { recursive: true });
    await fs.writeFile(
      target,
      typeof content === 'string' ? content : `${JSON.stringify(content, null, 2)}\n`,
    );
  };

  const baseManifest = {
    name: '@acme/ui-kit',
    version: '0.2.0',
    private: false,
    type: 'module',
    files: ['dist'],
    sideEffects: ['*.css'],
    exports: {
      '.': {
        types: './dist/index.d.mts',
        default: './dist/index.mjs',
      },
      './styles.css': './dist/styles.css',
      './package.json': './package.json',
    },
    peerDependencies: {
      react: '^19.0.0',
    },
  };

  await write('package.json', baseManifest);
  await write('src/index.ts', 'export function greet(name: string): string { return name; }\n');
  await write('dist/index.mjs', 'export function greet(name) {\n  return name;\n}\n');
  await write('dist/index.d.mts', 'export declare function greet(name: string): string;\n');
  await write('dist/styles.css', '.acme-btn { display: inline-flex; }\n');

  try {
    await callback({ pkgDir, baseManifest, write });
  } finally {
    await fs.rm(temp, { recursive: true, force: true });
  }
}

test('packComponent packs valid ESM + d.ts + CSS library, cleans stale tarballs, and writes deterministic receipt', async () => {
  await withPackFixture(async ({ pkgDir }) => {
    const releaseDir = path.join(pkgDir, 'release');
    await fs.mkdir(releaseDir, { recursive: true });
    await fs.writeFile(path.join(releaseDir, 'acme-ui-kit-0.1.0.tgz'), 'stale');

    const result = await packComponent(pkgDir);
    assert.equal(result.receipt.name, '@acme/ui-kit');
    assert.equal(result.receipt.version, '0.2.0');
    assert.equal(result.receipt.filename, 'acme-ui-kit-0.2.0.tgz');
    assert.equal(result.receipt.publintPassed, true);
    assert.equal(result.receipt.attwProfile, 'esm-only');
    assert.equal(result.receipt.attwPassed, true);

    const releaseEntries = (await fs.readdir(releaseDir)).sort();
    assert.deepEqual(releaseEntries, ['acme-ui-kit-0.2.0.tgz', 'package-receipt.json']);
    const savedReceipt = JSON.parse(
      await fs.readFile(path.join(releaseDir, 'package-receipt.json'), 'utf8'),
    );
    assert.deepEqual(savedReceipt, result.receipt);
  });
});

test('packComponent rejects private packages while verifyPackableComponent allows private file: deps in build verify mode', async () => {
  await withPackFixture(async ({ pkgDir, baseManifest, write }) => {
    await write('package.json', {
      ...baseManifest,
      private: true,
      dependencies: { '@acme/tokens': 'file:../tokens' },
    });
    await assert.rejects(() => packComponent(pkgDir), /has "private": true/);

    const buildVerify = await verifyPackableComponent(pkgDir, { allowPrivateLocalDeps: true });
    assert.equal(buildVerify.ok, true, JSON.stringify(buildVerify.errors));

    const publishVerify = await verifyPackableComponent(pkgDir, { allowPrivateLocalDeps: false });
    assert.equal(publishVerify.ok, false);
    assert(publishVerify.errors.some((err) => err.includes('LOCAL_DEPENDENCY')));
  });
});

test('verifyPackableComponent fails on broken exports (publint) and broken ESM typing (attw)', async () => {
  await withPackFixture(async ({ pkgDir, baseManifest, write }) => {
    // Missing exported target triggers missing target + publint failure
    await write('package.json', {
      ...baseManifest,
      exports: {
        '.': {
          default: './dist/index.mjs',
          types: './dist/index.d.mts',
        },
      },
    });
    const orderCheck = await verifyPackableComponent(pkgDir);
    assert.equal(orderCheck.ok, false);
    assert(
      orderCheck.errors.some((err) => err.includes('EXPORTS_TYPES_SHOULD_BE_FIRST')),
      JSON.stringify(orderCheck.errors),
    );

    // FalseESM / untyped resolution caught by @arethetypeswrong/core
    await write('package.json', baseManifest);
    await write('dist/index.d.mts', "export { missing } from './nonexistent.mjs';\n");
    const attwCheck = await verifyPackableComponent(pkgDir);
    assert.equal(attwCheck.ok, false);
    assert(
      attwCheck.errors.some((err) => err.includes('[attw:')),
      JSON.stringify(attwCheck.errors),
    );
  });
});

test('verifyPackableComponent rejects vite/client references, ../src/ d.ts leaks, and leaked src/ files', async () => {
  await withPackFixture(async ({ pkgDir, baseManifest, write }) => {
    await write(
      'dist/index.d.mts',
      '/// <reference types="vite/client" />\nexport declare function greet(name: string): string;\n',
    );
    const viteLeak = await verifyPackableComponent(pkgDir);
    assert.equal(viteLeak.ok, false);
    assert(viteLeak.errors.some((err) => err.includes('vite/client')));

    await write('dist/index.d.mts', "export type { Foo } from '../src/index.js';\n");
    const srcImportLeak = await verifyPackableComponent(pkgDir);
    assert.equal(srcImportLeak.ok, false);
    assert(srcImportLeak.errors.some((err) => err.includes('../src')));

    await write('dist/index.d.mts', 'export declare function greet(name: string): string;\n');
    await write('package.json', { ...baseManifest, files: ['dist', 'src'] });
    const tarballSrcLeak = await verifyPackableComponent(pkgDir);
    assert.equal(tarballSrcLeak.ok, false);
    assert(tarballSrcLeak.errors.some((err) => err.includes('src/index.ts')));
  });
});
