# Upgrading Devkit Environments

Devkit pins each repository's accepted environment in `.devkit/manifest.json` and `.devkit/runtime/toolchains.env`. Installing a newer CLI globally or invoking a newer version via `npx` never mutates an existing project implicitly — upgrades are explicit, reviewable Git commits.

## Standard Upgrade Flow

Run upgrades on a dedicated Git branch:

```sh
git switch -c chore/devkit-0.6.0

# 1. Inspect planned file changes and applicable migration notes without modifying files (add --json for file hashes and modes)
npx @docouno/devkit@0.6.0 update . --to 0.6.0 --preview

# 2. Apply the environment update
npx @docouno/devkit@0.6.0 update . --to 0.6.0

# 3. Prepare updated containers/dependencies and run all gates
make setup
make check
make test
make build
```

Review the diff, including new root workspace files and removed component-local files, and commit the whole change together.

## Supported upgrade window

Devkit `0.6.0` supports direct upgrades from `0.5.0` and `0.5.1`. The minimum accepted version is recorded in `release.json` as `minSupportedFromVersion`. Older checkouts are rejected before acquiring locks or changing files.

| Accepted checkout  | Next step                                                                                                                        |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------------- |
| `0.5.x`            | Preview the update to `0.6.0`, then apply it. On Linux ARM64, invoke `@docouno/devkit@0.6.0` directly rather than `make update`. |
| `0.4.x`            | On Linux x86-64, run `npx @docouno/devkit@0.5.1 update . --to 0.5.1`, run setup and checks, and commit that migration first.     |
| `0.2.x` or `0.3.x` | On Linux x86-64, reach `0.4.1` with `npx @docouno/devkit@0.4.1 update . --to 0.4.1`, verify it, then bridge through `0.5.1`.     |

The loader checked into a `0.5.x` repository requires x86-64. On Linux ARM64, create the upgrade branch with `git switch -c` or `wt switch --create --no-hooks` so Worktrunk does not invoke that loader, then run the `0.6.0` npm command above or the versioned installer.

For a schema 1 checkout, first use the published `0.2.1` bootstrap on Linux x86-64 to migrate to schema 2. Run these commands from the checkout on your upgrade branch, verify the result, then follow the `0.2.x` steps above.

```sh
bootstrap=$(mktemp)
curl -fsSL https://github.com/docouno/devkit/releases/download/v0.2.1/bootstrap.sh -o "$bootstrap"
bash "$bootstrap" update "$PWD" --version 0.2.1
make setup
make check
```

## Workspace migration and file ownership

On `0.5.x` and later on Linux x86-64, the checkout's Makefile can preview and apply the update.

```sh
make update to=0.6.0 preview=1
make update to=0.6.0
```

Rust projects gain a virtual root Cargo workspace. Existing crate names, dependencies and selected versions carry forward. The migration combines compatible component locks into one root `Cargo.lock`, moves workspace-wide Cargo settings and formatting to the root, and installs the target release's root `rust-toolchain.toml`. Preview and apply both run pinned Cargo in Docker when the combined graph needs verification beyond the known templates or simple local dependencies. If the old components disagree, preview identifies the paths to reconcile on `0.5.x`. The old build directories remain local to the checkout and can be removed once the new build has succeeded. See the [Cargo migration guide](migrations/0.6.0.md#adopt-the-root-cargo-workspace).

On upgrade to `0.6.0`, projects without a registered Electron component also drop the managed desktop Dockerfile, seccomp profile, associated notices, desktop recipe pin, and desktop tool Compose service. Adding or registering the first Electron app restores them in the same transaction as registration.

Updates also refresh managed runtime files, Make/Compose projections, and the accepted Node tool dependencies and pnpm workspace lock where applicable. Product source, root `Makefile`, `compose.yaml`, registry settings and project-owned environment files stay under your control. Devkit checks managed file identities before applying changes.

## Transitive Lockfile Reconciliation (`--reconcile-lock`)

