# Architecture

Devkit provides deterministic, containerized development environments for standalone TypeScript and Rust repositories while keeping product source, build graphs, and release artifacts under native ecosystem conventions.

## Component Graph (`0..N` Apps, `1..M` Packages)

A Devkit repository contains at least one registered component across two directories:

- `apps/<slug>` (`Role::App`): Runnable product entry points (`0..N`). Repositories dedicated to shared libraries or design systems can contain zero applications (`apps/` is optional) or include a lightweight `apps/playground` app.
- `packages/<slug>` (`Role::Library`): Shared or publishable libraries (`1..M` in library-only repos, `0..M` when applications are present).

### Tooling Profiles

| Profile         | Default Template | Role               | Toolchain | Outputs           | Packaging                                               |
| --------------- | ---------------- | ------------------ | --------- | ----------------- | ------------------------------------------------------- |
| `node`          | `node-api`       | `app`              | `node`    | `dist`            | Application build                                       |
| `web`           | `react-web`      | `app`              | `node`    | `dist`            | Vite static bundle                                      |
| `electron`      | `electron-react` | `app`              | `node`    | `dist`, `release` | Linux Electron directory & archive                      |
| `react-library` | `react-client`   | `library`          | `node`    | `dist`            | Internal workspace-only (`"private": true`)             |
| `ts-library`    | `ts-library`     | `library`          | `node`    | `dist`, `release` | ESM + `.d.ts` via `tsdown`; optional `.tgz`             |
| `react-ui`      | `react-ui`       | `library`          | `node`    | `dist`, `release` | React ESM + `.d.ts` + CSS via `tsdown`; optional `.tgz` |
| `rust`          | `rust-api`       | `app` or `library` | `rust`    | `target`          | Cargo binary/library crate                              |

## Native Manifest Authority

`.devkit/manifest.json` records only environment identity (`version`, `releaseIdentity`), managed file digests, component roles/profiles, inter-component DAG edges (`dependencies`), and the exact `toolDependencies` subset managed inside each Node component's `devDependencies`.

Devkit deliberately avoids duplicating native package metadata in `.devkit/manifest.json`:

- **npm components** share a single root npm workspace (`package.json` + `package-lock.json`). Package names, versions, `"private": true | false`, `exports`, `files`, `publishConfig`, `dependencies` (including `npm:` aliases), and `peerDependencies` live exclusively in `{component}/package.json`.
- **Rust components** own `{component}/Cargo.toml` and `{component}/Cargo.lock`. A single registered Rust component (for example `apps/api`) can organize multiple internal crates using a native Cargo `[workspace]` inside `apps/api/Cargo.toml` (`members = [".", "crates/*"]`) while preserving a single `{component}/rust-toolchain.toml` and `{component}/Cargo.lock`.

When manual edits to native manifests affect workspace projections or lockfile metadata, `devkit sync` reconciles `.devkit/*` projections and `package-lock.json` transactionally without changing the accepted Devkit release.

## Container Isolation and Security Model

Every task executes inside digest-pinned Linux containers (`DEVKIT_NODE_IMAGE`, `DEVKIT_RUST_IMAGE`, `DEVKIT_DESKTOP_IMAGE`) with dropped capabilities (`--cap-drop ALL`), `no-new-privileges:true`, `--init`, and host UID/GID mapping:

1. **Read-only offline gates (`check`, `test`, `build`)**:
   - `/workspace` is bind-mounted read-only.
   - Component build directories (`dist/`, `target/`) are mounted writable only where needed.
   - Network access is disabled (`--network none`).
2. **Isolated library packaging (`package` for `ts-library` and `react-ui`)**:
   - Runs under `--network none` with `/workspace`, `node_modules`, and `{component}/dist` mounted read-only.
   - Only `{component}/release` (and `/tmp`) is writable, ensuring `npm pack`, `publint`, and `@arethetypeswrong/core` validate the exact `dist/` produced by `build` without mutating build outputs or workspace state.
