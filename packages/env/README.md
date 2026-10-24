# @docouno/env

ESM configurations, source checks and test reporters for Devkit's TypeScript applications. The component runs its installed TypeScript, ESLint, Prettier and test binaries directly. A Devkit release pins compatible versions in native lockfiles.

## Configuration

Create `eslint.config.mjs` in the component:

```js
import { nodeConfig } from '@docouno/env/eslint';

export default nodeConfig(import.meta.dirname);
```

Use `browserConfig` for React/browser code or `desktopConfig` for Electron. Each factory takes the absolute component root and enables typed linting. Desktop configurations separate main/preload from renderer code and check renderer imports against the native boundary. Inline ESLint disable directives are rejected; exceptions belong in visible configuration.

Extend the appropriate compiler baseline in `tsconfig.json`:

```json
{
  "extends": "@docouno/env/tsconfig/node.json",
  "include": ["src"]
}
```

Use `@docouno/env/tsconfig/browser.json` for browser code. The component supplies its source/output paths and runtime-specific types.

Create `prettier.config.mjs`:

```js
export { default } from '@docouno/env/prettier';
```

## Source verification

Run the installed `devkit-verify` from the component, or use `devkit-verify --root PATH`. `--json` returns a structured result. Configuration or coverage failures return nonzero.

The defaults can be declared explicitly in `package.json`:

```json
{
  "devkit": {
    "sourceDirectories": ["src"],
    "typecheckConfigs": ["tsconfig.json"],
    "boundaries": {
      "forbidCrossAppImports": true,
      "forbiddenDirectories": []
    }
  }
}
```

Declared source requires strict compiler and lint coverage. TypeScript additionally requires typed linting; JavaScript requires `checkJs`. Ignored or empty source roots, `noCheck`, `ts-nocheck`, source symlinks and unsupported configuration are errors. For multiple compiler projects, list the leaf configurations.

In Devkit projects, verification also checks native dependency declarations and shared packages' public exports using TypeScript resolution. Cross-app imports, library-to-app imports and private library paths fail. Computed runtime module paths remain outside static import checking.

Verification checks compiler and lint coverage. Generated scripts also run the compiler, ESLint and Prettier.

## Test reporters

For Node's test runner:

```sh
node --test --test-reporter=@docouno/env/node-test-reporter
```

For Vitest:

```js
import { defineConfig } from 'vitest/config';
import RunnableTestsReporter from '@docouno/env/vitest-reporter';

export default defineConfig({
  test: {
    passWithNoTests: false,
    reporters: ['default', new RunnableTestsReporter()],
  },
});
```

Both profiles require a runnable registered test case. Empty, skipped-only and TODO-only suites fail; ordinary test failures retain native diagnostics. The Node profile uses `node:test` registrations. Vitest is an optional peer, required only by the Vitest reporter.

## License

[MIT](LICENSE).
