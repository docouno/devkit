# Daily Workflows

This guide covers day-to-day repository creation, component wiring, dependency management, local development, and workspace synchronization in Devkit `0.5.0`.

## 1. Create a New Repository

### Runnable Product Applications

Initialize a single-app or multi-app workspace:

```sh
npx @docouno/devkit@0.5.0 init my-product --app web=react-web
```

For a web-only fullstack stand (`apps/api` Rust API + `apps/web` React SPA):

```sh
npx @docouno/devkit@0.5.0 init my-web --preset fullstack-web
```

Or scaffold the fullstack preset (`apps/api` Rust API + `apps/web` React SPA + `apps/desktop` Electron app + `packages/client` shared React library):

```sh
npx @docouno/devkit@0.5.0 init my-suite --preset fullstack-desktop
```

### Standalone Libraries (Zero-App Workspace)

Create a package-only repository without scaffolding a dummy runnable application:

```sh
npx @docouno/devkit@0.5.0 init acme-sdk --package sdk=ts-library
```

Create a React component library repository:

```sh
npx @docouno/devkit@0.5.0 init acme-ui --package ui=react-ui
```

### Design System with Interactive Playground

Scaffold `packages/ui` (`react-ui`) pre-wired to `apps/playground` (`react-web`) with instant source HMR:

```sh
npx @docouno/devkit@0.5.0 init acme-design --preset design-system
```

After initialization, prepare pinned toolchains and workspace dependencies:

```sh
cd acme-design
make setup
```

## 2. Add Components and Wire Internal Packages (`--use`)

Add libraries or applications at any time and wire them to existing `packages/<slug>` components with repeatable `--use`:

```sh
npx @docouno/devkit@0.5.0 add-package . --name tokens --template ts-library
npx @docouno/devkit@0.5.0 add-package . --name ui --template react-ui --use tokens
npx @docouno/devkit@0.5.0 add-app . --name docs --template react-web --use tokens --use ui
make setup
```

`--use <slug>` adds `"<pkgName>": "workspace:*"` to the consumer's `package.json`, updates `pnpm-lock.yaml`, and derives the build/check DAG automatically in `.devkit/manifest.json` and `.devkit/project.mk`.

## 3. Add or Update Third-Party Dependencies (`devkit exec`)

Never run host `npm install` or `cargo` directly on the checkout. Always execute package-manager mutations inside the pinned container with `--write` and `--network bridge`:

```sh
# Node.js / React / TypeScript component (including npm: aliases)
npx @docouno/devkit@0.5.0 exec --component packages/ui --write --network bridge -- \
  pnpm add clsx

npx @docouno/devkit@0.5.0 exec --component packages/ui --write --network bridge -- \
  pnpm add legacy-zod@npm:zod@^3.24.0

# Rust component
npx @docouno/devkit@0.5.0 exec --component apps/api --write --network bridge -- \
  cargo update --manifest-path Cargo.toml
```

Without host Node.js, use the root `Makefile` wrapper:

```sh
make exec package=ui write=1 network=bridge command="pnpm add clsx"
```

Follow dependency mutations with `make setup` to refresh the verified workspace lock receipt.

## 4. Edit Native Manifests and Synchronize (`make sync` / `devkit sync`)

Devkit treats `package.json`, `Cargo.toml`, and root `.npmrc` as product-owned sources of truth. When you edit package metadata directly — such as renaming `"name"` to `@corp/ui`, setting `"private": false`, adjusting workspace dependencies, or adding scoped private registries in `.npmrc` — synchronize the lockfile and Devkit projections:

```sh
# Preview planned lockfile and projection updates without writing
make sync preview=1

# Apply fast offline JSON sync (or online resolution when registry packages changed)
make sync

# Reconcile drifted peer/transitive locks across the 3-stage ladder when needed
make sync reconcile=1

make setup
```

You can also invoke `npx @docouno/devkit@0.5.0 sync . [--preview] [--reconcile-lock]` or `sh .devkit/runtime/devkit.sh cli sync . --json`.

## 5. Run Daily Gates and Development Services

