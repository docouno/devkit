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

Add components as the repository grows:

```sh
npx @docouno/devkit@0.5.0 add-app . --name api --template rust-api
npx @docouno/devkit@0.5.0 add-package . --name ui --template react-ui
npx @docouno/devkit@0.5.0 add-app . --name desktop --template electron-react --use ui
make setup
```

Use `register --help` to adopt an existing component without replacing its source, and `make sync` (or `devkit sync`) after editing `package.json` metadata (`name`, `"private": false`, `exports`, `publishConfig`) or `.npmrc`:

```sh
make sync
```

## Development

| Command            | Purpose                                                                   |
| ------------------ | ------------------------------------------------------------------------- |
| `make setup`       | Prepare accepted images and dependencies                                  |
| `make sync`        | Synchronize projections and `pnpm-lock.yaml` after manifest edits         |
| `make check`       | Check source, types, formatting, lint and applicable native gates         |
| `make test`        | Run the registered components' native test suites                         |
| `make build`       | Build the registered components                                           |
| `make package`     | Package Electron apps or publishable libraries (`ts-library`, `react-ui`) |
| `make dev app=web` | Start one application's development service                               |
| `make status`      | Show services and their local ports                                       |
| `make down`        | Stop services while preserving data volumes                               |

Use `app=NAME` or `package=NAME` for setup, check, fix, test, build and package. `make down` stops the project stack. The root Makefile belongs to the product, so custom targets and dependencies remain part of the build.

Checks, tests, builds and packaging keep source, configuration and lockfiles read-only. `make fix` explicitly permits formatting changes. To edit dependencies with the accepted toolchain:

```sh
npx @docouno/devkit@0.5.0 exec --component apps/web --write --network bridge -- pnpm add zod
make setup
make check
```

On hosts without a global `devkit` or Node.js installation, run any CLI subcommand directly through `sh .devkit/runtime/devkit.sh cli <subcommand> ...` (for example `sh .devkit/runtime/devkit.sh cli doctor . --json`).

`make package app=desktop` builds a Linux Electron directory and archive; `make package package=ui` builds a verified `.tgz` tarball and receipt (`publint` + `@arethetypeswrong/core`) for publishable libraries.

## Update an existing project

Choose the target release, inspect the proposed changes and migration notes, then apply them on a branch:

```sh
git switch -c chore/devkit-0.5.0
npx @docouno/devkit@0.5.0 update . --to 0.5.0 --preview
npx @docouno/devkit@0.5.0 update . --to 0.5.0
make setup
make check
```

Pass `--reconcile-lock` when transitive lockfile dependencies require re-resolution across toolchain updates. Review and merge the resulting diff. Updating the CLI alone does not update a project's accepted environment. Application source, product dependencies and product-owned Make/Compose files retain their ownership; conflicting edits to managed files stop the update.

After an interrupted update, `npx @docouno/devkit@0.5.0 recover .` restores the known transaction state. Unrecognized conflicting edits require resolution before recovery can proceed.

Devkit `0.5.0` upgrades `0.4.x` projects directly (`minSupportedFromVersion: "0.4.0"`). For `0.2.x` or `0.3.x` repositories, bridge through `0.4.1` first (`npx @docouno/devkit@0.4.1 update . --to 0.4.1 && make setup && make check`), then upgrade to `0.5.0`.

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
