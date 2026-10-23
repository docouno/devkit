# Devkit environment

Reusable ESM configuration and source verification for Devkit applications. Standard tools remain native application dependencies; this package does not locate or dispatch their binaries.

The supported initial suite is Node 24.18, ESLint 10.13, TypeScript 6.0 and Prettier 3.9. The exact accepted versions and application lockfiles belong to a Devkit release. TypeScript 7 is outside this package's peer range.

## Configuration

An application owns its native files. In its `eslint.config.mjs`:

```js
import { nodeConfig } from '@docouno/env/eslint';
export default nodeConfig(import.meta.dirname);
```

Use `browserConfig` for React/browser code. Both factories require the absolute application root. They enable typed linting and reject inline ESLint disable directives. Source-specific exceptions therefore remain visible configuration changes rather than local comments.

An application TSConfig extends `@docouno/env/tsconfig/node.json` or `@docouno/env/tsconfig/browser.json`. These configs contain compiler settings and no application source/output paths. Set `include`, `exclude`, `rootDir`, `outDir` and any runtime-specific types in the application. The Node config includes Node types; the browser config keeps DOM types separate.

In `prettier.config.mjs`:

```js
export { default } from '@docouno/env/prettier';
```

## Declared source verification

Run the installed `devkit-verify` command from the application, or pass `--root <application>`. `--json` emits its structured result. Success returns 0; metadata, coverage or policy failures return nonzero. It validates coverage and configuration; native lint, compiler and test commands still run separately.

Optional application metadata in `package.json`:

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

These values are the defaults. Unknown options, parent traversal, source symlinks, empty source directories and unreadable configuration fail explicitly. Every declared JS/TS semantic file must be included in a declared compiler config; JavaScript also requires `checkJs`. Files intentionally outside these directories are not implicitly promised covered. Keep synthetic invalid source payloads outside declared semantic roots.

Typechecking must remain strict and cannot use `noCheck` or a leading `ts-nocheck` directive. Coverage follows the compiler's actual import graph, not only its initial root file list. Authored declaration files are not claimed checked when `skipLibCheck` skips them. Every declared TS file must have the typed parser, project context and mandatory type-aware error rules. An ignored file is rejected even when the compiler could accept its code. Multiple `typecheckConfigs` select explicit rooted lint projects; a single ordinary `tsconfig.json` uses the TypeScript project service. List leaf configs rather than only an empty solution config.

Cross-app checking rejects static relative import, re-export, dynamic-import and `require` literals into a sibling under `apps/`. It is not a runtime dependency graph or a proof about computed module paths. The policy can be disabled explicitly. Directory bans are opt-in basenames; no private naming scheme or legacy `spec` link is embedded.

Use `eslint --no-ignore --max-warnings 0` on the declared source roots, a separate `tsc --noEmit`, and `prettier --check`. Formatting and lint autofix belong to a separate explicit command. Template checks also require real tests; empty test suites are not accepted.

Template formatting commands cover source and human-owned tool configuration. Native `package.json` and lockfiles are machine-managed metadata validated by the schema, package manager and lock checks. Their serialization is not a source-formatting gate, so ordinary dependency updates do not require a formatter before setup.

## Native Node tests

The Node template invokes native `node --test` with `--test-reporter=@docouno/env/node-test-reporter`. The reporter delegates normal output to Node's spec reporter and requires at least one runnable registered `node:test` case across file summaries. Empty modules and skipped/TODO-only suites fail even when the native runner would report successful test files. This profile requires the `node:test` registration API; custom top-level-assertion harnesses without registrations are not promised covered. Ordinary failed tests preserve their native failure.

## Native Vitest tests

The React template keeps Vitest's default reporter and adds `new RunnableTestsReporter()` from `@docouno/env/vitest-reporter`. With `passWithNoTests: false`, missing and empty modules fail natively; the additional reporter rejects a successful run containing only skipped or TODO cases. Individual skipped tests remain valid alongside a completed test. Failed runs retain their native diagnostics. This export targets the tested Vitest 5.0.5 reporter API through an optional exact peer; Node applications do not need Vitest installed. Reporter interfaces can change within a minor release, so update this compatibility constraint only after testing the actual native CLI.

## Verification

`npm test` runs source-level package tests. Release validation must additionally install the real package archive in a separate consumer without access to this checkout, then exercise its exports and commands. Passing the source tests alone does not establish packaging correctness.
