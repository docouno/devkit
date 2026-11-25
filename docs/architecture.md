# Architecture

Devkit provides deterministic, containerized development environments for standalone TypeScript and Rust repositories while keeping product source, build graphs, and release artifacts under native ecosystem conventions.

## Component Graph (`0..N` Apps, `1..M` Packages)

A Devkit repository contains at least one registered component across two directories:

- `apps/<slug>` (`Role::App`): Runnable product entry points (`0..N`). Repositories dedicated to shared libraries or design systems can contain zero applications (`apps/` is optional) or include a lightweight `apps/playground` app.
- `packages/<slug>` (`Role::Library`): Shared or publishable libraries (`1..M` in library-only repos, `0..M` when applications are present).

### Tooling Profiles

| Profile         | Default Template           | Role               | Toolchain | Outputs           | Packaging                                               |
| --------------- | -------------------------- | ------------------ | --------- | ----------------- | ------------------------------------------------------- |
| `node`          | `node-api`                 | `app`              | `node`    | `dist`            | Application build                                       |
| `web`           | `react-web`                | `app`              | `node`    | `dist`            | Vite static bundle                                      |
| `electron`      | `electron-react`           | `app`              | `node`    | `dist`, `release` | Linux Electron directory & archive                      |
| `react-library` | `react-client`             | `library`          | `node`    | `dist`            | Internal workspace-only (`"private": true`)             |
| `ts-library`    | `ts-library`               | `library`          | `node`    | `dist`, `release` | ESM + `.d.ts` via `tsdown`; optional `.tgz`             |
| `react-ui`      | `react-ui`                 | `library`          | `node`    | `dist`, `release` | React ESM + `.d.ts` + CSS via `tsdown`; optional `.tgz` |
| `rust`          | `rust-api`, `rust-library` | `app` or `library` | `rust`    | root `target/`    | Cargo binaries and libraries                            |

## Native Manifest Authority

`.devkit/manifest.json` records environment identity (`version`, `releaseIdentity`), managed file digests, component roles/profiles, inter-component DAG edges (`dependencies`), and the exact `toolDependencies` subset managed inside each Node component's `devDependencies`. A root Cargo workspace can also record `cargoResolution`, which binds the accepted lock to the toolchain, manifests and checkout Cargo configuration. When the lock allows several local directories for one package ID, Devkit records which path Cargo selected. Committing this state with `Cargo.lock` lets an unchanged checkout run `make sync` before setup.

Starting with `0.6.0`, the accepted component profiles determine which desktop assets belong in the checkout. A registered `electron` component adds the desktop Dockerfile, seccomp profile, its Apache license and notice, the desktop recipe pin and the desktop tool service. The release always carries the complete set so registration can use the accepted payload. The checkout records the digests of its rendered pins and Compose files.

Devkit deliberately avoids duplicating native package metadata in `.devkit/manifest.json`:

- **Node components** share one root pnpm workspace (`pnpm-workspace.yaml` + `pnpm-lock.yaml`) with `nodeLinker: isolated` and `packageImportMethod: hardlink`. Package names, versions, `"private": true | false`, `exports`, `files`, `publishConfig`, `dependencies` (including `npm:` aliases), and `peerDependencies` live exclusively in `{component}/package.json`.
  Devkit commands use the release's verified pnpm executable and container Node runtime with automatic manager and SDK installation disabled; user-owned `packageManager` and `devEngines` metadata is preserved.
- **Rust components** share one virtual root `Cargo.toml` and one `Cargo.lock`. The root `rust-toolchain.toml` comes from the accepted release. The project owns `rustfmt.toml` and Cargo manifests. A registered component can own several crates. List each member directory explicitly in `workspace.members`, including nested crates, and give each workspace package a unique name. Keep Cargo configuration in the checkout root.

Cargo's selected local dependency graph determines Make prerequisites. An application or library can depend on a registered library. Edges into another application and inter-component dependency cycles are rejected. For a local dependency outside registered components, list its directory or an ancestor under `workspace.exclude`. Members, `workspace.default-members`, and exclusions use explicit paths. Every directory in `workspace.default-members` must be a workspace member. Exclusions must be disjoint from registered components.

