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
          './icons': {
            types: './dist/icons.d.mts',
            default: './dist/icons.mjs',
          },
          './styles.css': './dist/style.css',
          './tokens.css': './dist/tokens/base.css',
          './package.json': './package.json',
        },
      }),
    );
    await fs.mkdir(path.join(uiRoot, 'src/icons'), { recursive: true });
    await fs.writeFile(path.join(uiRoot, 'src/index.tsx'), 'export const Button = () => null;\n');
    await fs.writeFile(path.join(uiRoot, 'src/icons/index.ts'), 'export const Icon = "ok";\n');
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

    const normalize = (p) => p.replaceAll('\\', '/');
    assert.equal(
      plugin.resolveId('@demo/package-ui'),
      normalize(path.join(uiRoot, 'src/index.tsx')),
    );
    assert.equal(
      plugin.resolveId('@demo/package-ui/icons'),
      normalize(path.join(uiRoot, 'src/icons/index.ts')),
    );
    assert.equal(
      plugin.resolveId('@demo/package-ui/styles.css?inline'),
      `${normalize(path.join(uiRoot, 'src/styles.css'))}?inline`,
    );
    assert.equal(plugin.resolveId('@demo/package-ui/tokens.css'), null);
    assert.equal(plugin.resolveId('@demo/package-ui/package.json'), null);
  } finally {
    await fs.rm(temp, { recursive: true, force: true });
  }
});

