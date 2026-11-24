# Upgrading Devkit Environments

Devkit pins each repository's accepted environment in `.devkit/manifest.json` and `.devkit/runtime/toolchains.env`. Installing a newer CLI globally or invoking a newer version via `npx` never mutates an existing project implicitly — upgrades are explicit, reviewable Git commits.

## Standard Upgrade Flow

Run upgrades on a dedicated Git branch:

```sh
git switch -c chore/devkit-0.6.0

# 1. Inspect planned file changes and applicable migration notes without modifying files
npx @docouno/devkit@0.6.0 update . --to 0.6.0 --preview

# 2. Apply the environment update
npx @docouno/devkit@0.6.0 update . --to 0.6.0

# 3. Prepare updated containers/dependencies and run all gates
make setup
make check
make test
make build
```

Review `git diff` and commit the updated `.devkit/*`, component `devDependencies` (`toolDependencies`), and `pnpm-lock.yaml`.

## Supported Upgrade Window

Version `0.6.0` accepts checkouts from `0.5.0` onward. On Linux x86-64, update a `0.5.x` checkout with `make update to=0.6.0 preview=1`, then run it again without `preview=1`. On Linux ARM64, use the `0.6.0` npm command above or the versioned installer. The loader checked into a `0.5.x` repository requires x86-64. Create the upgrade branch with `git switch -c` or `wt switch --create --no-hooks` so Worktrunk does not invoke that loader.

For a `0.4.x` repository, run the following on Linux x86-64 before following the standard upgrade flow. A `0.2.x` or `0.3.x` repository must first update to `0.4.1` on that host.

```sh
npx @docouno/devkit@0.5.1 update . --to 0.5.1
make setup
make check
```

## Files affected by an update

If a `0.4.x` to `0.5.0` update leaves the older loader installed, use the `0.5.0` installer with `recover .` or `npx @docouno/devkit@0.5.0 recover .`. For journals older than `0.5.0` that exceed 4 MiB, run recovery with the pinned release that wrote the journal.

- **Updated automatically**:
  - `.devkit/runtime/**`, `.devkit/project.mk`, `.devkit/compose.*.yaml|json`, `.devkit/tooling-profiles.json`, `.devkit/runtime.sha256`
  - `{rust-component}/rust-toolchain.toml`
  - Managed `toolDependencies` entries inside `{node-component}/package.json` `devDependencies`
  - Root `pnpm-workspace.yaml` membership, the release-day age exception for the accepted `@docouno/env` version, and `pnpm-lock.yaml`
  - Registered internal Node dependencies become `workspace:*` (or `workspace:^` between public libraries); legacy npm linking settings are removed from root `.npmrc` during the 0.5 migration
- **Preserved untouched**:
  - Component source (`src/**`, `test/**`, `tests/**`)
  - Product fields and scripts, external dependencies and peer dependencies, and product-added `devDependencies` in `{component}/package.json`
  - `{rust-component}/Cargo.toml` and `{rust-component}/Cargo.lock`
  - Root `Makefile`, `compose.yaml`, and `.env`; registry routing and environment placeholders in `.npmrc`

If a managed file under `.devkit/` was edited manually, `devkit update` aborts before modifying the repository so local changes are never overwritten silently.

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

Daily commands select the checked-in release and verify the engine for the current host. `describe`, `status`, `info`, and stand cleanup operate offline. `info` can report checkout state while the cache is cold or busy. `doctor` inspects the host and checkout without holding the checkout lock.

`init --to VERSION` and `update --to VERSION` resolve the requested release. When that release has no engine for the current host, use a release that provides one. To migrate an x86-64-only `0.5.x` checkout on Linux ARM64, start the update directly with the `0.6.0` CLI.

## Recovering Interrupted Updates (`devkit recover`)

Updates commit through a crash-safe staging journal (`.devkit/update-pending.json`). If a process crash or power loss interrupts an update mid-commit:

```sh
npx @docouno/devkit@0.6.0 recover .
```

`devkit recover` verifies backed-up file digests and rolls the repository back to its exact pre-update state without requiring network access or Docker (for an interrupted pre-`0.4.0` update journal, run `npx @docouno/devkit@0.4.1 recover .`).
