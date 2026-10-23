import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { stripVTControlCharacters } from 'node:util';

const environment = fileURLToPath(new URL('..', import.meta.url));
const compiler = fileURLToPath(import.meta.resolve('typescript/bin/tsc'));
const oldDeclaration = `import type { Reporter, TestModule, TestRunEndReason } from 'vitest/node';
export default class RunnableTestsReporter implements Reporter {
  onTestRunEnd(modules: ReadonlyArray<TestModule>, errors: ReadonlyArray<unknown>, reason: TestRunEndReason): void;
}
`;

test(
  'hoisted reporter declarations accept nested real Vitest without a root peer',
  { timeout: 120_000 },
  (context) => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'devkit nested vitest '));
    const app = path.join(root, 'apps/web');
    const installedEnv = path.join(root, 'node_modules/@docouno/env');
    const write = (base, name, value) => {
      const target = path.join(base, name);
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.writeFileSync(target, typeof value === 'string' ? value : JSON.stringify(value));
    };
    const inherited = Object.fromEntries(
      Object.entries(process.env).filter(
        ([name]) => !/^npm_/iu.test(name) && !['NODE_OPTIONS', 'NODE_PATH'].includes(name),
      ),
    );
    const childEnv = {
      ...inherited,
      npm_config_cache:
        process.env.npm_config_cache ??
        process.env.NPM_CONFIG_CACHE ??
        path.join(root, 'npm-cache'),
      npm_config_userconfig: '/dev/null',
      npm_config_audit: 'false',
      npm_config_fund: 'false',
      npm_config_update_notifier: 'false',
    };
    const run = (binary, args, timeout = 30_000) =>
      spawnSync(binary, args, { cwd: app, env: childEnv, encoding: 'utf8', timeout });
    const output = (result) => `${result.stdout}\n${result.stderr}`;
    try {
      write(app, 'package.json', {
        name: 'nested-vitest-consumer',
        private: true,
        type: 'module',
        devDependencies: { vitest: '5.0.5', '@types/node': '24.19.2' },
      });
      const install = run(
        'npm',
        [
          'install',
          '--workspaces=false',
          '--ignore-scripts',
          '--strict-peer-deps',
          '--no-audit',
          '--no-fund',
        ],
        75_000,
      );
      assert.equal(install.status, 0, output(install));
      assert.equal(fs.existsSync(path.join(root, 'node_modules/vitest')), false);
      assert.equal(
        JSON.parse(fs.readFileSync(path.join(app, 'node_modules/vitest/package.json'))).version,
        '5.0.5',
      );
      for (const name of ['package.json', 'src/vitest-reporter.js', 'src/vitest-reporter.d.ts']) {
        write(installedEnv, name, fs.readFileSync(path.join(environment, name), 'utf8'));
      }
      write(app, 'tsconfig.json', {
        compilerOptions: {
          module: 'NodeNext',
          moduleResolution: 'NodeNext',
          target: 'ES2023',
          lib: ['ES2023', 'DOM'],
          types: ['node'],
          strict: true,
          skipLibCheck: false,
          noEmit: true,
        },
        files: ['reporter.ts'],
      });
      write(
        app,
        'reporter.ts',
        `import Guard from '@docouno/env/vitest-reporter';
import type { Reporter } from 'vitest/node';
const reporter: Reporter = new Guard();
const constructor: new () => Reporter = Guard;
const forward = (...args: Parameters<NonNullable<Reporter['onTestRunEnd']>>) => new Guard().onTestRunEnd(...args);
// @ts-expect-error The reporter must keep its required native module surface.
new Guard().onTestRunEnd([{}], [], 'passed');
// @ts-expect-error An unsupported run reason is not accepted.
new Guard().onTestRunEnd([], [], 'cancelled');
void reporter; void constructor; void forward;
`,
      );
      const declaration = path.join(installedEnv, 'src/vitest-reporter.d.ts');
      const current = fs.readFileSync(declaration, 'utf8');
      fs.writeFileSync(declaration, oldDeclaration);
      const before = run(process.execPath, [compiler, '-p', 'tsconfig.json']);
      assert.notEqual(before.status, 0);
      assert.match(output(before), /vitest-reporter\.d\.ts.*TS2307.*vitest\/node/u);
      fs.writeFileSync(declaration, current);
      const after = run(process.execPath, [compiler, '-p', 'tsconfig.json']);
      assert.equal(after.status, 0, output(after));
      context.diagnostic(
        'Old external-peer declaration: TS2307; current declaration: strict tsc + native Reporter assignability passed.',
      );

      write(
        app,
        'vitest.config.mjs',
        `import Guard from '@docouno/env/vitest-reporter';
export default { test: { reporters: ['default', new Guard()], passWithNoTests: true, maxWorkers: 1, fileParallelism: false } };
`,
      );
      const vitest = path.join(app, 'node_modules/vitest/vitest.mjs');
      const cases = [
        [
          'passed-with-individual-skip',
          "import {it,expect} from 'vitest'; it('works',()=>expect(1).toBe(1)); it.skip('optional',()=>{});",
          0,
          null,
        ],
        [
          'skip-only',
          "import {it} from 'vitest'; it.skip('optional',()=>{});",
          1,
          /No runnable Vitest/u,
        ],
        ['empty', 'export {};', 1, /No runnable Vitest/u],
        [
          'native-failure',
          "import {it,expect} from 'vitest'; it('native failure',()=>expect(1).toBe(2));",
          1,
          /expected 1 to be 2/u,
        ],
      ];
      for (const [name, source, status, diagnostic] of cases) {
        write(app, 'sample.test.js', source);
        const result = run(process.execPath, [vitest, 'run']);
        assert.equal(result.status, status, `${name}\n${output(result)}`);
        const plain = stripVTControlCharacters(output(result));
        if (diagnostic) {
          assert.match(plain, diagnostic, name);
        }
        if (name === 'native-failure') {
          assert.doesNotMatch(plain, /No runnable Vitest/u, name);
        }
      }
      context.diagnostic(
        'Real nested Vitest 5.0.5: positive, skip-only, empty and native-failure cases retained.',
      );
      assert.equal(fs.existsSync(path.join(root, 'node_modules/vitest')), false);
    } finally {
      fs.rmSync(root, { recursive: true, force: true });
    }
  },
);