test('devkitWorkspaceSource includes react-client development ./src exports and rejects symlinked paths', async () => {
  const temp = await fs.mkdtemp(path.join(os.tmpdir(), 'devkit-vite-react-client-'));
  const prevVitest = process.env.VITEST;
  try {
    delete process.env.VITEST;
    const appRoot = path.join(temp, 'apps', 'web');
    const clientRoot = path.join(temp, 'packages', 'client');
    const external = path.join(temp, 'external');
    await fs.mkdir(path.join(appRoot, 'src'), { recursive: true });
    await fs.mkdir(path.join(clientRoot, 'src'), { recursive: true });
    await fs.mkdir(external, { recursive: true });
    await fs.mkdir(path.join(temp, '.devkit'), { recursive: true });

    await fs.writeFile(
      path.join(temp, '.devkit/manifest.json'),
      JSON.stringify({
        schemaVersion: 2,
        components: {
          'apps/web': { role: 'app' },
          'packages/client': { role: 'library' },
        },
      }),
    );
    await fs.writeFile(
      path.join(clientRoot, 'package.json'),
      JSON.stringify({
        name: 'devkit-react-client',
        version: '0.1.0',
        private: true,
        type: 'module',
        exports: {
          '.': {
            types: './dist/index.d.ts',
            development: './src/index.ts',
            import: './dist/index.js',
          },
          './style.css': {
            development: './src/style.css',
            default: './dist/style.css',
          },
          './sub': {
            development: './src/sub/entry.ts',
            import: './dist/sub/entry.js',
          },
        },
      }),
    );
    await fs.writeFile(path.join(clientRoot, 'src/index.ts'), 'export const App = () => null;\n');
    await fs.writeFile(path.join(clientRoot, 'src/style.css'), '.app { display: grid; }\n');
    await fs.writeFile(path.join(external, 'entry.ts'), 'export const leaked = true;\n');
    await fs.symlink(external, path.join(clientRoot, 'src/sub'));

    const plugin = devkitWorkspaceSource(appRoot);
    const patch = plugin.config({ root: appRoot }, { command: 'serve', mode: 'development' });
    assert.ok(patch);
    assert(patch.optimizeDeps.exclude.includes('devkit-react-client'));
    assert(patch.server.fs.allow.includes(clientRoot));
    assert(patch.server.fs.allow.includes(path.join(clientRoot, 'src')));

    const normalize = (p) => p.replaceAll('\\', '/');
    assert.equal(
      plugin.resolveId('devkit-react-client'),
      normalize(path.join(clientRoot, 'src/index.ts')),
    );
    assert.equal(
      plugin.resolveId('devkit-react-client/style.css'),
      normalize(path.join(clientRoot, 'src/style.css')),
    );
    // Symlinked intermediate directory inside packages/client/src must be rejected.
    assert.equal(plugin.resolveId('devkit-react-client/sub'), null);

    // Inactive in build and VITEST modes.
    assert.equal(
      plugin.config({ root: appRoot }, { command: 'build', mode: 'production' }),
      undefined,
    );
    assert.equal(plugin.resolveId('devkit-react-client'), null);
    process.env.VITEST = 'true';
    assert.equal(
      plugin.config({ root: appRoot }, { command: 'serve', mode: 'development' }),
      undefined,
    );
    assert.equal(plugin.resolveId('devkit-react-client'), null);
    delete process.env.VITEST;

    // Symlinked source file or symlinked src directory itself must also be rejected.
    await fs.unlink(path.join(clientRoot, 'src/index.ts'));
    await fs.symlink(path.join(external, 'entry.ts'), path.join(clientRoot, 'src/index.ts'));
    await fs.unlink(path.join(clientRoot, 'src/style.css'));
    assert.equal(
      plugin.config({ root: appRoot }, { command: 'serve', mode: 'development' }),
      undefined,
    );

    const movedSrc = path.join(temp, 'moved-src');
    await fs.rm(path.join(clientRoot, 'src'), { recursive: true, force: true });
    await fs.mkdir(movedSrc, { recursive: true });
    await fs.writeFile(path.join(movedSrc, 'index.ts'), 'export const App = () => null;\n');
    await fs.symlink(movedSrc, path.join(clientRoot, 'src'));
    assert.equal(
      plugin.config({ root: appRoot }, { command: 'serve', mode: 'development' }),
      undefined,
    );

    // Restore valid client package, then test `packages/../../outside`, multi-segment `packages/nested/lib`,
    // symlinked `pkgRoot`, symlinked `.devkit/manifest.json`, and symlinked `.devkit`.
    await fs.unlink(path.join(clientRoot, 'src'));
    await fs.mkdir(path.join(clientRoot, 'src'), { recursive: true });
    await fs.writeFile(path.join(clientRoot, 'src/index.ts'), 'export const App = () => null;\n');
    const outsidePkg = path.join(path.dirname(temp), `devkit-outside-${path.basename(temp)}`);
    const nestedPkg = path.join(temp, 'packages', 'nested', 'lib');
    try {
      for (const [dir, pkgName] of [
        [outsidePkg, 'devkit-outside-escape'],
        [nestedPkg, 'devkit-nested-escape'],
      ]) {
        await fs.mkdir(path.join(dir, 'src'), { recursive: true });
        await fs.writeFile(
          path.join(dir, 'package.json'),
          JSON.stringify({
            name: pkgName,
            version: '0.1.0',
            private: true,
            type: 'module',
            exports: { '.': { development: './src/index.ts', import: './dist/index.js' } },
          }),
        );
        await fs.writeFile(path.join(dir, 'src/index.ts'), 'export const escaped = true;\n');
      }
      await fs.writeFile(
        path.join(temp, '.devkit/manifest.json'),
        JSON.stringify({
          schemaVersion: 2,
          components: {
            'apps/web': { role: 'app' },
            [`packages/../../${path.basename(outsidePkg)}`]: { role: 'library' },
            'packages/nested/lib': { role: 'library' },
          },
        }),
      );
      assert.equal(
        plugin.config({ root: appRoot }, { command: 'serve', mode: 'development' }),
        undefined,
      );
      assert.equal(plugin.resolveId('devkit-outside-escape'), null);
      assert.equal(plugin.resolveId('devkit-nested-escape'), null);

      // Symlinked pkgRoot (`packages/escaped -> outsidePkg`) must be rejected.
      await fs.symlink(outsidePkg, path.join(temp, 'packages', 'escaped'));
      await fs.writeFile(
        path.join(temp, '.devkit/manifest.json'),
        JSON.stringify({
          schemaVersion: 2,
          components: {
            'apps/web': { role: 'app' },
            'packages/escaped': { role: 'library' },
          },
        }),
      );
      assert.equal(
        plugin.config({ root: appRoot }, { command: 'serve', mode: 'development' }),
        undefined,
      );

      // Symlinked `.devkit/manifest.json` and symlinked `.devkit` directory must be rejected.
      const extManifest = path.join(external, 'manifest.json');
      await fs.writeFile(
        extManifest,
        JSON.stringify({
          schemaVersion: 2,
          components: {
            'apps/web': { role: 'app' },
            'packages/client': { role: 'library' },
          },
        }),
      );
      await fs.unlink(path.join(temp, '.devkit/manifest.json'));
      await fs.symlink(extManifest, path.join(temp, '.devkit/manifest.json'));
      assert.equal(
        plugin.config({ root: appRoot }, { command: 'serve', mode: 'development' }),
        undefined,
      );

      const extDevkit = path.join(external, 'fake-devkit');
      await fs.mkdir(extDevkit, { recursive: true });
      await fs.copyFile(extManifest, path.join(extDevkit, 'manifest.json'));
      await fs.rm(path.join(temp, '.devkit'), { recursive: true, force: true });
      await fs.symlink(extDevkit, path.join(temp, '.devkit'));
      assert.equal(
        plugin.config({ root: appRoot }, { command: 'serve', mode: 'development' }),
        undefined,
      );
    } finally {
      await fs.rm(outsidePkg, { recursive: true, force: true });
    }
  } finally {
    if (prevVitest === undefined) {
      delete process.env.VITEST;
    } else {
      process.env.VITEST = prevVitest;
    }
    await fs.rm(temp, { recursive: true, force: true });
  }
});
