import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';
import { ESLint } from 'eslint';
import { browserConfig, nodeConfig } from '../src/eslint.js';
import { verifyProject } from '../src/verify.js';

const configModule = pathToFileURL(
  fileURLToPath(new URL('../src/eslint.js', import.meta.url)),
).href;
const compiler = {
  compilerOptions: {
    strict: true,
    module: 'NodeNext',
    moduleResolution: 'NodeNext',
    target: 'ES2023',
    lib: ['ES2023'],
    types: [],
    noEmit: true,
  },
  include: ['src/**/*.ts'],
};

async function fixture(callback) {
  const temp = await fs.mkdtemp(path.join(os.tmpdir(), 'devkit env unit '));
  const root = path.join(temp, 'apps', 'main');
  await fs.mkdir(path.join(root, 'src'), { recursive: true });
  const write = async (name, value) => {
    const file = path.join(root, name);
    await fs.mkdir(path.dirname(file), { recursive: true });
    await fs.writeFile(
      file,
      typeof value === 'string' ? value : `${JSON.stringify(value, null, 2)}\n`,
    );
  };
  const pkg = { name: 'env-unit-app', private: true, type: 'module' };
  await write('package.json', pkg);
  await write('tsconfig.json', compiler);
  await write('src/index.ts', 'export const answer = 42;\n');
  await write(
    'eslint.config.mjs',
    `import { nodeConfig } from ${JSON.stringify(configModule)};\nexport default nodeConfig(import.meta.dirname);\n`,
  );
  try {
    await callback({ root, temp, write, pkg });
  } finally {
    await fs.rm(temp, { recursive: true, force: true });
  }
}

const has = (result, rule) => result.errors.some((entry) => entry.rule === rule);

test('config factories require an absolute app root and separate runtime globals', async () => {
  assert.throws(() => nodeConfig('.'), /absolute/u);
  await fixture(async ({ root }) => {
    const node = nodeConfig(root).find((entry) => entry.name === 'devkit/javascript');
    const browser = browserConfig(root).find((entry) => entry.name === 'devkit/javascript');
    assert.equal(node.languageOptions.globals.document, undefined);
    assert.notEqual(browser.languageOptions.globals.document, undefined);
  });
});

test('valid source has typecheck and strict typed-lint coverage', async () => {
  await fixture(async ({ root }) => {
    const result = await verifyProject(root);
    assert.deepEqual(result.errors, []);
    assert.equal(result.ok, true);
    assert.equal(result.fileCount, 1);
    assert.equal(result.configCount, 1);
  });
});

test('the exported factory applies a real typed rule', async () => {
  await fixture(async ({ root, write }) => {
    await write(
      'src/index.ts',
      'async function task(): Promise<void> { await Promise.resolve(); }\ntask();\nexport {};\n',
    );
    const messages = (await new ESLint({ cwd: root }).lintFiles(['src/index.ts'])).flatMap(
      (entry) => entry.messages,
    );
    assert.ok(
      messages.some(
        (entry) =>
          entry.ruleId === '@typescript-eslint/no-floating-promises' && entry.severity === 2,
      ),
    );
  });
});

test('an ignored semantic source is rejected even when the compiler could accept it', async () => {
  await fixture(async ({ root, write }) => {
    await write(
      'src/ignored.ts',
      'async function task(): Promise<void> { await Promise.resolve(); }\ntask();\nexport {};\n',
    );
    await write(
      'eslint.config.mjs',
      `import { nodeConfig } from ${JSON.stringify(configModule)};\nexport default [...nodeConfig(import.meta.dirname), { ignores: ['src/ignored.ts'] }];\n`,
    );
    const result = await verifyProject(root);
    assert.equal(result.ok, false);
    assert.ok(has(result, 'lint-coverage'));
  });
});

