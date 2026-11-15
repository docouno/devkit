# Upgrading Devkit Environments

Devkit pins each repository's accepted environment in `.devkit/manifest.json` and `.devkit/runtime/toolchains.env`. Installing a newer CLI globally or invoking a newer version via `npx` never mutates an existing project implicitly — upgrades are explicit, reviewable Git commits.

## Standard Upgrade Flow

Run upgrades on a dedicated Git branch:

```sh
git switch -c chore/devkit-0.5.0

# 1. Inspect planned file changes and applicable migration notes without modifying files
npx @docouno/devkit@0.5.0 update . --to 0.5.0 --preview

# 2. Apply the environment update
npx @docouno/devkit@0.5.0 update . --to 0.5.0

# 3. Prepare updated containers/dependencies and run all gates
make setup
make check
make test
make build
```

Review `git diff` and commit the updated `.devkit/*`, component `devDependencies` (`toolDependencies`), and `package-lock.json`.

## Supported Upgrade Window and `0.4.1` Bridge

Devkit `0.5.0` declares `minSupportedFromVersion: "0.4.0"` in `release.json` and supports direct upgrades from `0.4.0` and `0.4.1` checkouts. On checkouts older than `0.4.0` (or when `--to` targets a release below `0.4.0`), `devkit` `0.5.0` refuses before acquiring checkout locks or modifying files on disk.

To upgrade an archival `0.2.x` or `0.3.x` repository to `0.5.0`, bridge through the immutable `0.4.1` release first:

```sh
git switch -c chore/devkit-0.5.0

# Step 1: upgrade pre-0.4.0 checkout to 0.4.1
npx @docouno/devkit@0.4.1 update . --to 0.4.1
make setup
make check

# Step 2: upgrade 0.4.1 checkout to 0.5.0
npx @docouno/devkit@0.5.0 update . --to 0.5.0
make setup
make check
```

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
npx @docouno/devkit@0.5.0 update . --to 0.5.0 --reconcile-lock
```

`--reconcile-lock` executes a three-stage fallback strategy:

1. **`seeded`**: Standard lockfile refresh keeping existing transitive resolutions.
2. **`prune-dev`**: Prunes dev-only lockfile nodes (`"dev": true`) while keeping production `dependencies` pinned, then re-resolves dev tooling.
3. **`clean-fallback`**: Regenerates `package-lock.json` cleanly from workspace `package.json` manifests while asserting that every product dependency spec is preserved.

When `--reconcile-lock` is used, Devkit prints a concise summary of changed, added, and removed lockfile packages (and includes `lockReconciliation` in `--json` output).

## Cumulative Migration Notes

When upgrading across supported releases (for example `0.4.1 -> 0.5.0`), `devkit update` filters `release.json` migration entries by the version range `(old, target]` and your project's registered component profiles, printing short migration notes with both a local offline path (`~/.cache/docouno/devkit/releases/<ver>/<id>/docs/migrations/...`) and a GitHub link.

Because Devkit never overwrites product-owned `scripts` or configuration files silently, optional improvements to product-owned files are documented in `docs/migrations/<version>.md` whenever a release introduces them.

## Cross-Release Dispatch (`>= 0.4.0`)

- Daily and workspace-scoped commands (`devkit exec`, `devkit task`, `devkit sync`, `devkit add-app`, `devkit add-package`, and `devkit register`) verify and delegate to the project's accepted `0.4.x` or `0.5.0+` engine whenever it differs from the running binary. Read-only `devkit describe` and `devkit status` delegate when the accepted engine is already cached locally and remain strictly offline otherwise, while `devkit doctor` runs lock-free against the host and checkout directly.
- `devkit init --to <version>` and `devkit update --to <version>` (for `<version> >= 0.4.0`) resolve and verify the target release first; whenever `<version>` differs from the currently running `devkit` binary, execution delegates symmetrically to the verified `<version>` engine.
- On Linux hosts using a group-writable `umask 0002`, prefer `0.4.1+` (or the checked-in project loader `sh .devkit/runtime/devkit.sh cli <subcommand> ...`) when running daily `0.4.x` commands across releases; `devkit update . --to 0.5.0` works directly from both `0.4.0` and `0.4.1`.

## Recovering Interrupted Updates (`devkit recover`)

Updates commit through a crash-safe staging journal (`.devkit/update-pending.json`). If a process crash or power loss interrupts an update mid-commit:

```sh
npx @docouno/devkit@0.5.0 recover .
```

`devkit recover` verifies backed-up file digests and rolls the repository back to its exact pre-update state without requiring network access or Docker (for an interrupted pre-`0.4.0` update journal, run `npx @docouno/devkit@0.4.1 recover .`).
