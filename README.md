# Devkit

Devkit creates standalone TypeScript and Rust repositories with pinned container tools, native package managers and common Make commands. This monorepo builds `@docouno/env`, the runtime, app templates, project presets and a versioned CLI.

The supported platform is **Linux with a local Docker daemon and Docker Compose**. The host needs Bash, GNU Make, coreutils, `flock`, curl and GNU tar. Node and Rust run in containers. Setup downloads dependencies; ordinary checks and builds use the accepted tools offline. Desktop development additionally needs an X11 display and authorization, or explicit headless mode.

Version **0.2.0 is a local publication candidate**. Nothing has been published. The examples below use a release base supplied by the operator; it serves `vX.Y.Z/bootstrap.sh`, `devkit-X.Y.Z.tar.gz` and its `.sha256` sidecar. Use `--package-source local` until the matching npm package is published.

## Create without cloning Devkit

Download the bootstrap from your trusted release location and pass an exact version:

```sh
bootstrap=$(mktemp)
curl --fail --silent --show-error --location --proto =https --proto-redir =https \
  "$RELEASE_BASE/v0.2.0/bootstrap.sh" \
  -o "$bootstrap"
bash "$bootstrap" init /absolute/path/to/product \
  --version 0.2.0 --release-base-url "$RELEASE_BASE" \
  --app api=rust-api --app web=react-web --package-source local
cd /absolute/path/to/product
make setup
make check build
make dev
make status
make down
```

The script also supports `curl … | bash -s -- init …`. It downloads and checks the selected archive, runs its self-contained CLI in the pinned Node container and removes temporary acquisition files. No author checkout or host language SDK is needed. HTTPS is required except for explicit loopback HTTP test servers. Checksums detect corruption; the bootstrap and release location must be trusted.

The target must be new or empty, with an existing parent. Existing `.git` metadata is retained. Project/app names are lower-case slugs; pass `--name product` when the directory name is unsuitable.

| Template         | Component                                                               |
| ---------------- | ----------------------------------------------------------------------- |
| `node-api`       | Node HTTP API, TypeScript compiler and native Node tests                |
| `rust-api`       | Axum HTTP API, Cargo, rustfmt and Clippy                                |
| `react-web`      | React browser app, Vite and Vitest                                      |
| `electron-react` | Electron main/preload/React renderer, electron-vite and Linux packaging |
| `react-client`   | Shared React library, explicit exports, compiler build and Vitest       |

Repeat `--app NAME=TEMPLATE` to choose independent apps. Templates create one component; a preset also connects components. `--preset fullstack-desktop` creates a Rust API, web and Electron clients, and a shared `packages/client` UI with API adapters. Arbitrary `--app` selections do not invent service connections.

## Add components during development

Use the same bootstrap; add/register selects the version already accepted by the project:

```sh
bash "$bootstrap" add-package "$PWD" \
  --release-base-url "$RELEASE_BASE" --name client --template react-client
bash "$bootstrap" add-app "$PWD" \
  --release-base-url "$RELEASE_BASE" --name desktop \
  --template electron-react --use client
```

`--use client` connects a **new** web or Electron app to an existing shared React library. Adding a package does not refactor existing application source. Existing component paths are never overwritten. To adopt code already in the project, use `register "$PWD" --app worker --profile node` (or `--package client --profile react-library`) after preparing the required native tooling. Profiles are `node`, `web`, `electron`, `react-library` and `rust`.

An unpacked release offers the same commands directly through `./devkit`. Legacy init aliases `--template node|react|rust|mixed` remain supported.

## Generated repository

```text
Makefile                     # product-owned entry point and extra build edges
compose.yaml                 # product-owned development overrides
package.json                 # present for Node components; explicit npm workspaces
package-lock.json            # one native Node workspace lock
apps/
  api/                       # Cargo.toml, Cargo.lock, native configuration and source
  web/                       # package.json, Vite configuration and source
  desktop/                   # package.json, main/preload/renderer and packaging
packages/
  client/                    # shared library package.json, exports and source
.devkit/
  manifest.json              # accepted release, components and ownership
  project.mk                 # generated Make registration and dependency edges
  compose.apps.yaml          # generated app services
  compose.workspace.json     # generated writable dependency/output mounts
  tooling-profiles.json      # accepted tool dependency sets
  runtime/                   # versioned Make/Compose/native helpers
  runtime.sha256
  npm/                       # committed env archive in local package mode
  cache/                     # ignored dependency caches
  state/                     # ignored locks, setup receipts and image identities
```