Setup runs one `cargo fetch --locked` for the workspace. Gates use package selectors for the chosen component, including native test discovery. Shared compiled dependencies live in the checkout's root `target/`. Managed Rust development services mount the same directory. Each worktree keeps its own build outputs.

After manual manifest edits, `make sync` reconciles the native lock and the Make/Compose projections in the same transaction.

If `make register` reports a change to an existing Cargo selection, reconcile pending edits in this order.

1. Set aside dependency edits that require the incoming component. Restore the affected dependencies of registered components to their accepted values.
2. If independent edits need to be synchronized first, temporarily move the incoming directory outside `apps/` and `packages/`. Save and remove its entries from `workspace.members` and `workspace.default-members` in the root `Cargo.toml`, run `make sync`, then restore the directory and the exact entries in both lists.
3. Retry `make register` with all intended entries, including nested crate paths. Reapply the deferred dependency edits and run `make sync`.

Local directory snapshots retain their exact artifact and peer-context identities in the lock graph. Component dependency checks use their physical directories, so application dependencies, library build ordering, and cycle detection apply equally to directory snapshots and workspace links.

pnpm YAML acquisition is limited to 32 MiB before allocation throughout admission, source snapshots, staging, and transaction recovery. Saved transaction files retain the policy of their original target path. Git worktree pointers used to select a shared store are regular files limited to 16 KiB before reading or hashing.

## Host and container binaries

A release has one identity across architectures. The installer and npm launcher select a host binary by operating system and CPU and verify its SHA-256 before execution. The release index maps target triples to binary assets. The x86-64 binary retains its historical path, `devkit`, inside the release payload.

Docker tasks execute a static Linux helper selected for the local daemon's platform. On Linux x86-64 and ARM64, the same verified executable can serve both roles when the targets match. Tool images and standalone pnpm follow the daemon architecture. Platform-specific readiness records and caches belong to the checkout's ignored state or to the host cache.

## Container Isolation and Security Model

Every task executes inside digest-pinned Linux containers (`DEVKIT_NODE_IMAGE`, `DEVKIT_RUST_IMAGE`, `DEVKIT_DESKTOP_IMAGE`) with dropped capabilities (`--cap-drop ALL`), `no-new-privileges:true`, `--init`, and host UID/GID mapping:

1. **Read-only offline gates (`check`, `test`, `build`)**:
   - `/workspace` is bind-mounted read-only.
   - Node gates mount component output directories as needed. Rust gates mount the checkout's `target/` writable and mount the shared Cargo cache (`/cache/cargo`) read-only (managed Rust development services and offline Cargo validation also mount `/cache/cargo` read-only).
   - Network access is disabled (`--network none`).
2. **Isolated library packaging (`package` for `ts-library` and `react-ui`)**:
   - Runs under `--network none` with `/workspace`, `node_modules`, and `{component}/dist` mounted read-only.
   - Only `{component}/release` (and `/tmp`) is writable, ensuring `pnpm pack`, `publint`, and `@arethetypeswrong/core` validate the exact `dist/` produced by `build` without mutating build outputs or workspace state.
3. **Explicit write sessions (`make fix`, `devkit exec --write`)**:
   - Rust formatting (`make fix app=NAME` or `package=NAME`) mounts the selected component writable under `--network none` and formats its members.
   - Rust `exec --write` grants checkout-wide write access so Cargo can update the root lock and component manifests together. Read-only Rust exec sends build artifacts to `/tmp/cargo-target`. Run `make setup` after dependency edits to refresh readiness.
