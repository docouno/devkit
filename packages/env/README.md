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

During serve, the plugin also merges exact preview hosts from Devkit's read-only metadata file, even when no local packages or project manifest exist. Manual launches can use a concrete `DEVKIT_PREVIEW_URL`, comma-separated `DEVKIT_ALLOWED_HOSTS`/`VITE_ALLOWED_HOSTS` (explicit leading-dot suffixes allowed), or a bounded exact `DEVKIT_PREVIEW_HOSTS` list. Wildcards and URLs in host lists fail validation. Explicit `server.allowedHosts: true` remains the user's opt-in.

String `/api` proxy targets and an absent `/api` with `DEVKIT_API_TARGET` gain `changeOrigin: true` and `ws: true`. The automatic proxy rejects hostile WebSocket Host headers before forwarding. User proxy objects and other routes retain their functions/options. Build and Vitest runs do not read preview files or install server/client patches.

Default browser HMR routing on tested Vite 8.3.5/8.3.8 clients receives a guarded first-handshake recovery wrapper. It waits for an actual `connected` payload, uses one shared recovery attempt and at most one automatic reload per server token, and clears the budget after connection. Ordinary connected-session restart behavior stays native. Explicit custom HMR routing and workers retain the native transport; unknown versions/fragments produce one actionable warning.

For `"private": false` libraries, `devkit-pack` (invoked by `make package`) stages `pnpm pack --config.ignore-scripts=true`, checks public `exports`, blocks leaked `src/`/test/config files and test ambient `/// <reference types="..." />` directives, runs `publint` (`strict: true`) and `@arethetypeswrong/core` (`node16-esm` and `bundler`), and atomically writes `<name>-<version>.tgz` and `package-receipt.json` into `release/`. Programmatic helpers (`verifyPackableComponent`, `packComponent`) are exported from `@docouno/env/pack`.

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
