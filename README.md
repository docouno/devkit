# Devkit

A shared development environment for independent TypeScript and Rust repositories. Create applications and npm libraries from modular templates, run native tools in pinned containers, and adopt environment updates through a Git diff.

Requires Linux x86_64, a local Docker Engine with Docker Compose 5.0.0 or newer, and GNU Make. Node.js is needed only for the optional npm entry point.

## Create a project

Start the interactive CLI:

```sh
npx @docouno/devkit@0.4.0
```

Or provide the choices explicitly:

```sh
npx @docouno/devkit@0.4.0 init my-product --app web=react-web
cd my-product
make setup
make check
make dev app=web
```

Without host Node.js:

```sh
curl -fsSL https://github.com/docouno/devkit/releases/download/v0.4.0/install.sh | sh -s -- init my-product --app web=react-web
```

Both entry points use the same native CLI. Projects record an exact release and its integrity; ordinary development commands use the verified local cache.

## Applications, libraries and presets

Application templates include `node-api`, `rust-api`, `react-web`, and `electron-react`. Package templates include `ts-library`, `react-ui`, and `react-client`. Repositories can host `0..N` applications and `1..M` packages — including standalone library workspaces without applications:

```sh
npx @docouno/devkit@0.4.0 init my-sdk --package sdk=ts-library
npx @docouno/devkit@0.4.0 init my-design-system --preset design-system
npx @docouno/devkit@0.4.0 init fullstack-product --preset fullstack-desktop
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

Applications are runnable product entry points. Packages hold shared or publishable code (`ts-library` and `react-ui` start with `"private": true` and opt into publication in `package.json`). The `design-system` preset pairs a `react-ui` package with `apps/playground` using source-level Vite HMR. npm components share a root workspace and lockfile; Rust components own their Cargo manifests and locks.

Add components as the repository grows:

```sh
npx @docouno/devkit@0.4.0 add-app . --name api --template rust-api
npx @docouno/devkit@0.4.0 add-package . --name ui --template react-ui
npx @docouno/devkit@0.4.0 add-app . --name desktop --template electron-react --use ui
make setup
```

Use `register --help` to adopt an existing component without replacing its source, and `sync` after editing `package.json` metadata (`name`, `"private": false`, `exports`, `publishConfig`) or `.npmrc`:

```sh
npx @docouno/devkit@0.4.0 sync .
```

## Development

| Command            | Purpose                                                                   |
| ------------------ | ------------------------------------------------------------------------- |
| `make setup`       | Prepare accepted images and dependencies                                  |
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
npx @docouno/devkit@0.4.0 exec --component apps/web --write --network bridge -- npm install zod
make setup
make check
```

`make package app=desktop` builds a Linux Electron directory and archive; `make package package=ui` builds a verified `.tgz` tarball and receipt (`publint` + `@arethetypeswrong/core`) for publishable libraries.

## Update an existing project

Choose the target release, inspect the proposed changes and migration notes, then apply them on a branch:

```sh
git switch -c chore/devkit-0.4.0
npx @docouno/devkit@0.4.0 update . --to 0.4.0 --preview
npx @docouno/devkit@0.4.0 update . --to 0.4.0
make setup
make check
```

Pass `--reconcile-lock` when transitive lockfile dependencies require re-resolution across toolchain updates. Review and merge the resulting diff. Updating the CLI alone does not update a project's accepted environment. Application source, product dependencies and product-owned Make/Compose files retain their ownership; conflicting edits to managed files stop the update.

After an interrupted update, `npx @docouno/devkit@0.4.0 recover .` restores the known transaction state. Unrecognized conflicting edits require resolution before recovery can proceed.

The native npm/curl entry points upgrade public `0.2.x` and `0.3.x` projects directly.

## Documentation

Every initialized repository includes a concise offline reference at `.devkit/runtime/GUIDE.md`. Detailed guides live in [`docs/`](docs/architecture.md):

- [Architecture](docs/architecture.md) — `0..N` apps / `1..M` packages graph, tooling profiles, native manifest authority, and container security isolation.
- [Workflows](docs/workflows.md) — creating apps and libraries, `--preset design-system` live HMR, `--use`, `npm:` aliases, and `devkit sync`.
- [Publishing Libraries](docs/publishing.md) — opting into `"private": false`, `make package` verification, npm OIDC provenance, and GitLab Package Registry.
- [Continuous Integration](docs/ci.md) — GitHub Actions, GitLab CI, custom enterprise CAs (`DEVKIT_CA_CERT`), and airgapped runners.
- [Upgrading](docs/upgrades.md) — `devkit update`, `--reconcile-lock`, cumulative migration notes, forward-dispatch, and `devkit recover`.

## CLI and automation

Use `--help` for command options. Complete arguments run without prompts; `--non-interactive` makes missing choices an error. Lifecycle and metadata commands support `--json`. Runtime tasks stream native output and preserve their exit codes.

## Develop Devkit

Run `make help` for author commands. Releases are built, tested and published through [GitHub Actions](https://github.com/docouno/devkit/actions/workflows/release.yml).

Before releasing a version, add its changes to [CHANGELOG.md](https://github.com/docouno/devkit/blob/main/CHANGELOG.md). The release workflow uses that version's entry as its description; build and verification metadata stays in a collapsed section.

## License

[MIT](LICENSE). Distributed binaries include third-party notices.