All root `Makefile` gates run inside read-only, network-isolated (`--network none`) containers after `make setup`:

```sh
make setup                  # Pull pinned images and install locked dependencies
make doctor                 # Verify host prerequisites, locks, and container readiness
make check                  # Fast single-pass static verification (lint, format, types, Clippy)
make fix                    # Apply Prettier, ESLint --fix, and rustfmt explicitly
make test                   # Execute unit and integration test suites
make build                  # Build production bundles and verify library packability
make package                # Emit verified .tgz / desktop artifacts into release/
```

Target a single application or library with `app=<slug>` or `package=<slug>`:

```sh
make check package=ui
make test app=playground
make package package=ui
```

For repositories with runnable applications (`apps/*`):

```sh
make dev                    # Start development services and print http://127.0.0.1:<port> URLs
make up                     # Start development stack in the background and print service URLs
make status                 # Inspect running service containers via Docker Compose
npx @docouno/devkit@0.5.0 status . --json    # Structured services, health, URLs, and receipts
make logs app=playground    # Print recent service logs (follow=1 streams without holding task.lock)
make down                   # Stop and remove project containers and networks
```

For environment diagnostics and zero-Docker metadata inspection (via `npx` or directly through `.devkit/runtime/devkit.sh` without host Node.js):

```sh
sh .devkit/runtime/devkit.sh cli doctor . --json    # Lock-free Docker, cgroup v2, DinD, disk, and checkout checks
sh .devkit/runtime/devkit.sh cli describe . --json  # Offline component graph, selectors, ports, and receipts
```

Read-only commands (`devkit describe`, `devkit status`, `make doctor`, `make status`, `make logs`, and read-only `devkit exec`) use shared checkout locks so multiple terminals or automation tools can inspect the repository concurrently. Mutating commands serialize automatically via `.devkit/state/task.owner.json` (tunable with `DEVKIT_LOCK_TIMEOUT_MS`).

In zero-app library repositories, `make status`, `make logs`, and `make down` succeed as clean no-ops, while `make dev` and `make up` exit with a hint explaining how to add a playground app via `devkit add-app`.

## 6. Live Design-System HMR (`devkitWorkspaceSource`)

Web (`react-web`) and Electron (`electron-react`) applications using `devkitWorkspaceSource()` from `@docouno/env/vite` resolve internal `packages/*/src` TypeScript, TSX, and CSS entrypoints directly during `make dev`:

- Editing `packages/ui/src/Button.tsx` or `packages/ui/src/styles.css` triggers instant Vite Fast Refresh in `apps/playground` without running `tsdown` or modifying `packages/ui/dist/`.
- Because `devkitWorkspaceSource()` is strictly scoped to `apply: 'serve'`, `make test` and `make build` always validate the real compiled `dist/` output and public `exports` map of every workspace library.

## Development stands and preview URLs

`make up` and `make dev` allocate sticky host ports for every active Compose port binding. Automatic replica bindings receive a contiguous range. Explicit ports and ranges stay explicit. TCP and UDP leases are separate, wildcard host bindings overlap conservatively, and a real bind conflict can move only the failed automatic group. The first primary-web candidate uses the branch hash convention of Worktrunk's `hash_port`; the persisted lease remains authoritative after a branch rename or restart.

Leases live in the shared user cache (`port-leases.json`), with checkout identity and desired/applied references under ignored `.devkit/state/`. Every checkout using the same host should share `DEVKIT_CACHE_DIR`. `DEVKIT_PORT_RANGE` defaults to `10000-19999`; changing the range preserves an existing usable lease. A deterministic managed Compose projection uses ephemeral declarations; the untracked stand override contains actual ports. Starting and stopping services leaves tracked files clean.

- `make down` stops the current Compose stack and preserves leases, metadata and data volumes.
- `make down cleanup=1` verifies stand ownership, removes owned remnants, and releases leases and unused metadata. It preserves service volumes.
- A never-started fresh checkout can clean up before setup without Docker or an engine download. An existing state directory requires verified launch/epoch provenance; absence of a marker alone is insufficient.
- A failed daemon or unfinished creator leaves a recovery intent. Retry after resolving the diagnostic; Devkit does not adopt unknown legacy containers or report an unproved cleanup as successful.