test('turning off a mandatory typed rule cannot leave a green coverage result', async () => {
  await fixture(async ({ root, write }) => {
    await write(
      'eslint.config.mjs',
      `import { nodeConfig } from ${JSON.stringify(configModule)};\nexport default [...nodeConfig(import.meta.dirname), { files: ['**/*.ts'], rules: { '@typescript-eslint/no-floating-promises': 'off' } }];\n`,
    );
    assert.ok(has(await verifyProject(root), 'typed-lint-coverage'));
  });
});

test('compiler exclusions cannot remove declared source silently', async () => {
  await fixture(async ({ root, write }) => {
    await write('src/excluded.ts', 'export const excluded = 1;\n');
    await write('tsconfig.json', { ...compiler, exclude: ['src/excluded.ts'] });
    const result = await verifyProject(root);
    assert.ok(
      result.errors.some(
        (entry) => entry.path === 'src/excluded.ts' && entry.rule === 'typecheck-coverage',
      ),
    );
  });
});

test('a compiler import graph covers declared modules beyond its rootNames', async () => {
  await fixture(async ({ root, write }) => {
    await write('src/index.ts', "export { child } from './child.js';\n");
    await write('src/child.ts', 'export const child = 42;\n');
    await write('tsconfig.json', { ...compiler, include: ['src/index.ts'] });
    const result = await verifyProject(root);
    assert.equal(has(result, 'typecheck-coverage'), false);
    assert.equal(result.ok, true);
  });
});

test('authored declaration files are not claimed checked when skipLibCheck skips them', async () => {
  await fixture(async ({ root, write }) => {
    await write('src/declared.d.ts', 'export declare const value: number;\n');
    await write('tsconfig.json', {
      ...compiler,
      compilerOptions: { ...compiler.compilerOptions, skipLibCheck: true },
    });
    assert.ok(
      (await verifyProject(root)).errors.some(
        (entry) => entry.path === 'src/declared.d.ts' && entry.rule === 'typecheck-coverage',
      ),
    );
  });
});

for (const [name, option] of [
  ['noCheck', { noCheck: true }],
  ['strict', { strict: false }],
  ['strictNullChecks', { strictNullChecks: false }],
]) {
  test(`compiler ${name} cannot weaken declared checking`, async () => {
    await fixture(async ({ root, write }) => {
      await write('tsconfig.json', {
        ...compiler,
        compilerOptions: { ...compiler.compilerOptions, ...option },
      });
      assert.ok(has(await verifyProject(root), 'strict-typecheck'));
    });
  });
}

test('real triple-slash ts-nocheck cannot bypass the guard when the optional lint rule is off', async () => {
  await fixture(async ({ root, write }) => {
    await write(
      'eslint.config.mjs',
      `import { nodeConfig } from ${JSON.stringify(configModule)};\nexport default [...nodeConfig(import.meta.dirname), { files: ['**/*.ts'], rules: { '@typescript-eslint/ban-ts-comment': 'off' } }];\n`,
    );
    const source = "export const invalid: number = 'bad';\n";
    await write('src/index.ts', `/// @ts-nocheck\n${source}`);
    const compile = () =>
      spawnSync(
        process.execPath,
        [fileURLToPath(import.meta.resolve('typescript/bin/tsc')), '--noEmit', '-p', root],
        { cwd: root, encoding: 'utf8', timeout: 15_000 },
      );
    const suppressed = compile();
    assert.equal(suppressed.status, 0, suppressed.stdout + suppressed.stderr);
    const lint = await new ESLint({ cwd: root }).lintFiles(['src/index.ts']);
    assert.equal(
      lint.reduce((count, result) => count + result.errorCount, 0),
      0,
    );
    assert.ok(has(await verifyProject(root), 'unchecked-source'));
    await write('src/index.ts', source);
    const checked = compile();
    assert.equal(checked.status, 2, checked.stdout + checked.stderr);
    assert.match(checked.stdout, /TS2322/u);
  });
});