4. **Registry secret and TLS CA isolation**:
   - Root `.npmrc` permits only HTTPS registries (`http://` is permitted solely on loopback `127.0.0.1` / `localhost` for local testing) and `${ENV_VAR}` placeholders — literal `_authToken`, `_auth`, `_password`, `token`, `certfile`, `keyfile`, and `strict-ssl=false` are rejected before container startup.
   - Referenced `${ENV_VAR}` tokens and `DEVKIT_CA_CERT` (`NODE_EXTRA_CA_CERTS=/devkit/ca.pem`) are injected **only** into networked Node containers (`make setup`, `devkit sync`, `devkit update`, and `devkit exec --network bridge`).
   - Offline Node containers receive dummy placeholder values (`devkit-offline-unused`) and no CA mount; Rust containers never receive npm registry tokens or `NODE_EXTRA_CA_CERTS`.
   - `DEVKIT_CA_CERT` extends the pinned Mozilla roots for native HTTPS and standalone pnpm. The native downloader preserves the replacement semantics of an explicit `SSL_CERT_FILE`.
   - Implicit `.pnpmfile.cjs`, `.pnpmfile.mjs`, and `.pnpmfile.js` hooks are unsupported and rejected before container startup. Automatic pnpm operations also disable these hooks; project installation scripts run in the separate offline phase.
   - Root `.cargo/config.toml` rejects executable-hook, flag-override, and credential directives (`build.rustc`, `build.rustc-wrapper`, `build.rustc-workspace-wrapper`, `build.rustdoc`, `build.target-dir`, `build.build-dir`, `build.rustflags`, `build.rustdocflags`, `[env]`, `[alias]`, `[http]`, `net.git-fetch-with-cli`, `paths`, `registry.*`/`registries.*` credential/token keys, and `target.*.{runner,linker,rustflags,rustdocflags}`) as well as non-relative/file URLs and root-level or duplicate `include` directives. The shared `CARGO_HOME` cache must not contain Cargo config or credential files.
5. **macOS APFS case-sensitivity boundary**:
   - On case-insensitive macOS APFS volumes, Devkit never mounts a unified ancestor above the checkout root (such as `$HOME`) into `/devkit/unified`, falling back to a checkout-local pnpm store instead so host siblings outside the checkout cannot be accessed through alternate casing over VirtioFS.
   - Electron binary installation (`setup_electron`) runs with `/workspace` mounted read-only, `/devkit/unified` unmounted, checkout secret files masked, and writes restricted to `node_modules` and `/cache/{electron,pnpm}`.
   - On case-insensitive APFS shares, Linux VFS bind-masks over checkout-internal `.env*` files only match exact filename casing; teams requiring strict checkout-internal secret masking during explicit write-and-network sessions (`devkit exec --write --network bridge`) should use a case-sensitive APFS volume or keep sensitive credentials out of checkout `.env` files.

## Adaptive Resource Budgeting and Concurrency

Containerized builds and test suites frequently run on shared CI runners or nested Docker-in-Docker environments where host `nproc` and `docker info` exceed the container's real cgroup v2 slice. Devkit computes a per-invocation `HostResourceBudget` before launching tool containers or Compose stacks:

1. **Real quota discovery**: Walks cgroup v2 (`cpu.max`, `cpuset.cpus.effective`, `memory.max`, `memory.high`, `pids.max`, and active working set `memory.current - inactive_file`) alongside CPU affinity and Docker daemon limits. Explicit `DEVKIT_CPUS`, `DEVKIT_MEMORY_LIMIT`, and `DEVKIT_CARGO_BUILD_JOBS` overrides clamp or adjust the computed budget.
2. **Nested DinD cgroup v2 fallback**: Probes whether cgroup v2 domain controllers (`cpu`, `memory`, `pids`) are delegated to the Docker daemon's cgroup hierarchy (and retries reactively once on OCI cgroup v2 admission errors), omitting `--cpus` / `--memory` flags when blocked while keeping all in-process toolchain caps active.
3. **Coordinated toolchain concurrency**: Translates the container budget into deterministic runtime environment variables (`CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, `RAYON_NUM_THREADS`, `GOMAXPROCS`, `UV_THREADPOOL_SIZE`, `ROLLDOWN_WORKER_THREADS`, `DEVKIT_TEST_WORKERS`, and `DEVKIT_WORKER_MAX_OLD_SPACE_SIZE`, plus fallback `TOKIO_WORKER_THREADS` when CPU cgroup limits are unavailable and fallback `NODE_OPTIONS=--max-old-space-size=<MiB>` when memory cgroup limits are unavailable). In `@docouno/env`, `RunnableTestsReporter.onInit` also bounds Vitest worker counts and fork isolate heaps automatically — including on upgraded projects that retain an unmodified `vitest.config.mjs`.

## Shared and Adaptive Checkout Locking

`.devkit/state/task.lock` coordinates concurrent CLI and Make invocations without blocking parallel read-only inspection:

- **Lock-free**: `devkit doctor`, `devkit info`, `make info` (and its quiet `make status` alias, which performs only a non-blocking `LOCK_NB` probe), `make help`, `devkit catalog`, `devkit --version`, and `devkit --help` never hold `task.lock`.
- **Shared lock (`LOCK_SH`)**: `devkit describe`, `devkit status`, `make doctor`, `make logs` (releasing the lock before `follow=1` / `--follow` begins streaming), and read-only `make exec` / `devkit exec` run concurrently across terminals and automation agents.
- **Exclusive lock (`LOCK_EX`)**: Mutating and build tasks write `.devkit/state/task.owner.json` (PID, process start time and operation name) so concurrent invocations wait adaptively up to `DEVKIT_LOCK_TIMEOUT_MS` (default `120000` ms) while detecting recursive self-deadlocks immediately.

## Host-local development stand authority

Tracked Make/Compose projections describe components and port intent. A checkout-owned, ignored stand override supplies host leases, ownership labels, preview metadata and inspected image pins. Checkout locking precedes the shared cache registry lock. Devkit validates a port candidate outside the registry lock and checks it again before publication. Docker, image builds and network operations never execute under the registry lock. Durable local intent precedes global claims and container creation. Physical checkout identity plus an incarnation nonce prevents a recreated directory from inheriting old ownership. Controlled transactions transfer the verified epoch; unexplained state cannot become fresh by deleting a marker.

Stand observations are scoped to the Compose project and verify exact ownership before removal or socket reuse. Ordinary down retains leases. Cleanup releases only after owned containers and inspection helpers are proven absent, without deleting service volumes. Unknown/unfinished creators keep recovery state. An independent receipt records possible actors before creation, so a missing ledger cannot be treated as empty history. Desired deployments and container-labelled applied deployments retain immutable host files until their last reference disappears.

Stand history survives changes to the current component model. Status and cleanup still observe the recorded project when the accepted tree has only libraries and no project Compose file. Only a verified empty actor history permits cleanup without Docker.

Preview image acquisition runs through Compose with zero replicas and no service mounts, networks or volumes. A pull or build failure can be retried before any inspector creation is possible. The next step records the exact helper set before creating stopped containers with build and pull disabled. Client completion and daemon completion are tracked separately. An unknown create outcome retains its intent and actor receipt. Verified helper IDs and their anonymous volumes are removed before that history can become empty.

The main Compose creator follows the same outcome boundary. Before `compose up --no-start`, the ledger records the expected service/replica set and existing container IDs. This command builds dependencies and creates containers. Its success completes the creator before `compose start --wait` checks readiness. After a failed or interrupted creation, recovery requires the process family to be reaped and every planned replica to have a new observed owned ID. Pre-existing containers and a partially observed set cannot prove completion. Bounded reconciliation retains unknown outcomes, leases, metadata and image pins; unchanged successful launches reuse existing containers.

Preview host authority comes from registered frontend profiles and final normalized TCP bindings. Mount inputs consist of the frontend and its recursive `volumes_from` providers. Build-context dependencies participate in acquisition. The launch overlay disables build and pull for inspected images and pins external provider IDs. Devkit validates the normalized launch model before committing leases and passes those same bytes to Compose over stdin for creation and startup. Each container label identifies its service's deployment. Status validates the observed image, reserved environment, read-only bind and file digest against that deployment. Rendering an API preview URL remains independent of its container configuration.

Metadata proof shares a 30-second deadline across external-provider reads, helper and image inspection, namespace reads and retained-container observations. Each daemon metadata command has at most three seconds and bounded output. Image pull/build acquisition time is excluded; cancellation and exact helper recovery still apply.

The namespace model distinguishes a receiver's own mounts from Docker mountpoints inherited through `volumes_from`. File-backed Compose configs and secrets participate as read-only binds with Compose's default or explicit destinations. Provider tmpfs and devices are not inherited and cannot mask a source-backed ancestor for the receiver. A receiver's own deeper writable tmpfs can still provide a safe literal metadata slot.

`development.api` relates Compose services. Modern projections add required startup ordering. The development Make DAG includes both frontend and connected API library builds while preserving native library dependency ordering. API bind defaults come from a managed Compose `env_file` before project-owned env files and explicit environment.