Set `DEVKIT_PREVIEW_URL_TEMPLATE` to an absolute HTTP(S) URL. `{port}` is a published host port, `{service}` is the Compose service, and `{branch}`/`{slug}` is a lower-case DNS-safe slug of the branch (checkout basename outside Git; detached commits use a short revision). Userinfo, wildcard hosts and unknown placeholders are rejected. The template does not provision a proxy, DNS or TLS.

Preview links are rendered for actual published TCP endpoints of all services, including APIs. Every published TCP port of registered web/Electron frontends contributes an exact Vite host, including non-default targets, explicit ranges and replicas. APIs and UDP bindings do not become Vite hosts. Local addresses remain available alongside previews, with IPv6 loopback preserved. Both `make status` and `devkit status --json` report applied endpoints, metadata verification and whether changed preview settings need a restart. JSON `standPorts` and retained-lease lines remain available after ordinary down; these reservations are not reported as running endpoints. They do not advertise a newly supplied template as already running.

Devkit mounts one immutable, digest-checked JSON host set read-only at `/run/devkit/preview-hosts.json`, falling back to `/devkit/preview-hosts.json`. The file is limited to 32 MiB and 65,535 sorted unique hosts. Managed launches clear the legacy `DEVKIT_PREVIEW_HOSTS` CSV carrier. Short `DEVKIT_PREVIEW_HOSTS_FILE`, `DEVKIT_PREVIEW_HOSTS_SHA256`, `DEVKIT_PREVIEW_URL`, `DEVKIT_SERVICE` and `DEVKIT_BRANCH` variables describe it; automatic host sets are never expanded into a large environment variable.

Custom images/builds are acquired through Compose and inspected in stopped image-only containers before any user-backed service starts. Image symlink ancestry, mount aliases, inherited volumes, configs, secrets, devices and retained namespaces must leave a provable literal slot. Keep one slot outside project/volume namespaces; an opaque layout fails with a bounded diagnostic. Actual launch pins inspected image IDs and external volume-provider IDs, retaining declared platforms. Inspectors never run image commands, receive service mounts or remove service volumes. Their anonymous inspection volumes are cleaned separately.

Docker does not inherit a provider's tmpfs or devices through `volumes_from`. A provider's deeper tmpfs therefore cannot protect the receiver from an inherited source bind. A receiver's own writable tmpfs remains supported.

## Connecting a frontend to an API

```sh
npx @docouno/devkit@0.5.0 add-app . --name dashboard --template react-web --api api
make setup
make up app=dashboard
```

`--api` accepts an existing registered Node/Rust API slug and works with `react-web` or `electron-react`. Validation and generation form one transaction. The connection is `development.api`, separate from package imports and library dependencies. It supplies `DEVKIT_API_TARGET`, a required Compose `service_started` edge, and frontend development prerequisites for libraries reached by the API. Backend readiness beyond process startup remains the product's responsibility.

New Node/Rust API starters accept `HOST` and decimal `PORT` (`0..65535`, including ephemeral port `0`); defaults are `0.0.0.0:8080`. They log the actual bound address and retain graceful signal shutdown. Invalid values fail visibly.

For multi-app aggregate `make up`/`make dev`, an empty `DISPLAY` with `headless=0` skips Electron before desktop prerequisites and prints a warning. Explicit `app=desktop`, direct `make dev-desktop` and single-app Electron projects still require a display or `headless=1`. Headless mode uses Xvfb; normal X11 validation remains active when a display is supplied.

Keep ignored stand state while services or inspection creators may exist. A missing launch ledger cannot prove that cleanup is safe: an independent actor receipt preserves that uncertainty. Restore recorded state before automated cleanup when history was removed; ordinary `make down` remains available to stop the Compose project.

A failed or interrupted acquisition may complete a Docker create after the client exits. Cleanup observes late helpers and retains incomplete creator outcomes with a bounded diagnostic. Rerun cleanup after pending daemon work completes; keep the recorded state while the outcome remains unknown.