3. **Explicit write sessions (`make fix`, `devkit exec --write`)**:
   - Formatting (`make fix`) mounts source writable under `--network none`.
   - Dependency mutations (`devkit exec --component ... --write --network bridge`) allow explicit registry access and invalidate `.devkit/state/node-workspace.lock` so the next `make setup` verifies the updated lockfile cleanly.
4. **Registry secret and TLS CA isolation**:
   - Root `.npmrc` permits only HTTPS registries (`http://` is permitted solely on loopback `127.0.0.1` / `localhost` for local testing) and `${ENV_VAR}` placeholders — literal `_authToken`, `_auth`, `_password`, `token`, `certfile`, `keyfile`, and `strict-ssl=false` are rejected before container startup.
   - Referenced `${ENV_VAR}` tokens and `DEVKIT_CA_CERT` (`NODE_EXTRA_CA_CERTS=/devkit/ca.pem`) are injected **only** into networked Node containers (`make setup`, `devkit sync`, `devkit update`, and `devkit exec --network bridge`).
   - Offline Node containers receive dummy placeholder values (`devkit-offline-unused`) and no CA mount; Rust containers never receive npm registry tokens or `NODE_EXTRA_CA_CERTS`.

## Adaptive Resource Budgeting and Concurrency

Containerized builds and test suites frequently run on shared CI runners or nested Docker-in-Docker environments where host `nproc` and `docker info` exceed the container's real cgroup v2 slice. Devkit computes a per-invocation `HostResourceBudget` before launching tool containers or Compose stacks:

1. **Real quota discovery**: Walks cgroup v2 (`cpu.max`, `cpuset.cpus.effective`, `memory.max`, `memory.high`, `pids.max`, and active working set `memory.current - inactive_file`) alongside CPU affinity and Docker daemon limits. Explicit `DEVKIT_CPUS`, `DEVKIT_MEMORY_LIMIT`, and `DEVKIT_CARGO_BUILD_JOBS` overrides clamp or adjust the computed budget.
2. **Nested DinD cgroup v2 fallback**: Probes whether cgroup v2 domain controllers (`cpu`, `memory`, `pids`) are delegated to the Docker daemon's cgroup hierarchy (and retries reactively once on OCI cgroup v2 admission errors), omitting `--cpus` / `--memory` flags when blocked while keeping all in-process toolchain caps active.
3. **Coordinated toolchain concurrency**: Translates the container budget into deterministic runtime environment variables (`CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, `RAYON_NUM_THREADS`, `GOMAXPROCS`, `UV_THREADPOOL_SIZE`, `ROLLDOWN_WORKER_THREADS`, `DEVKIT_TEST_WORKERS`, and `DEVKIT_WORKER_MAX_OLD_SPACE_SIZE`, plus fallback `TOKIO_WORKER_THREADS` when CPU cgroup limits are unavailable and fallback `NODE_OPTIONS=--max-old-space-size=<MiB>` when memory cgroup limits are unavailable). In `@docouno/env`, `RunnableTestsReporter.onInit` also bounds Vitest worker counts and fork isolate heaps automatically — including on upgraded projects that retain an unmodified `vitest.config.mjs`.

## Shared and Adaptive Checkout Locking

`.devkit/state/task.lock` coordinates concurrent CLI and Make invocations without blocking parallel read-only inspection:

- **Lock-free**: `devkit doctor`, `devkit catalog`, `devkit --version`, and `devkit --help` never acquire `task.lock`.
- **Shared lock (`LOCK_SH`)**: `devkit describe`, `devkit status`, `make doctor`, `make status`, `make logs` (releasing the lock before `--follow` begins streaming), and read-only `devkit exec` run concurrently across terminals and automation agents.
- **Exclusive lock (`LOCK_EX`)**: Mutating and build tasks write `.devkit/state/task.owner.json` (`pid`, `/proc/<pid>/stat` start ticks, operation name) so concurrent invocations wait adaptively up to `DEVKIT_LOCK_TIMEOUT_MS` (default `120000` ms) while detecting recursive self-deadlocks immediately.
