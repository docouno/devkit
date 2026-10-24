# Devkit

A shared development environment for independent TypeScript and Rust repositories. Create applications from modular templates, run native tools in pinned containers, and adopt environment updates through a Git diff.

Requires Linux x86_64, a local Docker Engine with Docker Compose 5.0.0 or newer, and GNU Make. Node.js is needed only for the optional npm entry point.

## Create a project

Start the interactive CLI:

```sh
npx @docouno/devkit@0.3.1
```

Or provide the choices explicitly:

```sh
npx @docouno/devkit@0.3.1 init my-product --app web=react-web
cd my-product
make setup
make check
make dev app=web
```

Without host Node.js:

```sh
curl -fsSL https://github.com/docouno/devkit/releases/download/v0.3.1/install.sh | sh -s -- init my-product --app web=react-web
```

Both entry points use the same native CLI. Projects record an exact release and its integrity; ordinary development commands use the verified local cache.

## Applications and shared packages

Templates include `node-api`, `rust-api`, `react-web`, `electron-react`, and the shared `react-client` package. The `fullstack-desktop` preset combines a Rust API with web and Electron applications using one React client:

```sh
npx @docouno/devkit@0.3.1 init fullstack-product --preset fullstack-desktop
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

Applications are runnable product entry points. Packages hold code reused by applications and expose it through their native public interfaces. npm components share a root workspace and lockfile; Rust components own their Cargo manifests and locks.

Add components as the product grows:

```sh
npx @docouno/devkit@0.3.1 add-app . --name api --template rust-api
npx @docouno/devkit@0.3.1 add-package . --name client --template react-client
npx @docouno/devkit@0.3.1 add-app . --name desktop --template electron-react --use client
make setup
```

Use `register --help` to adopt an existing component without replacing its source.

## Development

| Command            | Purpose                                                           |
| ------------------ | ----------------------------------------------------------------- |
| `make setup`       | Prepare accepted images and dependencies                          |
| `make check`       | Check source, types, formatting, lint and applicable native gates |
| `make test`        | Run the registered components' native test suites                 |
| `make build`       | Build the registered components                                   |
| `make dev app=web` | Start one application's development service                       |
| `make status`      | Show services and their local ports                               |
| `make down`        | Stop services while preserving data volumes                       |

Use `app=NAME` or `package=NAME` for setup, check, fix, test and build. `make down` stops the project stack. The root Makefile belongs to the product, so custom targets and dependencies remain part of the build.

Checks and builds keep source, configuration and lockfiles read-only. `make fix` explicitly permits formatting changes. To edit dependencies with the accepted toolchain:

```sh
npx @docouno/devkit@0.3.1 exec --component apps/web --write --network bridge -- npm install zod
make setup
make check
```

`make package app=desktop` builds a Linux Electron directory and archive. TypeScript uses its native compiler and linters; web uses Vite, Electron uses electron-vite, and Rust uses Cargo, rustfmt and Clippy.

## Update an existing project

Choose the target release, inspect the proposed changes, then apply them on a branch:

```sh
git switch -c chore/devkit-0.3.1
npx @docouno/devkit@0.3.1 update . --to 0.3.1 --preview
npx @docouno/devkit@0.3.1 update . --to 0.3.1
make setup
make check
```

Review and merge the resulting diff. Updating the CLI alone does not update a project's accepted environment. Application source, product dependencies and product-owned Make/Compose files retain their ownership; conflicting edits to managed files stop the update.

After an interrupted update, `npx @docouno/devkit@0.3.1 recover .` restores the known transaction state. Unrecognized conflicting edits require resolution before recovery can proceed.

The native npm/curl entry points also upgrade public 0.2.0 and 0.2.1 projects. Use the new entry point for this migration.

## CLI and automation

Use `--help` for command options. Complete arguments run without prompts; `--non-interactive` makes missing choices an error. Lifecycle and metadata commands support `--json`. Runtime tasks stream native output and preserve their exit codes.

## Develop Devkit

Run `make help` for author commands. Releases are built, tested and published through [GitHub Actions](https://github.com/docouno/devkit/actions/workflows/release.yml).

## License

[MIT](LICENSE). Distributed binaries include third-party notices.
