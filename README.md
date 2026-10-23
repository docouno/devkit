# Devkit

Devkit creates standalone TypeScript, Rust and mixed application repositories with pinned container tooling and common Make commands. This repository contains the shared environment package, runtime, templates and release tooling.

The first development platform is **Linux + Docker Compose**. A developer needs Bash, GNU Make, coreutils, `flock` and access to a local Docker daemon with Compose. Node and Rust run in containers. Setup needs public npm/Cargo/image network access; ordinary gates do not update accepted tools. Docker must support the bind mounts and caller UID/GID used here. macOS, Windows and a remote Docker daemon are not validated platforms.

Version 0.1.0 is prepared locally; no public package or release has been published. Use the explicit local package mode below until publication.

## Create a project from source

```sh
make setup
make init template=mixed target=/absolute/path/to/my-product
cd /absolute/path/to/my-product
make setup
make check
make build
make dev
make status
make down
```

The target's parent must exist. The target must be new or empty; existing `.git` metadata is retained. Templates are `node` (Node HTTP API), `react` (React/Vite), `rust` (Axum API), and `mixed` (React + Rust API). Project names are lower-case slugs; the standalone CLI accepts `--name` when the directory name is unsuitable.

`make init` builds a release and copies a real `@docouno/env` archive into the new repository's `.devkit/npm`. Commit it alongside the accepted runtime and native lockfiles. The project does not need this author checkout or a temporary registry afterward.

## Use a standalone release

```sh
sha256sum -c devkit-0.1.0.tar.gz.sha256
tar -xzf devkit-0.1.0.tar.gz
./devkit-0.1.0/devkit init /absolute/path/to/product --template node --package-source local
```

An unpacked release contains its own launcher, compiled helper, runtime, templates and npm archive. Without `--package-source local`, the CLI selects registry mode: exact published `@docouno/env` versions and canonical npm lock URLs. This default becomes usable when the corresponding package is published. No implicit latest-version fetch occurs.

## A generated repository

```text
Makefile                     # product-owned app list and build edges
compose.yaml                 # product-owned development services
apps/
  web/                       # package.json, lockfile, TS/Vite config, source
  api/                       # Cargo.toml, Cargo.lock, toolchain, source
.devkit/
  manifest.json              # accepted release and ownership identities
  runtime/                   # versioned shared scripts/configuration
  runtime.sha256
  npm/                       # committed archives in local package mode
  cache/                     # ignored native dependency caches
  state/                     # ignored setup receipts and admission lock
```

Single-language projects have only their required app. Native manifests belong to the app. There is no required root npm or Cargo workspace. Libraries and workspace members can use the owning ecosystem's layout inside an app; service interactions use ordinary interfaces such as HTTP. The mixed starter connects Vite's `/api` proxy to the Rust service and enables its API-backed UI.

The root Makefile remains small:

```make
DEVKIT_APPS := web api
include .devkit/runtime/devkit.mk

# Add this only when a product actually has the corresponding build dependency:
# build-api: build-web
```

## Daily commands

| Command                          | Behavior                                                              |
| -------------------------------- | --------------------------------------------------------------------- |
| `make setup [app=web]`           | Prepare pinned images and install locked native dependencies          |
| `make doctor`                    | Check prerequisites, accepted metadata and prepared tool images       |
| `make check [app=web]`           | Required source/configuration, formatting, lint, types and test gates |
| `make test [app=api]`            | Native finite test run                                                |
| `make build [app=web]`           | Native production build with separate writable output                 |
| `make fix [app=web]`             | Explicit formatter/linter fixes; inspect the resulting diff           |
| `make dev` / `make up`           | Start detached product Compose services                               |
| `make status`                    | Show services and actual ephemeral localhost port mappings            |
| `make logs [app=api] [tail=100]` | Finite logs; opt into streaming with `follow=1`                       |
| `make down`                      | Stop this checkout's development services; preserve named data        |

Use `make -C /path/to/project check` from another directory. Per-app targets such as `check-web` and `build-api` support normal Make dependencies. All registered apps run by default; unknown, missing, ambiguous or unregistered apps fail visibly. The common task lock prevents setup/update from racing a gate in the same checkout.

Check/test/build mount source, configuration and locks read-only. Native dependencies, caches and outputs have separate writable mounts. Fix deliberately permits source writes. The root project's secret `.env` is not loaded automatically; add explicit environment configuration to product Compose services when required.

## Update through a reviewable change

Use the **new** unpacked release against an existing project:

```sh
git switch -c chore/devkit-0.1.1
/path/to/devkit-0.1.1/devkit update "$PWD"
git diff
make setup
make check
make build
```

Commit and submit that diff through your usual PR process. Updating changes the accepted runtime, named tooling dependencies, native locks and managed Rust toolchain pin. It preserves product code, scripts, dependencies, environment values, root Makefile and Compose. Local edits to managed files or tool-owned dependency values stop the update before applying it. An update preserves the package source mode unless explicitly switched with `--package-source`.

After an interrupted apply, ordinary commands reject the pending state. Run `/path/to/release/devkit recover "$PWD"` to roll back known transaction bytes, inspect the diff and retry. Recovery refuses conflicting new edits rather than discarding them. Details and ownership boundaries are in [releases](docs/releases.md).

To add an existing native app, use the matching accepted release's `devkit register "$PWD" --app worker --template node` (or `react`/`rust`). It validates native tools without creating code. Then add the app to `DEVKIT_APPS` and its development service to Compose. Adding an app is an explicit product change.

## Develop Devkit

```sh
make setup
make check                    # implementation types/lint/format + core/environment tests
make pack                     # standalone release and npm archive, no publication
make verify-runtime           # runtime/admission/cancellation black-box tests
make verify-rust              # native Rust helper contract
make verify-packaged          # isolated standalone consumer matrix and local update
make verify-registry          # real npm registry resolution with temporary fixture
```

The black-box author harnesses additionally need Python 3. `make verify` runs the full set. These checks use uniquely owned temporary containers/checkouts and retain evidence under ignored `.artifacts/`; toolchain images and Docker build cache remain available. No host language SDK or global npm configuration is installed.

Source areas: `packages/env` contains the reusable ESM package; `runtime` holds Make/Compose/native helpers; `templates` holds native starters; `tooling` builds releases and performs generation/updates; `test` proves their behavior. See [architecture and decisions](docs/architecture.md), [research and experiments](docs/research.md), [release preparation](docs/releases.md) and [verification](docs/verification.md).

Devkit's responsibility is executable tooling and its configuration. Team conventions and agent instructions belong in the team's knowledge system; the release contains no agent files or documentation framework.

## License

MIT.
