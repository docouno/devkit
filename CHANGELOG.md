# Changelog

## [Unreleased]

## [0.4.0] - 2026-11-12

### Added

- Standalone and workspace npm library templates (`ts-library` and `react-ui` bundled with `tsdown`), repositories with `0..N` applications and `1..M` packages, repeatable `init --package NAME=TEMPLATE` and `--use PKG` flags, and `--preset design-system` (`packages/ui` + `apps/playground`).
- Offline library packaging via `make package` (`devkit-pack` / `devkit-verify --pack`) validating export maps, `publint`, and `@arethetypeswrong/core` before emitting `.tgz` archives and `package-receipt.json`.
- `devkitWorkspaceSource` Vite plugin (`@docouno/env/vite`) for instant source-level HMR on internal `packages/*` libraries during `make dev` without rebuilding `dist/`.
- `devkit sync` to reconcile Make/Compose projections and `package-lock.json` after manual `package.json` or `.npmrc` edits, plus three-stage `--reconcile-lock` recovery on `sync`, `update`, `add-app`, `add-package`, and `register`.
- Support for `npm:` dependency aliases across workspace graphs and private HTTPS npm registries (including GitLab Package Registry subpath endpoints) with `${ENV_VAR}` token expansion, custom TLS root CAs (`DEVKIT_CA_CERT`), and strict secret isolation in offline and Rust containers.
- Cumulative migration notes in `devkit update`, forward-dispatch to newer target or accepted engines, managed `.devkit/runtime/GUIDE.md`, and complete standalone documentation under `docs/`.
- Adaptive container resource budgeting (`HostResourceBudget` with `DEVKIT_CPUS` and `DEVKIT_MEMORY_LIMIT` overrides), automatic cgroup v2 delegation probing and DinD fallback, Compose service resource overlays, and retroactive Vitest worker and V8 isolate heap bounding via `RunnableTestsReporter.onInit`.
- Lock-free `devkit doctor [TARGET] [--json]` host/DinD/cgroup diagnostics, `devkit status [TARGET] [--json]` structured service and URL discovery, enriched zero-Docker `devkit describe [TARGET] [--json]` (`composeProject`, component `selector`, `packageable`, `exposedContainerPort`, and local `receipts`), and automatic `http://127.0.0.1:<port>` output after `make dev` / `make up`.
- Shared and adaptive checkout locking (`LOCK_SH` for concurrent read-only commands, `.devkit/state/task.owner.json` wait queuing up to `DEVKIT_LOCK_TIMEOUT_MS`, and lock-free log streaming in `make logs follow=1`), plus single-command remediation hints across CLI, Make, and `@docouno/env` error messages.

### Changed

- Accelerate Rust `make check` by compiling workspace targets once (`cargo clippy --all-targets` + `cargo test -- --list`) without executing runtime tests twice, scale `CARGO_BUILD_JOBS` from cgroup v2 CPU quotas, and switch `docker compose up` from `--pull never` to `--pull missing` so backing services pull missing images on demand without re-pulling cached images.
- Harden `make setup` supply-chain isolation by running `npm ci --ignore-scripts` during networked package extraction and executing any declared workspace component install hooks (`preinstall`, `install`, `postinstall`, `prepare`) in a subsequent `--network none` step without registry credentials.
- Adopt file-glob ESLint and Prettier scripts and single-pass `check` scripts across Node and React templates.
- Generate GitHub release descriptions from the matching version entry in this changelog while keeping build contracts and verification receipts in a collapsed section.

## [0.3.2] - 2026-10-24

### Fixed

- Wait for npm to expose a successful `latest` update before completing release promotion.
- Compact verification receipts without dropping their measurements or integrity bindings.
- Keep repeated release-note updates stable and preserve surrounding operator notes.

## [0.3.1] - 2026-10-24

### Fixed

- Wait for newly published npm versions to become readable instead of failing immediately after a successful publish.
- Require an existing stable version before the normal candidate-to-latest release flow.

### Verification

- Add a public native-to-native upgrade check, including accepted-version pins, source preservation and recovery.

## [0.3.0] - 2026-10-24

### Added

- A standalone Rust CLI for project creation, component registration, environment updates, recovery and daily orchestration.
- Direct npm and curl entry points, an interactive wizard and JSON output for automation.
- A verified release cache and explicit accepted-version pins for each project.
- Containerized native command execution for editing dependencies with the accepted toolchain.

### Changed

- Replace the TypeScript/Bash management core and Node control containers with the native CLI.
- Preserve the root Makefile's task graph and product-owned source, dependencies and configuration during updates.
- Build and verify one frozen artifact set in GitHub Actions before publishing and promoting it.

### Upgrade

- Use this release's native npm/curl entry point to upgrade 0.2.0 or 0.2.1 projects. Older versioned bootstrap scripts retain their original behavior.

## [0.2.1] - 2026-10-23

### Fixed

- Use the public GitHub release endpoint by default in the bootstrap.
- Correct curl protocol-option quoting for Bash and zsh.
- Add repository, homepage and issue links to the environment package.

### Verification

- Verify actual GitHub downloads, npm integrity and explicit environment upgrades through the public distribution path.

## [0.2.0] - 2026-10-23

### Added

- Modular Node API, Rust API, React web and Electron templates, plus a shared React client package.
- A fullstack desktop preset combining a Rust API, web, Electron and shared client code.
- Pinned container tooling, common Make commands and explicit environment updates with conflict detection and recovery.

### Platform

- Linux with local Docker Compose. Desktop packaging produces a Linux directory and archive.
