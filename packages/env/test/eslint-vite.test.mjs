import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { ESLint } from 'eslint';
import { browserConfig, reactLibraryConfig } from '../src/eslint.js';
import { verifyProject } from '../src/verify.js';
import { devkitWorkspaceSource } from '../src/vite.js';

const eslintModuleUrl = pathToFileURL(
  fileURLToPath(new URL('../src/eslint.js', import.meta.url)),
).href;

test('reactLibraryConfig allows mixed component/constant exports, enforces hooks, and supports root .mjs/.cjs configs', async () => {
  const temp = await fs.mkdtemp(path.join(os.tmpdir(), 'devkit-react-lib-eslint-'));
  try {
    await fs.mkdir(path.join(temp, 'src'), { recursive: true });
    await fs.writeFile(
      path.join(temp, 'package.json'),
      JSON.stringify({ name: '@acme/ui', private: true, type: 'module' }, null, 2),
    );
    await fs.writeFile(
      path.join(temp, 'tsconfig.json'),
      JSON.stringify(
        {
          compilerOptions: {
            strict: true,
            module: 'ESNext',
            moduleResolution: 'Bundler',
            target: 'ES2023',
            lib: ['ES2023', 'DOM'],
            jsx: 'react-jsx',
            types: [],
            noEmit: true,
          },
          include: ['src/**/*'],
        },
        null,
        2,
      ),
    );
    await fs.writeFile(
      path.join(temp, 'eslint.config.mjs'),
      `import { reactLibraryConfig } from ${JSON.stringify(eslintModuleUrl)};\nexport default reactLibraryConfig(import.meta.dirname);\n`,
    );
    await fs.writeFile(
      path.join(temp, 'tsdown.config.mjs'),
      'export default { entry: [process.env.ENTRY ?? "src/index.ts"] };\n',
    );
    await fs.writeFile(
      path.join(temp, 'legacy.config.cjs'),
      'const path = require("node:path");\nmodule.exports = { root: path.resolve(__dirname) };\n',
    );
    await fs.writeFile(
      path.join(temp, 'src/Button.tsx'),
      'export const buttonVariants = { primary: "btn-primary" };\nexport function createButtonTheme(): string { return "light"; }\nexport function Button(): null { return null; }\n',
    );

    const eslint = new ESLint({ cwd: temp });
    const results = await eslint.lintFiles([
      'eslint.config.mjs',
      'tsdown.config.mjs',
      'legacy.config.cjs',
      'src/Button.tsx',
    ]);
    const allMessages = results.flatMap((r) => r.messages);
    assert.deepEqual(allMessages, []);

    const calc = await eslint.calculateConfigForFile('src/Button.tsx');
    assert.ok(calc.rules['react-hooks/rules-of-hooks']);
    assert.equal(calc.rules['react-refresh/only-export-components'], undefined);

    const browserCalc = browserConfig(temp);
    assert.ok(browserCalc.some((item) => item.name === 'devkit/root-configs'));
    assert.ok(
      reactLibraryConfig({ root: temp }).some((item) => item.name === 'devkit/react-library'),
    );

    // Verify uncovered root .ts config is caught explicitly by verifyProject
    await fs.writeFile(path.join(temp, 'tsdown.config.ts'), 'export default {};\n');
    const verified = await verifyProject(temp);
    assert.equal(verified.ok, false);
    assert(verified.errors.some((entry) => entry.rule === 'root-ts-coverage'));
  } finally {
    await fs.rm(temp, { recursive: true, force: true });
  }
});

test('devkitWorkspaceSource activates only in serve mode and maps workspace dist exports to src', async () => {
  const temp = await fs.mkdtemp(path.join(os.tmpdir(), 'devkit-vite-source-'));
  try {
    const appRoot = path.join(temp, 'apps', 'playground');
    const uiRoot = path.join(temp, 'packages', 'ui');
    await fs.mkdir(path.join(appRoot, 'src'), { recursive: true });
    await fs.mkdir(path.join(uiRoot, 'src'), { recursive: true });
    await fs.mkdir(path.join(temp, '.devkit'), { recursive: true });

    await fs.writeFile(
      path.join(temp, '.devkit/manifest.json'),
      JSON.stringify({
        schemaVersion: 2,
        components: {
          'apps/playground': { role: 'app' },
          'packages/ui': { role: 'library' },
        },
      }),
    );
    await fs.writeFile(
      path.join(uiRoot, 'package.json'),
      JSON.stringify({
        name: '@demo/package-ui',
        version: '0.1.0',
        private: true,
        type: 'module',
        exports: {
          '.': {
            types: './dist/index.d.mts',
            default: './dist/index.mjs',
          },
          './styles.css': './dist/style.css',
          './package.json': './package.json',
        },
      }),
    );
    await fs.writeFile(path.join(uiRoot, 'src/index.tsx'), 'export const Button = () => null;\n');
    await fs.writeFile(path.join(uiRoot, 'src/styles.css'), '.btn { color: red; }\n');

    const plugin = devkitWorkspaceSource(appRoot);
    assert.equal(plugin.apply, 'serve');
    assert.equal(plugin.enforce, 'pre');

    // Inactive in build mode
    assert.equal(
      plugin.config({ root: appRoot }, { command: 'build', mode: 'production' }),
      undefined,
    );
    assert.equal(plugin.resolveId('@demo/package-ui'), null);

    // Active in serve mode
    const patch = plugin.config({ root: appRoot }, { command: 'serve', mode: 'development' });
    assert.ok(patch);
    assert(patch.optimizeDeps.exclude.includes('@demo/package-ui'));
    assert(patch.server.fs.allow.includes(temp));
    assert(patch.server.fs.allow.includes(path.join(uiRoot, 'src')));

    assert.equal(plugin.resolveId('@demo/package-ui'), path.join(uiRoot, 'src/index.tsx'));
    assert.equal(
      plugin.resolveId('@demo/package-ui/styles.css?inline'),
      `${path.join(uiRoot, 'src/styles.css')}?inline`,
    );
    assert.equal(plugin.resolveId('@demo/package-ui/package.json'), null);
  } finally {
    await fs.rm(temp, { recursive: true, force: true });
  }
});
