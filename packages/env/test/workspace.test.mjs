import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import ts from 'typescript';
import { workspaceContext, workspaceImportErrors } from '../src/workspace.js';

const options = {
  module: ts.ModuleKind.NodeNext,
  moduleResolution: ts.ModuleResolutionKind.NodeNext,
  target: ts.ScriptTarget.ES2023,
};
function fixture(run) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'devkit-workspace-'));
  const app = path.join(root, 'apps/web');
  const lib = path.join(root, 'packages/client');
  const write = (name, content) => {
    const file = path.join(root, name);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, typeof content === 'string' ? content : JSON.stringify(content));
  };
  const pkg = {
    name: 'web',
    type: 'module',
    dependencies: { '@fixture/client': 'file:../../packages/client' },
  };
  write('apps/web/package.json', pkg);
  write('apps/web/src/index.ts', 'export {};');
  write('apps/api/package.json', { name: '@fixture/api', type: 'module', exports: './index.js' });
  write('apps/api/index.js', 'export const privateValue = 1;');
  write('packages/client/package.json', {
    name: '@fixture/client',
    type: 'module',
    exports: {
      '.': { types: './dist/index.d.ts', import: './dist/index.js' },
      './style.css': './dist/style.css',
    },
  });
  write('packages/client/dist/index.d.ts', 'export declare const value: number;');
  write('packages/client/dist/index.js', 'export const value = 1;');
  write('packages/client/dist/style.css', 'body {}');
  write('packages/client/src/private.ts', 'export const value = 2;');
  write('packages/client/src/private.css', 'body {}');
  write('.devkit/manifest.json', {
    schemaVersion: 2,
    components: {
      'apps/web': { role: 'app' },
      'apps/api': { role: 'app' },
      'packages/client': { role: 'library' },
    },
  });
  fs.mkdirSync(path.join(root, 'node_modules/@fixture'), { recursive: true });
  fs.symlinkSync('../../packages/client', path.join(root, 'node_modules/@fixture/client'));
  fs.symlinkSync('../../apps/api', path.join(root, 'node_modules/@fixture/api'));
  const errors = (source, settings = options, base = app) =>
    workspaceImportErrors(
      workspaceContext(base),
      path.join(base, 'src/index.ts'),
      source,
      settings,
    );
  try {
    run({ root, app, lib, pkg, write, errors });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

test('declared public workspace code and stylesheet exports are allowed', () =>
  fixture(({ errors }) => {
    assert.deepEqual(errors("import { value } from '@fixture/client'; export {value};"), []);
    assert.deepEqual(errors("import '@fixture/client/style.css';"), []);
  }));

test('root hoisting does not authorize undeclared library imports', () =>
  fixture(({ write, pkg, errors }) => {
    write('apps/web/package.json', { ...pkg, dependencies: {} });
    assert(
      errors("import '@fixture/client';").some((message) =>
        message.includes('not a declared dependency'),
      ),
    );
  }));

test('relative, private alias and application imports cannot cross component boundaries', () =>
  fixture(({ app, write, pkg, errors }) => {
    assert(errors("import '../../../packages/client/src/private.ts';").length);
    assert(errors("import '../../../packages/client/src/private.css';").length);
    assert(
      errors("import '@fixture/client/private';", {
        ...options,
        baseUrl: app,
        paths: { '@fixture/client/private': ['../../packages/client/src/private.ts'] },
      }).length,
    );
    write('apps/web/package.json', {
      ...pkg,
      dependencies: { ...pkg.dependencies, '@fixture/api': 'file:../api' },
    });
    assert(
      errors("import '@fixture/api';").some((message) => message.includes('another application')),
    );
  }));

test('library-to-application import and private CSS alias fail', () =>
  fixture(({ app, lib, write, errors }) => {
    write('packages/client/package.json', {
      name: '@fixture/client',
      type: 'module',
      dependencies: { '@fixture/api': 'file:../../apps/api' },
      exports: './dist/index.js',
    });
    assert(errors("import '@fixture/api';", options, lib).length);
    assert(
      errors("import '@private/style.css';", {
        ...options,
        baseUrl: app,
        paths: { '@private/*': ['../../packages/client/src/*'] },
      }).length,
    );
  }));

test('same package name does not confuse a nested registry dependency with a workspace library', () =>
  fixture(({ write, errors, root }) => {
    write('packages/debug/package.json', {
      name: 'debug',
      version: '999.0.0',
      type: 'module',
      exports: './index.js',
    });
    write('packages/debug/index.js', 'export const value = 999;');
    write('apps/web/node_modules/debug/package.json', {
      name: 'debug',
      version: '4.3.4',
      type: 'module',
      exports: { '.': { types: './index.d.ts', import: './index.js' } },
    });
    write('apps/web/node_modules/debug/index.d.ts', 'export declare const value: number;');
    write('apps/web/node_modules/debug/index.js', 'export const value = 4;');
    write('.devkit/manifest.json', {
      schemaVersion: 2,
      components: { 'apps/web': { role: 'app' }, 'packages/debug': { role: 'library' } },
    });
    fs.symlinkSync('../packages/debug', path.join(root, 'node_modules/debug'));
    assert.deepEqual(errors("import { value } from 'debug'; export { value };"), []);
  }));

test('asset resolution preserves custom conditions and detects redirected public stylesheet names', () =>
  fixture(({ app, lib, pkg, write, errors }) => {
    write('packages/client/package.json', {
      name: '@fixture/client',
      type: 'module',
      exports: {
        './style.css': { development: './src/development.css', default: './dist/style.css' },
      },
    });
    write('packages/client/src/development.css', 'body {}');
    fs.rmSync(path.join(lib, 'dist/style.css'));
    assert.deepEqual(
      errors("import '@fixture/client/style.css';", {
        ...options,
        customConditions: ['development'],
      }),
      [],
    );
    write('packages/client/dist/style.css', 'body {}');
    write('apps/web/package.json', pkg);
    const alias = {
      ...options,
      baseUrl: app,
      paths: { '@fixture/client/style.css': ['../../packages/client/src/private.css'] },
    };
    assert(
      errors("import '@fixture/client/style.css';", alias).some((message) =>
        message.includes('path alias'),
      ),
    );
  }));

test('ordinary TypeScript imports use the package CommonJS resolution mode', () =>
  fixture(({ app, write, errors, pkg }) => {
    write('apps/web/package.json', { ...pkg, type: 'commonjs' });
    write('packages/client/package.json', {
      name: '@fixture/client',
      type: 'commonjs',
      exports: { '.': { require: './dist/index.cjs' } },
    });
    write('packages/client/dist/index.cjs', 'exports.value = 1;');
    write('packages/client/dist/index.d.cts', 'export declare const value: number;');
    const source = "import { value } from '@fixture/client'; export const used = value;";
    write('apps/web/src/index.ts', source);
    const program = ts.createProgram([path.join(app, 'src/index.ts')], options);
    assert.deepEqual(ts.getPreEmitDiagnostics(program), []);
    assert.deepEqual(errors(source), []);
  }));

test('Bundler public comparison preserves node/browser export conditions for code and styles', () =>
  fixture(({ write, errors }) => {
    write('packages/client/package.json', {
      name: '@fixture/client',
      type: 'module',
      exports: {
        '.': { node: { types: './dist/node.d.ts' }, default: { types: './dist/browser.d.ts' } },
        './style.css': { node: './dist/node.css', default: './dist/browser.css' },
      },
    });
    write('packages/client/dist/node.d.ts', 'export declare const value: string;');
    write('packages/client/dist/browser.d.ts', 'export declare const value: number;');
    write('packages/client/dist/node.css', 'body {}');
    write('packages/client/dist/browser.css', 'body {}');
    const settings = {
      ...options,
      module: ts.ModuleKind.ESNext,
      moduleResolution: ts.ModuleResolutionKind.Bundler,
    };
    assert.deepEqual(
      errors(
        "import { value } from '@fixture/client'; import '@fixture/client/style.css'; export { value };",
        settings,
      ),
      [],
    );
  }));
