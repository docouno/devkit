import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { pathToFileURL } from 'node:url';
import { ESLint } from 'eslint';
import { desktopConfig } from '../src/eslint.js';

const moduleUrl = pathToFileURL(path.resolve(import.meta.dirname, '../src/eslint.js')).href;

async function fixture(run) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'devkit-desktop-config-'));
  await fs.writeFile(path.join(root, 'package.json'), '{"type":"module"}\n');
  await fs.writeFile(
    path.join(root, 'eslint.config.mjs'),
    `import { desktopConfig } from ${JSON.stringify(moduleUrl)};\nexport default desktopConfig(import.meta.dirname);\n`,
  );
  try {
    await run(root, new ESLint({ cwd: root }));
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

test('Electron config preserves Node main globals and rejects privileged renderer access', async () => {
  await fixture(async (root, eslint) => {
    assert(Array.isArray(desktopConfig(root)));
    const main = await eslint.lintText('export const value = process.platform;\n', {
      filePath: 'src/main/index.js',
    });
    assert.equal(main[0].errorCount, 0, JSON.stringify(main[0].messages));
    const renderer = await eslint.lintText(
      "import fs from 'node:fs';\nprocess.exit(0);\nexport const value = fs;\n",
      { filePath: 'src/renderer/src/index.js' },
    );
    assert(renderer[0].messages.some((entry) => entry.ruleId === 'no-restricted-imports'));
    assert(renderer[0].messages.some((entry) => entry.ruleId === 'no-restricted-globals'));
    const browser = await eslint.lintText('export const value = document.title;\n', {
      filePath: 'src/renderer/src/index.js',
    });
    assert.equal(browser[0].errorCount, 0, JSON.stringify(browser[0].messages));
  });
});

test('Electron renderer receives React rules, without applying them to main/preload', async () => {
  await fixture(async (_, eslint) => {
    const renderer = await eslint.calculateConfigForFile('src/renderer/src/App.tsx');
    assert.equal(renderer.rules['react-hooks/rules-of-hooks'][0], 2);
    assert.equal(renderer.rules['react-refresh/only-export-components'][0], 2);
    const main = await eslint.calculateConfigForFile('src/main/index.ts');
    const preload = await eslint.calculateConfigForFile('src/preload/index.ts');
    assert.equal(main.rules['react-hooks/rules-of-hooks'], undefined);
    assert.equal(preload.rules['react-hooks/rules-of-hooks'], undefined);
  });
});

test('Electron renderer rejects native literal dynamic imports as well as static imports', async () => {
  await fixture(async (_, eslint) => {
    for (const source of [
      "export const load = () => import('node:fs');",
      "export const load = () => import('electron');",
      'export const load = () => import(`node:fs`);',
    ]) {
      const [result] = await eslint.lintText(source, { filePath: 'src/renderer/src/index.js' });
      assert(
        result.messages.some((entry) => entry.ruleId === 'devkit-desktop/no-native-dynamic-import'),
      );
    }
    const [main] = await eslint.lintText("export const load = () => import('node:fs');", {
      filePath: 'src/main/index.js',
    });
    assert.equal(main.errorCount, 0, JSON.stringify(main.messages));
  });
});
