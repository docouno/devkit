# Devkit

A shared development environment for independent TypeScript and Rust repositories. Create applications and npm libraries from modular templates, run native tools in pinned containers, and adopt environment updates through a Git diff.

Requires Linux x86_64, a local Docker Engine with Docker Compose 5.0.0 or newer, and GNU Make. Node.js is needed only for the optional npm entry point.

## Create a project

Start the interactive CLI:

```sh
npx @docouno/devkit@0.5.0
```

Or provide the choices explicitly:

```sh
npx @docouno/devkit@0.5.0 init my-product --app web=react-web
cd my-product
make setup
make check
make dev app=web
```

Without host Node.js:

```sh
curl -fsSL https://github.com/docouno/devkit/releases/download/v0.5.0/install.sh | sh -s -- init my-product --app web=react-web
```

Both entry points use the same native CLI. Projects record an exact release and its integrity; ordinary development commands use the verified local cache.

## Applications, libraries and presets

Application templates include `node-api`, `rust-api`, `react-web`, and `electron-react`. Package templates include `ts-library`, `react-ui`, and `react-client`. Repositories can host `0..N` applications and `1..M` packages — including standalone library workspaces without applications:

```sh
npx @docouno/devkit@0.5.0 init my-sdk --package sdk=ts-library
npx @docouno/devkit@0.5.0 init my-design-system --preset design-system
npx @docouno/devkit@0.5.0 init fullstack-product --preset fullstack-desktop
```

```text
fullstack-product/
├── Makefile
├── compose.yaml
├── apps/
│   ├── api/       # Rust API
│   ├── web/       # Web application
│   └── desktop/   # Electron application
└── packages/
    └── client/    # UI and logic shared by web and desktop
```

Applications are runnable product entry points. Packages hold shared or publishable code (`ts-library` and `react-ui` start with `"private": true` and opt into publication in `package.json`). The `design-system` preset pairs a `react-ui` package with `apps/playground` using source-level Vite HMR. Node components share `pnpm-workspace.yaml` and `pnpm-lock.yaml` with isolated package links; Rust components own their Cargo manifests and locks.

After initialization, use the checkout's Makefile for daily work and maintenance. Add components as the repository grows:

```sh
make add-app name=api template=rust-api
make add-package name=client template=react-client
make add-app name=desktop template=electron-react use=client
make setup
```

Use `make register app=NAME profile=ID` or `make register package=NAME profile=ID` to adopt existing source. Run `make sync` after editing `package.json` metadata (`name`, `"private": false`, `exports`, `publishConfig`) or `.npmrc`:

```sh
make sync
```

## Development

| Command            | Purpose                                                                   |
| ------------------ | ------------------------------------------------------------------------- |
| `make help`        | List the project commands and registered apps and packages, offline       |
| `make setup`       | Prepare accepted images and dependencies                                  |
| `make info`        | Show the branch, checkout readiness, service state and leased addresses   |
| `make sync`        | Synchronize projections and `pnpm-lock.yaml` after manifest edits         |
| `make check`       | Check source, types, formatting, lint and applicable native gates         |
| `make test`        | Run the registered components' native test suites                         |
| `make build`       | Build the registered components                                           |
| `make package`     | Package Electron apps or publishable libraries (`ts-library`, `react-ui`) |
| `make dev app=web` | Start one application's development service                               |
| `make down`        | Stop services while preserving data volumes                               |

Use `app=NAME` or `package=NAME` for setup, check, fix, test, build and package. `make down` stops the project stack. The root Makefile belongs to the product, so custom targets and dependencies remain part of the build.

Host ports are leased per checkout and survive `make down`. Concurrent Git worktrees receive separate ports; `make down cleanup=1` also releases that checkout's leases while preserving data volumes. The default automatic range is `10000-19999`, configurable with `DEVKIT_PORT_RANGE=A-B`.

For a reverse proxy that routes published host ports, set a vendor-neutral template in the shell or once in the main checkout's root `.env` (linked Git worktrees inherit it automatically):

```sh
export DEVKIT_PREVIEW_URL_TEMPLATE='https://{port}-{service}-{branch}.preview.example.com'
make up app=web
make info
```

Placeholders are `{port}`, `{service}`, `{branch}` and `{slug}` (the last two use the same DNS-safe branch slug). A linked worktree's own `.env` can override the template or set `DEVKIT_PREVIEW_URL_TEMPLATE=` to disable inheritance. Configure routing and TLS in your proxy. Devkit prints the URLs and passes exact frontend hosts to its Vite plugin. See [development stands](docs/workflows.md#development-stands-and-preview-urls) for custom images and status semantics.

`init --preset fullstack-web` creates a Rust API and React web app. To connect another frontend to a registered Node or Rust API, use `make add-app name=dashboard template=react-web api=api`. Aggregate `make up` skips Electron apps without `DISPLAY`. Use `headless=1` to include them through Xvfb.

