# @docouno/env

ESM configurations, source/boundary checks, Vite workspace HMR, library tarball verification, and test reporters for Devkit's TypeScript applications and libraries. Each component runs its installed TypeScript, ESLint, Prettier, bundler, and test binaries directly; a Devkit release pins compatible versions in native lockfiles.

## Configuration

Create `eslint.config.mjs` in the component:

```js
import { nodeConfig } from '@docouno/env/eslint';

export default nodeConfig(import.meta.dirname);
```

Available ESLint factories:

- `nodeConfig` — Node.js services and pure TypeScript libraries (`ts-library`).
- `browserConfig` — React/browser applications (`react-web`, `react-client`).
- `reactLibraryConfig` — React UI libraries and design systems (`react-ui`), enabling `react-hooks` without restricting modules to single-component Fast Refresh exports.
- `desktopConfig` — Electron applications (`electron-react`), separating main/preload from renderer code and enforcing the native boundary.

Inline ESLint disable directives are rejected; exceptions belong in visible configuration.

Extend the appropriate compiler baseline in `tsconfig.json`:

```json
{
  "extends": "@docouno/env/tsconfig/node.json",
  "include": ["src"]
}
```

Available TypeScript baselines:

- `@docouno/env/tsconfig/node.json` — Node.js services.
- `@docouno/env/tsconfig/browser.json` — React/browser applications.
- `@docouno/env/tsconfig/library.json` — publishable ESM TypeScript libraries (`isolatedDeclarations`, bundler module resolution, empty ambient `types` so `.d.ts` output never leaks build-tool globals).
- `@docouno/env/tsconfig/react-library.json` — publishable React UI libraries (`react-jsx`, DOM libs, `isolatedDeclarations`).

Create `prettier.config.mjs`:

```js
export { default } from '@docouno/env/prettier';
```

## Workspace Source HMR (`@docouno/env/vite`)

In `react-web` and `electron-react` applications, `devkitWorkspaceSource()` resolves internal `packages/*` TypeScript, TSX, and CSS entrypoints directly from `src/` during `vite serve` (`make dev`), while leaving `vite build` (`make build`) and Vitest (`make test`) pointed at compiled `dist/` outputs:

```js
import { devkitWorkspaceSource } from '@docouno/env/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [devkitWorkspaceSource(), react()],
});
```

## Source and Package Verification

Run the installed `devkit-verify` from the component, or pass `--root PATH`. `--json` returns a structured result, and `--pack` additionally validates the built npm tarball in a temporary staging directory (safe on read-only `/workspace` mounts during `make build`):

```sh
devkit-verify
devkit-verify --pack
```

For `"private": false` libraries, `devkit-pack` (invoked by `make package`) stages `npm pack --ignore-scripts`, checks public `exports`, blocks leaked `src/`/test/config files and test ambient `/// <reference types="..." />` directives, runs `publint` (`strict: true`) and `@arethetypeswrong/core` (`node16-esm` and `bundler`), and atomically writes `<name>-<version>.tgz` and `package-receipt.json` into `release/`. Programmatic helpers (`verifyPackableComponent`, `packComponent`) are exported from `@docouno/env/pack`.

Source defaults can be customized in `package.json`:

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

Declared source requires strict compiler and lint coverage. Cross-app imports, library-to-app imports, private library paths, and runtime imports from `devDependencies` inside `packages/*/src` fail verification (and public `"private": false` libraries also forbid exported type-only imports from undeclared `devDependencies`).

## Test Reporters

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

Both profiles require at least one runnable registered test case; empty, skipped-only, and TODO-only suites fail.

## License

[MIT](LICENSE).
