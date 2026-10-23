# Architecture and decisions

## Native components, one command interface

The accepted manifest registers applications and libraries independently of their language. The root Make interface delegates to each component's native scripts or Cargo. Node components share an explicit root npm workspace and lock. Rust components keep Cargo metadata and locks; Rust-only projects do not acquire a root npm workspace.

A component's template records where its initial source came from. Its current tooling profile determines managed tool dependencies and runtime behavior. Updating Devkit does not rerun its template. Project presets compose ordinary components and service bindings; they are not another build system.

The runtime derives Make registration and prerequisite edges from accepted component metadata. Native local-library dependencies are checked against that metadata. Library prerequisites build before dependent applications, and their checks participate in aggregate checks. Root Make and Compose files remain extensible product files. Generated integration lives under `.devkit` and is validated before tasks run.

Bazel/Pants-style build graph ownership and a JS task runner are unnecessary for this scope. Native dependency graphs, a small registration layer, Make ordering and explicit service connections cover the validated workloads. Remote build caches and incremental cross-project scheduling would need separate evidence and design.

## TypeScript tools

`@docouno/env` exports rooted ESLint configurations, compiler baselines, Prettier configuration, source verification and native test reporters. Applications invoke installed tools directly. Parsers/plugins are environment dependencies; compiler/linter versions are compatible component peers. Executable helpers ship as JavaScript because Node does not strip TypeScript inside `node_modules`. [Node TypeScript support](https://nodejs.org/api/typescript.html).

The accepted family is TypeScript 6, ESLint 10 and typescript-eslint 8.72. TypeScript 7 and Oxlint were exercised separately, but do not have the selected plugin compatibility contract. Exact versions are in native locks, not a floating `latest` selector. [typescript-eslint support](https://typescript-eslint.io/users/configs/).

Node API output uses `tsc`; React web uses Vite; React libraries use `tsc` plus explicit CSS copying. Electron uses electron-vite for main/preload/renderer and electron-builder for a Linux directory, then GNU tar for the archive. The Devkit author factory uses esbuild only to bundle its own parser dependencies. Vite is not treated as a type checker. [Vite TypeScript](https://vite.dev/guide/features.html#typescript).

The source verifier checks declared semantic files against the actual TypeScript program and typed ESLint configuration. It rejects ignored declared source, disabled strict typechecking and missing mandatory typed rules. Compiler and linter still run separately. Native Node/Vitest reporters reject successful empty or skip-only suites without replacing those runners.

For registered workspace imports, the verifier uses TypeScript resolution with the file's real options and import mode. A declared local library must resolve through its public exports. Static imports into another application or a library's private source fail, including aliases and relevant asset paths. Computed runtime imports and files outside declared source roots are not covered by this static contract.

## Shared web and Electron UI

The fullstack preset shares a real React package with explicit public exports. Development exports expose source for Vite HMR; production exports point to built files. Both web and Electron renderer use that package. Platform-specific API access stays in the clients: browser fetch uses `/api`; Electron uses a narrow preload bridge and a main-process HTTP adapter.

An explicit API binding configures the browser development proxy and enables its API panel in production builds. It does not embed a Docker service hostname into the browser bundle. Deploying that web output requires a normal reverse proxy for `/api`. Packaged Electron reads its API target at launch through `DEVKIT_API_TARGET`; endpoints, credentials and deployment belong to the product.

Electron main/preload/renderer are built within one application. The renderer has sandboxing, context isolation, no Node integration, denied external window creation and a restricted navigation/IPC sender boundary. The environment's desktop profile forbids direct native imports/globals in renderer source, including literal dynamic imports. These checks are useful guardrails, not an application-security audit or analysis of arbitrary computed code.

The derived Linux desktop image runs with the caller UID/GID, dropped capabilities, no-new-privileges and a versioned seccomp profile. Headless development uses Xvfb. Interactive development uses an existing X11 display/socket/authorization file read-only; it does not run `xhost`. Packaging is a Linux directory and tar archive. Signing, installers, auto-update, Wayland-native forwarding and other desktop operating systems are outside this release.

## Rust remains native

A Cargo dependency cannot import a toolchain pin, workspace policy and rustfmt configuration into unrelated repositories like an npm config package. Devkit therefore delivers a pinned container recipe, small native configuration files and a shared command helper. Native Cargo metadata remains at the Rust component. [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html), [rustup overrides](https://rust-lang.github.io/rustup/overrides.html).

Setup derives an image from the digest-pinned official base and adds rustfmt/Clippy. The runtime records its immutable local image ID and recipe label. Normal gates neither build nor pull images. Base digest, recipe hash and resulting image ID are distinct identities; separate image builds are not promised bitwise identical.

Rust setup fetches with `--locked`; gates use `--locked --offline`. The shared build command uses Cargo's default development profile (`target/debug`); a product release/packaging pipeline can select its optimized Cargo profile separately. Formatting uses the explicit config, refuses ignored/unstable options and does not permit formatting to be disabled. Clippy checks all workspace targets with warnings denied. Tests include doctests; at least one non-ignored native case is required. A `no_run` doctest is compile validation and is not described as executing a body. Custom `harness=false` discovery is outside the policy. [Cargo test](https://doc.rust-lang.org/cargo/commands/cargo-test.html).

Rustdoc builds temporary executable tests. Its `/tmp` tmpfs explicitly permits execution while source, configuration, locks and the container root remain read-only. This requirement was reproduced against the shipping image and is covered by the native helper suite. [Rustdoc tests](https://doc.rust-lang.org/rustdoc/documentation-tests.html).

## Accepted state and execution

A consumer commits its runtime and accepted manifest. A read-only, networkless Node control container validates schema, managed file hashes/modes, the complete checksum table, component registration, native inputs and generated integration. It has no dependency/cache mounts. Rust commands still execute directly through Cargo.

Setup records a fingerprint of Node's accepted native input closure, including local dependency metadata and archives. Failed or interrupted repeated setup invalidates the prior receipt. Native manifest/lock changes require setup; changed component integration requires the accepted release's update/register path. Root shrinkwrap is explicitly rejected in package-lock mode so admission cannot validate a lock npm ignores.

One checkout lock serializes finite operations and lifecycle changes. Root Make recipes stay serial even when `-j` is inherited, preventing shared-library check/build prerequisites from competing for that lock; native tools retain internal parallelism. Make captures the accepted manifest at invocation and checks it between leaves and after aggregates, so a version change cannot silently mix task generations. Reserved writable paths reject symlinks. These controls detect accidental drift and unsafe redirection; a developer-controlled checkout is not a hostile-code sandbox.

Checks/test/build use read-only source mounts with writable dependency/cache/output paths. Fix explicitly permits source writes. Root `.env` is not implicitly injected. Product Compose can opt into its own environment files and overrides.

Before development startup, the runtime reads the native normalized Compose model and admits every registered application that the selected service can start through dependencies. Unrelated unprepared components do not block selection. This closes the indirect-start path for stale image IDs and missing Node setup receipts. [Compose dependencies](https://docs.docker.com/compose/how-tos/startup-order/).

Each canonical checkout path determines an isolated Compose project identity. Default server ports use ephemeral localhost bindings; `make status` reports them. Down preserves named data and does not remove orphan services. [Compose project names](https://docs.docker.com/compose/how-tos/project-name/).

## Acquisition, updates and cancellation

The remote bootstrap is acquisition only. It obtains an explicit version, checks bounded archive contents and delegates to the same standalone CLI. Daily tasks do not download a newer runtime. Updates are staged and compare both owned bytes and product-rooted native dependency identities before commit; recovery restores known journaled bytes and refuses unknown edits. Details are in [releases](releases.md).

One-shot containers are guarded by exact names and invocation labels. Cancellation terminates/reaps the creator before cleanup and makes cleanup failure visible. Terminal process-group INT/TERM and TERM to Make are tested. INT delivered only to Make's PID, SIGKILL, daemon failure/hang, reboot and deliberately detached creator descendants have no cleanup guarantee. The runtime never prunes unrelated Docker state.