Checks, tests, builds and packaging keep source, configuration and lockfiles read-only. `make fix` explicitly permits formatting changes. To edit dependencies with the accepted toolchain:

```sh
make exec app=web write=1 network=bridge command='pnpm add zod --lockfile-only --ignore-scripts'
make setup
make check
```

Use `make exec command='pnpm …'` for commands in the root Node workspace (pass `-w` with `pnpm add` or `pnpm remove` at the workspace root). Run `make info json=1` for a machine-readable checkout summary. `make status` remains an alias for `make info`.

`make package app=desktop` builds a Linux Electron directory and archive; `make package package=ui` builds a verified `.tgz` tarball and receipt (`publint` + `@arethetypeswrong/core`) for publishable libraries.

## Update an existing project

From a `0.5.0` checkout, choose the target release, inspect the proposed changes and migration notes, then apply them on a branch. This example repeats the accepted version:

```sh
git switch -c chore/devkit-0.5.0
make update to=0.5.0 preview=1
make update to=0.5.0
make setup
make check
```

Pass `reconcile=1` when transitive lockfile dependencies require re-resolution across toolchain updates. Review and merge the resulting diff. Application source, product dependencies and project-owned Make/Compose files retain their ownership. Conflicting edits to managed files stop the update.

After an interrupted update, `make recover` restores the known transaction state. See [Upgrading](docs/upgrades.md) for recovery while the older loader is still installed.

To upgrade from `0.4.1`, use `sh .devkit/runtime/devkit.sh cli update . --to 0.5.0`. For `0.4.0`, use the versioned `0.5.0` installer or `npx @docouno/devkit@0.5.0 update . --to 0.5.0`. Repositories on `0.2.x` or `0.3.x` must bridge through `0.4.1` first.

## Documentation

Every initialized repository includes a concise offline reference at `.devkit/runtime/GUIDE.md`. Detailed guides live in [`docs/`](docs/architecture.md):

- [Architecture](docs/architecture.md) — `0..N` apps / `1..M` packages graph, tooling profiles, native manifest authority, and container security isolation.
- [Workflows](docs/workflows.md) — creating apps and libraries, `--preset design-system` live HMR, `--use`, `npm:` aliases, and `devkit sync`.
- [Publishing Libraries](docs/publishing.md) — opting into `"private": false`, `make package` verification, npm OIDC provenance, and GitLab Package Registry.
- [Continuous Integration](docs/ci.md) — GitHub Actions, GitLab CI, custom enterprise CAs (`DEVKIT_CA_CERT`), and airgapped runners.
- [Upgrading](docs/upgrades.md) — `devkit update`, `--reconcile-lock`, cumulative migration notes, cross-release dispatch, the `0.4.1` bridge, and `devkit recover`.

## CLI and automation

Use `--help` for command options. Complete arguments run without prompts; `--non-interactive` makes missing choices an error. Lifecycle and metadata commands support `--json`. Runtime tasks stream native output and preserve their exit codes.

## Shared package cache

`make setup` lazily obtains the SHA-256-pinned static pnpm executable and installs from `pnpm-lock.yaml`. `DEVKIT_CACHE_DIR` (default `~/.cache/docouno/devkit`) holds the pnpm store/cache, Cargo downloads, and Electron distributions across repositories and Git worktrees. Setup verifies complete Electron distributions before sharing them. Daily gates run offline and mount `node_modules/.pnpm` read-only. Rust-only projects never download pnpm.

Hardlinks require a shared filesystem and mount. If the configured cache cannot share one with the checkout, Devkit uses the repository's common Git directory (`devkit-store`) or `.devkit/cache/pnpm-store` for a non-Git checkout. See the [0.5 migration guide](docs/migrations/0.5.0.md#migrate-node-workspaces-to-pnpm) for existing projects and CI cache placement.

## Develop Devkit

Run `make help` for author commands. Releases are built, tested and published through [GitHub Actions](https://github.com/docouno/devkit/actions/workflows/release.yml).

`DEVKIT_AUTHOR_PNPM_STORE` and `DEVKIT_AUTHOR_PNPM_CACHE` configure the author SDK caches for setup and subsequent Node gates, including offline integration tests. They default to `.artifacts/author/pnpm-store` and `.artifacts/author/pnpm-cache` in this checkout; external directories are mounted into the SDK containers.

Template v9 locks record the complete native pnpm selection graph, including peer contexts. Release preparation validates the complete seed with frozen pnpm and changes only the owned environment version and archive identity; the producer and independent assembler reject other selection changes even when the package set and integrity values match. The source environment's dependencies, peers, and metadata must agree with that graph. Regenerate and commit template locks explicitly when changing their dependency graph.

Before releasing a version, add its changes to [CHANGELOG.md](https://github.com/docouno/devkit/blob/main/CHANGELOG.md). The release workflow uses that version's entry as its description; build and verification metadata stays in a collapsed section.

## License

[MIT](LICENSE). Distributed binaries include third-party notices.