By default, `devkit update` preserves previously resolved transitive versions in `pnpm-lock.yaml` where compatible. When a jump across `@docouno/env`, TypeScript, Vite, or ESLint majors causes strict peer-dependency conflicts with stale transitive entries in `pnpm-lock.yaml`, re-run with `--reconcile-lock`:

```sh
npx @docouno/devkit@0.6.0 update . --to 0.6.0 --reconcile-lock
```

`--reconcile-lock` executes a three-stage fallback strategy:

1. **`seeded`**: Standard lockfile refresh keeping existing transitive resolutions.
2. **`prune-dev`**: Prunes graph nodes reachable only from development dependencies while keeping production `dependencies` pinned, then re-resolves dev tooling.
3. **`clean-fallback`**: Regenerates `pnpm-lock.yaml` cleanly from workspace `package.json` manifests while asserting that every product dependency spec is preserved.

When `--reconcile-lock` is used, Devkit prints a concise summary of changed, added, and removed lockfile packages (and includes `lockReconciliation` in `--json` output).

Reconciliation may select different external package versions while retaining direct dependency declarations and local workspace target paths. Preview the report before applying it.

## Cumulative Migration Notes

When upgrading across supported releases (for example `0.5.1 -> 0.6.0`), `devkit update` filters `release.json` migration entries by the version range `(old, target]` and your project's registered component profiles, printing short migration notes with both a local offline path (`~/.cache/docouno/devkit/releases/<ver>/<id>/docs/migrations/...`) and a GitHub link.

Because Devkit never overwrites product-owned `scripts` or configuration files silently, optional improvements to product-owned files are documented in `docs/migrations/<version>.md` whenever a release introduces them.

## Cross-release dispatch

- Daily and workspace-scoped commands (`devkit exec`, `devkit task`, `devkit sync`, `devkit add-app`, `devkit add-package`, and `devkit register`) verify and delegate to the project's accepted `0.5.0+` engine for the current host whenever it differs from the running binary.
- `devkit stand` (`reserve` / `remove`) selects the cached accepted `0.5.0+` engine locally without network access and refuses before locking on checkouts older than `0.5.0`. If another process is installing the accepted engine, retry the stand command once installation finishes.
- Read-only `devkit describe` and `devkit status` delegate when the accepted engine is already cached locally and remain strictly offline otherwise; `devkit info` uses a cached accepted `0.5.0+` engine offline and returns a local summary if the engine is unavailable or its cache lock is busy, while `devkit doctor` runs lock-free against the host and checkout directly.
- `devkit init --to <version>` and `devkit update --to <version>` (for `<version> >= 0.5.0`) resolve and verify the target release first; whenever `<version>` differs from the currently running `devkit` binary, execution delegates symmetrically to the verified `<version>` engine. When a release has no engine for the current host, use a release that provides one — to migrate an x86-64-only `0.5.x` checkout on Linux ARM64, start the update directly with the `0.6.0` CLI.

### Choosing a CLI for headless projects

Use the checkout's Make targets, `sh .devkit/runtime/devkit.sh cli <command> ...`, or an external CLI at `0.6.0` or newer. Headless checkouts from `0.6.0` omit `DEVKIT_DESKTOP_RECIPE_HASH`. External engines through `0.5.1` expect that pin. In `0.5.1`, `add-app`, `add-package`, `register`, `sync`, `exec`, `task`, `describe`, `status`, `stand reserve` and `stand remove` fail before dispatch. The old `doctor` reports `Missing toolchain pin`, and `info` reports readiness as `unknown`.

## Recovering Interrupted Updates (`devkit recover`)

Updates use a local transaction journal at `.devkit/update-pending.json`. After an interruption, run `make recover` from the affected checkout. Devkit verifies the recorded file identities before restoring the pre-update bytes and modes. A second recovery reports that there is no pending transaction.

If the `0.5.x` loader is still in place, follow the [0.6.0 offline recovery instructions](migrations/0.6.0.md#recover-an-interrupted-update) to locate and run the verified engine in your configured cache. An already installed `0.6.0` CLI can also run `devkit recover .`. For an older interrupted release, use the version that wrote its journal.