Only selected components are created. The root npm workspace contains Node components only; Rust uses its own Cargo manifest and lock. The shared library's native dependency edges determine prerequisite checks/builds. Generated files under `.devkit` are validated against metadata. Root Make, Compose and application code remain project-owned.

## Daily commands

| Command                           | Behavior                                                                      |
| --------------------------------- | ----------------------------------------------------------------------------- |
| `make setup [app=web]`            | Install the Node workspace, prepare pinned images and fetch Rust dependencies |
| `make doctor`                     | Verify accepted metadata, installed dependencies and prepared tool images     |
| `make check [app=web]`            | Source coverage/configuration, format, lint, types and tests                  |
| `make test [package=client]`      | Native finite test run for the selected component                             |
| `make build [app=web]`            | Native build, including required shared libraries                             |
| `make fix [app=web]`              | Explicit formatter/linter edits                                               |
| `make dev` / `make up`            | Start app services detached; `app=web` selects one                            |
| `make dev app=desktop headless=1` | Run Electron under a container Xvfb display                                   |
| `make package [app=desktop]`      | Build a Linux Electron directory and `.tar.gz` archive                        |
| `make status`                     | Show actual localhost port mappings and service state                         |
| `make logs [app=api] [tail=100]`  | Finite logs; `follow=1` enables streaming                                     |
| `make down`                       | Stop this checkout's services and preserve data                               |

`make -C /path/to/product check` works, including paths with spaces. Per-component targets include `check-web`, `build-api` and `build/packages/client`. Unknown selections and stale registration fail visibly. Checks, tests and builds mount source/configuration/locks read-only; dependency trees, caches and outputs have separate writable mounts. Fix permits source changes. Node setup is shared across the workspace even when selecting one app. Root recipes remain serial even with an inherited `-j`; native tools may parallelize internally.

Electron uses a sandboxed, isolated renderer and a narrow preload bridge. GUI development forwards an authorized X11 socket without `xhost`; headless mode is explicit. Packaging is currently a Linux directory/archive, without installer signing, auto-update or macOS/Windows support. See [architecture](docs/architecture.md) for runtime limits.

## Update through a reviewable diff

```sh
git switch -c chore/devkit-0.2.1
bash "$bootstrap" update "$PWD" \
  --version 0.2.1 --release-base-url "$RELEASE_BASE"
git diff
make setup
make check build
```

The new release updates owned tooling, runtime and native lock integration, preserving product source, product dependencies, root Make/Compose additions and environment files. Local edits to managed files or tool-owned dependency values stop it before applying. The package source mode is retained unless explicitly changed. No automatic update runs during normal development; no Git commit, branch, PR or merge is created by the CLI.

An interrupted apply blocks normal commands. `recover "$PWD" --version X.Y.Z` through the bootstrap, or `devkit recover "$PWD"` from a release, restores known pre-transaction bytes and rejects conflicting new edits. A migration from 0.1.0 adopts schema 2 and native root npm workspaces while retaining product-owned development services. See [releases and recovery](docs/releases.md).

## Develop Devkit

```sh
make setup
make check
make pack-next                # local 0.2.0 and 0.2.1 previews, no publication
make verify-runtime
make verify-rust
make verify-bootstrap
make verify-packaged
make verify-registry
```

`make verify` runs the complete set. The author black-box harnesses also require Python 3. They create isolated consumers and retain evidence under ignored `.artifacts/`; no host SDK, global npm configuration or public registry is installed. `make init template=mixed target=/path/to/product` is a source-author convenience, not the consumer distribution path.

See [architecture](docs/architecture.md), [research](docs/research.md), [releases](docs/releases.md) and [verification](docs/verification.md). Private team conventions and agent instructions live in the team's knowledge system. Devkit ships executable tooling and ordinary usage documentation; it contains no agent files or documentation framework.

## License

MIT. Bundled third-party runtime notices are included under `runtime/licenses`.
