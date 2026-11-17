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

- **Node components** share one root pnpm workspace (`pnpm-workspace.yaml` + `pnpm-lock.yaml`) with `nodeLinker: isolated` and `packageImportMethod: hardlink`. Package names, versions, `"private": true | false`, `exports`, `files`, `publishConfig`, `dependencies` (including `npm:` aliases), and `peerDependencies` live exclusively in `{component}/package.json`.
  Devkit commands use the release's verified pnpm executable and container Node runtime with automatic manager and SDK installation disabled; user-owned `packageManager` and `devEngines` metadata is preserved.
- **Rust components** own `{component}/Cargo.toml` and `{component}/Cargo.lock`. A single registered Rust component (for example `apps/api`) can organize multiple internal crates using a native Cargo `[workspace]` inside `apps/api/Cargo.toml` (`members = [".", "crates/*"]`) while preserving a single `{component}/rust-toolchain.toml` and `{component}/Cargo.lock`.

When manual edits to native manifests affect workspace projections or lockfile metadata, `devkit sync` reconciles `.devkit/*` projections and `pnpm-lock.yaml` transactionally without changing the accepted Devkit release.

Local directory snapshots retain their exact artifact and peer-context identities in the lock graph. Component dependency checks use their physical directories, so application dependencies, library build ordering, and cycle detection apply equally to directory snapshots and workspace links.

pnpm YAML acquisition is limited to 32 MiB before allocation throughout admission, source snapshots, staging, and transaction recovery. Saved transaction files retain the policy of their original target path. Git worktree pointers used to select a shared store are regular files limited to 16 KiB before reading or hashing.

## Container Isolation and Security Model

Every task executes inside digest-pinned Linux containers (`DEVKIT_NODE_IMAGE`, `DEVKIT_RUST_IMAGE`, `DEVKIT_DESKTOP_IMAGE`) with dropped capabilities (`--cap-drop ALL`), `no-new-privileges:true`, `--init`, and host UID/GID mapping:

1. **Read-only offline gates (`check`, `test`, `build`)**:
   - `/workspace` is bind-mounted read-only.
   - Component build directories (`dist/`, `target/`) are mounted writable only where needed.
   - Network access is disabled (`--network none`).
2. **Isolated library packaging (`package` for `ts-library` and `react-ui`)**:
   - Runs under `--network none` with `/workspace`, `node_modules`, and `{component}/dist` mounted read-only.
   - Only `{component}/release` (and `/tmp`) is writable, ensuring `pnpm pack`, `publint`, and `@arethetypeswrong/core` validate the exact `dist/` produced by `build` without mutating build outputs or workspace state.
3. **Explicit write sessions (`make fix`, `devkit exec --write`)**:
   - Formatting (`make fix`) mounts source writable under `--network none`.
   - Dependency mutations (`devkit exec --component ... --write --network bridge`) allow explicit registry access and invalidate `.devkit/state/node-workspace.lock` so the next `make setup` verifies the updated lockfile cleanly.
4. **Registry secret and TLS CA isolation**:
   - Root `.npmrc` permits only HTTPS registries (`http://` is permitted solely on loopback `127.0.0.1` / `localhost` for local testing) and `${ENV_VAR}` placeholders — literal `_authToken`, `_auth`, `_password`, `token`, `certfile`, `keyfile`, and `strict-ssl=false` are rejected before container startup.
   - Referenced `${ENV_VAR}` tokens and `DEVKIT_CA_CERT` (`NODE_EXTRA_CA_CERTS=/devkit/ca.pem`) are injected **only** into networked Node containers (`make setup`, `devkit sync`, `devkit update`, and `devkit exec --network bridge`).
   - Offline Node containers receive dummy placeholder values (`devkit-offline-unused`) and no CA mount; Rust containers never receive npm registry tokens or `NODE_EXTRA_CA_CERTS`.
   - `DEVKIT_CA_CERT` extends the pinned Mozilla roots for native HTTPS and standalone pnpm. The native downloader preserves the replacement semantics of an explicit `SSL_CERT_FILE`.
   - Implicit `.pnpmfile.cjs`, `.pnpmfile.mjs`, and `.pnpmfile.js` hooks are unsupported and rejected before container startup. Automatic pnpm operations also disable these hooks; project installation scripts run in the separate offline phase.

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

## Host-local development stand authority

Tracked Make/Compose projections describe components and port intent. A checkout-owned, ignored stand override supplies host leases, ownership labels, preview metadata and inspected image pins. Checkout locking precedes the shared cache registry lock; Docker, image builds and network operations never execute under the registry lock. Durable local intent precedes global claims and container creation. Physical checkout identity plus an incarnation nonce prevents a recreated directory from inheriting old ownership. Controlled transactions transfer the verified epoch; unexplained state cannot become fresh by deleting a marker.

Stand observations are scoped to the Compose project and verify exact ownership before removal or socket reuse. Ordinary down retains leases. Cleanup releases only after owned containers and inspection helpers are proven absent, without deleting service volumes. Unknown/unfinished creators keep recovery state. An independent receipt records possible actors before creation, so a missing ledger cannot be treated as empty history. Desired deployments and container-labelled applied deployments retain immutable host files until their last reference disappears.

Stand history survives changes to the current component model. Status and cleanup still observe the recorded project when the accepted tree has only libraries and no project Compose file. Only a verified empty actor history permits cleanup without Docker.

Inspection acquisition records client completion separately from uncertain daemon outcomes. Interrupted or failed acquisition observes the complete planned helper set for late creates; a missing required outcome retains its intent and actor receipt. Observed helpers and anonymous volumes are removed by verified immutable identity before that history can become empty.

Preview host authority comes from registered frontend profiles and final normalized TCP bindings. A bounded image-only acquisition phase follows runtime volume providers and build-context dependencies, then proves a literal metadata slot against pinned image ancestry and all mount providers. The launch overlay disables build/pull for inspected images, pins external provider IDs, and is normalized again before up. Status validates observed image, reserved environment, read-only bind and file digest against the applied deployment.

Metadata proof shares a 30-second deadline across external-provider reads, helper and image inspection, namespace reads and retained-container observations. Each daemon metadata command has at most three seconds and bounded output. Image pull/build acquisition time is excluded; cancellation and exact helper recovery still apply.

The namespace model distinguishes a receiver's own mounts from Docker mountpoints inherited through `volumes_from`. Provider tmpfs and devices are not inherited and cannot mask a source-backed ancestor for the receiver. A receiver's own deeper writable tmpfs can still provide a safe literal metadata slot.

`development.api` is a service relationship, not an import edge. Modern projections add required startup ordering; the development Make DAG includes both frontend and connected API library builds while preserving native library dependency ordering. Historical `0.4.x` projection bytes remain compatible.