test('directive grammar covers valid leading pragmas without matching harmless text', async () => {
  for (const directive of ['// @ts-nocheck', '/// @ts-nocheck', '/// @TS-NOCHECK: explanation']) {
    await fixture(async ({ root, write }) => {
      await write('src/index.ts', `${directive}\nexport const answer = 42;\n`);
      assert.ok(has(await verifyProject(root), 'unchecked-source'), directive);
    });
  }
  for (const source of [
    '// This code prevents ts-nocheck from bypassing the check.\nexport const text = "/// @ts-nocheck";\n',
    '/* @ts-nocheck */\nexport const answer = 42;\n',
    '//// @ts-nocheck\nexport const answer = 42;\n',
    '// @ts-nocheck-extra\nexport const answer = 42;\n',
    'export const answer = 42;\n/// @ts-nocheck\n',
  ]) {
    await fixture(async ({ root, write }) => {
      await write('src/index.ts', source);
      assert.equal(has(await verifyProject(root), 'unchecked-source'), false, source);
    });
  }
});

test('missing and empty source directories fail explicitly', async () => {
  await fixture(async ({ root }) => {
    await fs.unlink(path.join(root, 'src/index.ts'));
    assert.ok(has(await verifyProject(root), 'source-coverage'));
    await fs.rmdir(path.join(root, 'src'));
    assert.ok(has(await verifyProject(root), 'source-coverage'));
  });
});

for (const devkit of [
  null,
  { unknown: true },
  { sourceDirectories: 'src' },
  { sourceDirectories: ['src\u0000'] },
  { sourceDirectories: ['src\u007f'] },
  { sourceDirectories: ['src['] },
  { sourceDirectories: ['src]'] },
  { sourceDirectories: ['src?'] },
  { sourceDirectories: ['src\\child'] },
  { boundaries: { forbiddenDirectories: null } },
  { typecheckConfigs: ['../tsconfig.json'] },
  { boundaries: { forbidCrossAppImports: 'yes' } },
]) {
  test(`malformed metadata is rejected: ${JSON.stringify(devkit)}`, async () => {
    await fixture(async ({ root, write, pkg }) => {
      await write('package.json', { ...pkg, devkit });
      assert.ok(has(await verifyProject(root), 'metadata'));
    });
  });
}

test('source symlinks and metadata paths through symlink ancestors fail', async () => {
  await fixture(async ({ root, temp, write, pkg }) => {
    const outside = path.join(temp, 'outside');
    await fs.mkdir(path.join(outside, 'source'), { recursive: true });
    await fs.writeFile(path.join(outside, 'source/file.ts'), 'export const outside = 1;\n');
    await fs.symlink(outside, path.join(root, 'src/link'));
    assert.ok(has(await verifyProject(root), 'source-boundary'));
    await fs.unlink(path.join(root, 'src/link'));
    await fs.symlink(outside, path.join(root, 'linked'));
    await write('package.json', { ...pkg, devkit: { sourceDirectories: ['linked/source'] } });
    assert.ok(has(await verifyProject(root), 'source-coverage'));
  });
});

test('cross-app literal imports are checked and explicit policy can opt out', async () => {
  await fixture(async ({ root, write, pkg }) => {
    await write('src/index.ts', "export { other } from '../../other/src/index.js';\n");
    assert.ok(has(await verifyProject(root), 'cross-app-import'));
    await write('package.json', {
      ...pkg,
      devkit: { boundaries: { forbidCrossAppImports: false } },
    });
    assert.equal(has(await verifyProject(root), 'cross-app-import'), false);
  });
});

test('directory policies are explicit and do not inherit the old blanket utils ban', async () => {
  await fixture(async ({ root, write, pkg }) => {
    await write('src/utils/value.ts', 'export const value = 1;\n');
    assert.equal(has(await verifyProject(root), 'forbidden-directory'), false);
    await write('package.json', {
      ...pkg,
      devkit: { boundaries: { forbiddenDirectories: ['utils'] } },
    });
    assert.ok(has(await verifyProject(root), 'forbidden-directory'));
  });
});
