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

## 2. Add Components and Wire Internal Packages

Add libraries or applications and wire them to existing `packages/<slug>` components with `use=`:

```sh
make add-package name=tokens template=ts-library
make add-package name=ui template=react-ui use=tokens
make add-app name=docs template=react-web use='tokens ui'
make setup
```

`use=` adds `"<pkgName>": "workspace:*"` to the consumer's `package.json`, updates `pnpm-lock.yaml`, and derives the build/check DAG in `.devkit/manifest.json` and `.devkit/project.mk`.

## 3. Add or Update Third-Party Dependencies

Run package-manager mutations inside the pinned container. Use `write=1` to allow edits and `network=bridge` for registry access.

```sh
# A Node package
make exec package=ui write=1 network=bridge command='pnpm add clsx --lockfile-only --ignore-scripts'

# Root Node workspace (pass -w when omitting app= / package=)
make exec write=1 network=bridge command='pnpm add -D -w vitest --lockfile-only --ignore-scripts'

# A Rust app
make exec app=api write=1 network=bridge command='cargo update --manifest-path Cargo.toml'
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
make info                   # Inspect checkout readiness, leases and observed services
make info json=1            # Print the checkout summary as JSON
make logs app=playground    # Print recent service logs (follow=1 streams without holding task.lock)
make down                   # Stop and remove project containers and networks
```

For environment diagnostics and zero-Docker metadata inspection (via `npx` or directly through `.devkit/runtime/devkit.sh` without host Node.js):

```sh
sh .devkit/runtime/devkit.sh cli doctor . --json    # Lock-free Docker, cgroup v2, DinD, disk, and checkout checks
sh .devkit/runtime/devkit.sh cli describe . --json  # Offline component graph, selectors, ports, and receipts
```

Read-only commands such as `make logs` and read-only `make exec` share the checkout lock. `make info` probes the checkout lock without waiting and reports whether the stand can be observed. On first use it can download the verified pinned executable. Mutating commands serialize automatically via `.devkit/state/task.owner.json` (tunable with `DEVKIT_LOCK_TIMEOUT_MS`).

In zero-app library repositories, `make info` lists the registered packages. `make dev` and `make up` require an application.

## 6. Live Design-System HMR (`devkitWorkspaceSource`)

Web (`react-web`) and Electron (`electron-react`) applications using `devkitWorkspaceSource()` from `@docouno/env/vite` resolve internal `packages/*/src` TypeScript, TSX, and CSS entrypoints directly during `make dev` — both for `./dist/...` library export maps (`ts-library`, `react-ui`) and direct `"development": "./src/..."` exports (`react-client`):

- Editing `packages/ui/src/Button.tsx`, `packages/ui/src/styles.css`, or `packages/client/src/index.ts` triggers instant Vite Fast Refresh without running `tsdown` or modifying `dist/`.
- During tests and builds, Vitest and Vite resolve imports through the package's public `exports` and their active conditions. Vitest can select the `react-client` development export in `src/`. The default production build selects its compiled `dist/` export.

## Development stands and preview URLs

### Use Worktrunk

New projects include the project-owned `.config/wt.toml` unless initialized with `--no-worktrunk`. Install Worktrunk independently and approve its project hooks from a checkout that has the config. Worktrunk applies the config in the invoking checkout and manages worktree paths, branches and removal:

```toml
# Worktrunk runs approved project hooks in the target checkout.
# Pass --yes in non-interactive sessions or run wt config approvals add --yes.
# Enable the optional start with DEVKIT_WT_AUTOSTART=1.
[pre-start]
devkit-reserve = "sh .devkit/runtime/devkit.sh worktree-hook reserve"

[post-start]
devkit-start = "sh .devkit/runtime/devkit.sh worktree-hook start"

[pre-remove]
devkit-remove = "sh .devkit/runtime/devkit.sh worktree-hook remove"
```

```sh
# Interactive shell
wt switch --create feature/dashboard
make info
wt list
wt remove feature/dashboard

# Non-interactive scripts and PTY-backed agents (never blocks on hook prompts)
wt switch --create --no-cd --yes feature/dashboard
wt remove --foreground --yes feature/dashboard
# Or pre-approve hooks once for this repository:
wt config approvals add --yes
```

