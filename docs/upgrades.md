# Upgrading Devkit Environments

Devkit pins each repository's accepted environment in `.devkit/manifest.json` and `.devkit/runtime/toolchains.env`. Installing a newer CLI globally or invoking a newer version via `npx` never mutates an existing project implicitly — upgrades are explicit, reviewable Git commits.

## Standard Upgrade Flow

Run upgrades on a dedicated Git branch:

```sh
git switch -c chore/devkit-0.4.0

# 1. Inspect planned file changes and applicable migration notes without modifying files
npx @docouno/devkit@0.4.0 update . --to 0.4.0 --preview

# 2. Apply the environment update
npx @docouno/devkit@0.4.0 update . --to 0.4.0

# 3. Prepare updated containers/dependencies and run all gates
make setup
make check
make test
make build
```

Review `git diff` and commit the updated `.devkit/*`, component `devDependencies` (`toolDependencies`), and `package-lock.json`.

## What `devkit update` Changes vs Preserves

- **Updated automatically**:
  - `.devkit/runtime/**`, `.devkit/project.mk`, `.devkit/compose.*.yaml|json`, `.devkit/tooling-profiles.json`, `.devkit/runtime.sha256`
  - `{rust-component}/rust-toolchain.toml`
  - Managed `toolDependencies` entries inside `{node-component}/package.json` `devDependencies`
  - Root `package-lock.json`
- **Preserved untouched**:
  - Component source (`src/**`, `test/**`, `tests/**`)
  - Product fields, `scripts`, `dependencies`, `peerDependencies`, and product-added `devDependencies` in `{component}/package.json`
  - `{rust-component}/Cargo.toml` and `{rust-component}/Cargo.lock`
  - Root `Makefile`, `compose.yaml`, `.npmrc`, and `.env`

If a managed file under `.devkit/` was edited manually, `devkit update` aborts before modifying the repository so local changes are never overwritten silently.

## Transitive Lockfile Reconciliation (`--reconcile-lock`)

By default, `devkit update` preserves previously resolved transitive versions in `package-lock.json` where compatible. When a jump across `@docouno/env`, TypeScript, Vite, or ESLint majors causes strict peer-dependency conflicts with stale transitive entries in `package-lock.json`, re-run with `--reconcile-lock`:

```sh
npx @docouno/devkit@0.4.0 update . --to 0.4.0 --reconcile-lock
```

`--reconcile-lock` executes a three-stage fallback strategy:

1. **`seeded`**: Standard lockfile refresh keeping existing transitive resolutions.
2. **`prune-dev`**: Prunes dev-only lockfile nodes (`"dev": true`) while keeping production `dependencies` pinned, then re-resolves dev tooling.
3. **`clean-fallback`**: Regenerates `package-lock.json` cleanly from workspace `package.json` manifests while asserting that every product dependency spec is preserved.

When `--reconcile-lock` is used, Devkit prints a concise summary of changed, added, and removed lockfile packages (and includes `lockReconciliation` in `--json` output).

## Cumulative Migration Notes

When upgrading across versions (for example `0.3.2 -> 0.4.0`), `devkit update` filters `release.json` migration entries by the version range `(old, target]` and your project's registered component profiles, printing short migration notes with both a local offline path (`~/.cache/docouno/devkit/releases/<ver>/<id>/docs/migrations/...`) and a GitHub link.

Because Devkit never overwrites product-owned `scripts` or configuration files silently, optional improvements to product-owned files are documented in `docs/migrations/<version>.md`. When upgrading `0.3.x` projects to `0.4.0`, `@docouno/env`'s `RunnableTestsReporter` automatically applies cgroup-aware Vitest worker and V8 isolate heap bounds during `make test` even when product-owned `vitest.config.mjs` files remain untouched.

## Forward-Dispatch Across Releases

- Daily and workspace-scoped commands (`devkit exec`, `devkit task`, `devkit describe`, `devkit status`, `devkit sync`, and forward-version `add-app`, `add-package`, `register`) automatically dispatch to the project's accepted cached engine when pinned to a newer release, while `devkit doctor` runs lock-free against the host and checkout directly.
- `devkit update --to <version>` resolves and verifies the target release first; when `<version>` is newer than the currently running `devkit` binary, it delegates execution of the update to the verified `<version>` engine automatically.

## Recovering Interrupted Updates (`devkit recover`)

Updates commit through a crash-safe staging journal (`.devkit/update-pending.json`). If a process crash or power loss interrupts an update mid-commit:

```sh
npx @docouno/devkit@0.4.0 recover .
```

`devkit recover` verifies backed-up file digests and rolls the repository back to its exact pre-update state without requiring network access or Docker.
