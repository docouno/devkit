# Daily Workflows

This guide covers day-to-day repository creation, component wiring, dependency management, local development, and workspace synchronization in Devkit `0.4.0`.

## 1. Create a New Repository

### Runnable Product Applications

Initialize a single-app or multi-app workspace:

```sh
npx @docouno/devkit@0.4.0 init my-product --app web=react-web
```

Or scaffold the fullstack preset (`apps/api` Rust API + `apps/web` React SPA + `apps/desktop` Electron app + `packages/client` shared React library):

```sh
npx @docouno/devkit@0.4.0 init my-suite --preset fullstack-desktop
```

### Standalone Libraries (Zero-App Workspace)

Create a package-only repository without scaffolding a dummy runnable application:

```sh
npx @docouno/devkit@0.4.0 init acme-sdk --no-app --package sdk=ts-library
```

Create a React component library repository:

```sh
npx @docouno/devkit@0.4.0 init acme-ui --no-app --package ui=react-ui
```

### Design System with Interactive Playground

Scaffold `packages/ui` (`react-ui`) pre-wired to `apps/playground` (`react-web`) with instant source HMR:

```sh
npx @docouno/devkit@0.4.0 init acme-design --preset design-system
```

After initialization, prepare pinned toolchains and workspace dependencies:

```sh
cd acme-design
make setup
```

## 2. Add Components and Wire Internal Packages (`--use`)

Add libraries or applications at any time and wire them to existing `packages/<slug>` components with repeatable `--use`:

```sh
npx @docouno/devkit@0.4.0 add-package . --name tokens --template ts-library
npx @docouno/devkit@0.4.0 add-package . --name ui --template react-ui --use tokens
npx @docouno/devkit@0.4.0 add-app . --name docs --template react-web --use tokens --use ui
make setup
```

`--use <slug>` adds `"<pkgName>": "file:../../packages/<slug>"` to the consumer's `package.json`, updates `package-lock.json`, and derives the build/check DAG automatically in `.devkit/manifest.json` and `.devkit/project.mk`.

## 3. Add or Update Third-Party Dependencies (`devkit exec`)

Never run host `npm install` or `cargo` directly on the checkout. Always execute package-manager mutations inside the pinned container with `--write` and `--network bridge`:

```sh
# Node.js / React / TypeScript component (including npm: aliases)
npx @docouno/devkit@0.4.0 exec --component packages/ui --write --network bridge -- \
  npm install clsx

npx @docouno/devkit@0.4.0 exec --component packages/ui --write --network bridge -- \
  npm install legacy-zod@npm:zod@^3.24.0

# Rust component
npx @docouno/devkit@0.4.0 exec --component apps/api --write --network bridge -- \
  cargo update --manifest-path Cargo.toml
```

Without host Node.js, use the root `Makefile` wrapper:

```sh
make exec package=ui write=1 network=bridge command="npm install clsx"
```

Follow dependency mutations with `make setup` to refresh the verified workspace lock receipt.

## 4. Edit Native Manifests and Synchronize (`devkit sync`)

Devkit treats `package.json`, `Cargo.toml`, and root `.npmrc` as product-owned sources of truth. When you edit package metadata directly — such as renaming `"name"` to `@corp/ui`, setting `"private": false`, adjusting workspace dependencies, or adding scoped private registries in `.npmrc` — synchronize the lockfile and Devkit projections:

```sh
# Preview planned lockfile and projection updates without writing
npx @docouno/devkit@0.4.0 sync . --preview

# Apply fast offline JSON sync (or online resolution when registry packages changed)
npx @docouno/devkit@0.4.0 sync .

# Reconcile drifted peer/transitive locks across the 3-stage ladder when needed
npx @docouno/devkit@0.4.0 sync . --reconcile-lock

make setup
```

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
make dev                    # Start development services (foreground / interactive)
make up                     # Start development stack in the background and wait for health
make status                 # Inspect running service containers
make logs app=playground    # Print recent service logs (add follow=1 to stream)
make down                   # Stop and remove project containers and networks
```

In zero-app library repositories, `make status`, `make logs`, and `make down` succeed as clean no-ops, while `make dev` and `make up` exit with a hint explaining how to add a playground app via `devkit add-app`.

## 6. Live Design-System HMR (`devkitWorkspaceSource`)

Web (`react-web`) and Electron (`electron-react`) applications using `devkitWorkspaceSource()` from `@docouno/env/vite` resolve internal `packages/*/src` TypeScript, TSX, and CSS entrypoints directly during `make dev`:

- Editing `packages/ui/src/Button.tsx` or `packages/ui/src/styles.css` triggers instant Vite Fast Refresh in `apps/playground` without running `tsdown` or modifying `packages/ui/dist/`.
- Because `devkitWorkspaceSource()` is strictly scoped to `apply: 'serve'`, `make test` and `make build` always validate the real compiled `dist/` output and public `exports` map of every workspace library.