Set `WORKTRUNK_WORKTREE_PATH='{{ repo_path }}/.worktrees/{{ branch | sanitize }}'` or configure `worktree-path` in the Worktrunk user config to use nested worktrees. Keep `/.worktrees/` in your project's `.gitignore`. Use `make info` in the selected checkout for the confirmed address.

The `pre-start` hook reserves ports without starting containers. It can download a verified pinned engine on first use. A reservation failure reports a skip and leaves the checkout available for `make up`. Set `DEVKIT_WT_AUTOSTART=1` to run `make setup` and `make up` in the optional `post-start` hook. With the `pre-remove` hook approved, Worktrunk stops removal when cleanup fails. Declining approval or using `--no-hooks` can skip cleanup. In that case, first run `sh .devkit/runtime/devkit.sh cli stand remove .` in the target checkout. Declare long-lived data in Compose `external` volumes.

Other worktree managers can call `sh .devkit/runtime/devkit.sh cli stand reserve .` and `sh .devkit/runtime/devkit.sh cli stand remove .` from the target checkout. Keep the checkout on a cleanup error and retry after resolving the reported issue.

`make up` and `make dev` allocate sticky host ports for every active Compose port binding. Automatic replica bindings receive a contiguous range. Explicit ports and ranges stay explicit. TCP and UDP leases are separate, wildcard host bindings overlap conservatively, and a real bind conflict can move only the failed automatic group. The first primary-web candidate uses the branch hash convention of Worktrunk's `hash_port`; the persisted lease remains authoritative after a branch rename or restart.

Leases live in the shared user cache (`port-leases.json`), with checkout identity and desired/applied references under ignored `.devkit/state/`. Every checkout using the same host should share `DEVKIT_CACHE_DIR`. `DEVKIT_PORT_RANGE` defaults to `10000-19999`; changing the range preserves an existing usable lease. A deterministic managed Compose projection uses ephemeral declarations; the untracked stand override contains actual ports. Starting and stopping services leaves tracked files clean.

- `make down` stops the current Compose stack and preserves leases, metadata and data volumes.
- `make down cleanup=1` verifies stand ownership, removes owned remnants, and releases leases and unused metadata. It preserves service volumes.
- A never-started fresh checkout can clean up before setup without Docker or an engine download. An existing state directory requires verified launch/epoch provenance; absence of a marker alone is insufficient.
- A daemon create with an unknown outcome leaves a recovery intent until Devkit can verify what it created. Fixing a preview image pull or build error allows another `up` without clearing leases or state.

Set `DEVKIT_PREVIEW_URL_TEMPLATE` to an absolute HTTP(S) URL in the process environment or the current checkout's root `.env`. Devkit checks those sources in order. An empty value at either level disables preview URLs.

To share a template across a clone, put it in the root `.env` of a Devkit worktree on a named branch and run `git config --local devkit.previewSourceBranch next` (replace `next` with that branch). Devkit reads only the preview setting from the selected worktree when both local sources omit it. The URL uses the consuming worktree's branch. Keep the selected branch checked out in exactly one registered worktree. If it is missing or ambiguous, correct the setting or run `git config --local --unset devkit.previewSourceBranch` to return to local configuration.

`{port}` is a published host port, `{service}` is the Compose service, and `{branch}`/`{slug}` is a lower-case DNS-safe slug of the branch (checkout basename outside Git; detached commits use a short revision). Userinfo, wildcard hosts and unknown placeholders are rejected. The template does not provision a proxy, DNS or TLS.

Preview links are rendered for actual published TCP endpoints of all services, including APIs. Every published TCP port of registered web/Electron frontends contributes an exact Vite host, including non-default targets, explicit ranges and replicas. APIs and UDP bindings do not become Vite hosts. Local addresses remain available alongside previews, with IPv6 loopback preserved. Both `make info` and `devkit status --json` report applied endpoints, metadata verification and whether changed preview settings need a restart. JSON `standPorts` and retained-lease lines remain available after ordinary down (rendering the current checkout template for stopped services while preserving the applied snapshot on running services so an unapplied template is never advertised as already running).

Devkit mounts one immutable, digest-checked JSON host set read-only at `/run/devkit/preview-hosts.json`, falling back to `/devkit/preview-hosts.json`. The file is limited to 32 MiB and 65,535 sorted unique hosts. Managed launches clear the legacy `DEVKIT_PREVIEW_HOSTS` CSV carrier. Short `DEVKIT_PREVIEW_HOSTS_FILE`, `DEVKIT_PREVIEW_HOSTS_SHA256`, `DEVKIT_PREVIEW_URL`, `DEVKIT_SERVICE` and `DEVKIT_BRANCH` variables describe it; automatic host sets are never expanded into a large environment variable.

Host metadata uses browser URL canonicalization. For example, `127.1` becomes `127.0.0.1` and `::ffff:127.0.0.1` becomes `::ffff:7f00:1`. Invalid numeric/IDNA hosts and aliases of wildcard addresses fail before launch. The rendered URL retains its path and query; its resolved hostname must belong to the canonical file set.

Compose acquires frontend images and images of their recursive `volumes_from` providers, including build contexts. Devkit then inspects those images in stopped containers before mounting the preview file. Ordinary startup dependencies use the normal Compose launch path. Image symlinks, inherited mounts and retained volumes must leave a provable literal slot. A nested destination inside an opaque volume can alias a path outside that volume. Keep preview destinations provable by avoiding those nested mounts or using a bind with verifiable ancestry. The launch pins inspected image IDs and external volume-provider IDs and retains declared platforms. Inspectors have no service mounts and their own anonymous volumes are cleaned separately.

Docker does not inherit a provider's tmpfs or devices through `volumes_from`. A provider's deeper tmpfs therefore cannot protect the receiver from an inherited source bind. A receiver's own writable tmpfs remains supported.

## Connecting a frontend to an API

```sh
make add-app name=dashboard template=react-web api=api
make setup
make up app=dashboard
```

`--api` accepts an existing registered Node/Rust API slug and works with `react-web` or `electron-react`. Validation and generation form one transaction. The connection is `development.api`, separate from package imports and library dependencies. It supplies `DEVKIT_API_TARGET`, a required Compose `service_started` edge, and frontend development prerequisites for libraries reached by the API. Backend readiness beyond process startup remains the product's responsibility.

New Node/Rust API starters accept `HOST` and decimal `PORT` (`0..65535`, including ephemeral port `0`); defaults are `0.0.0.0:8080`. They log the actual bound address and retain graceful signal shutdown. Invalid values fail visibly.

For multi-app aggregate `make up`/`make dev`, an empty `DISPLAY` with `headless=0` skips Electron before desktop prerequisites and prints a warning. Explicit `app=desktop`, direct `make dev-desktop` and single-app Electron projects still require a display or `headless=1`. Headless mode uses Xvfb; normal X11 validation remains active when a display is supplied.

Keep ignored stand state while services or inspection creators may exist. A missing launch ledger cannot prove that cleanup is safe: an independent actor receipt preserves that uncertainty. Restore recorded state before automated cleanup when history was removed; ordinary `make down` remains available to stop the Compose project.

Changing to a tree with only libraries does not erase an earlier stand. Status still shows its containers, and `make down cleanup=1` verifies and removes owned remnants before releasing leases.

A failed or interrupted inspector creation may complete a Docker create after the client exits. Cleanup observes late helpers and retains incomplete creator outcomes with a bounded diagnostic. Rerun cleanup after pending daemon work completes and keep the recorded state while the outcome remains unknown.

The main Compose launch records successful container creation before starting services and waiting for health checks. A readiness failure still allows cleanup of the known containers. When creation fails or is interrupted, cleanup waits for a proven outcome for every planned replica, preserving leases and metadata while accepted creates may still arrive. Existing containers do not prove that a failed replacement completed. If some planned outcomes remain unknown, inspect the original daemon failure and preserve the recorded state.
