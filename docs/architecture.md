# Architecture and decisions

## Keep the product interface small

A consumer declares apps in a root Makefile. Shared Make targets delegate to native app scripts or Cargo. Native package managers own dependency graphs and locks; Make expresses cross-app build ordering and product Compose owns service networking. This works for a single app without forcing a polyglot workspace model on it.

A universal build graph such as Bazel/Pants would add a second dependency model and remote-cache/toolchain machinery before this first scope demonstrates that need. A JS-specific task runner would make Rust a guest of a Node workspace. The current boundary centralizes repeatable tool execution and leaves native build semantics with each app. Reconsider a graph system when measured cross-app rebuild cost and dependency scale justify it.

## TypeScript environment is a library

`@docouno/env` exports rooted ESLint configurations, compiler-option baselines, a Prettier configuration, source verification and native test reporters. Applications invoke installed ESLint, TypeScript, Prettier, Node or Vitest binaries directly. Plugins/parsers are environment dependencies; native compiler/linter versions are compatible application peers. This avoids binary lookup through guessed hoisted paths. Published executable modules are ordinary JavaScript because Node does not strip TypeScript under `node_modules`.

The first suite uses ESLint 10, TypeScript 6 and typescript-eslint 8.72. TypeScript 7 with Oxlint was also exercised; it handled the chosen semantic fixtures. The ESLint suite was selected for the supported plugin/custom-rule surface, and typescript-eslint's peer range excludes TS7. This is a compatibility choice, not a claim that TS7 is inherently unsuitable. Exact versions and locks change in an accepted release, since presets can change between minor releases. [typescript-eslint configurations](https://typescript-eslint.io/users/configs/), [Node TypeScript support](https://nodejs.org/api/typescript.html).

Running a compiler and linter successfully does not prove every intended source file was checked. The package's verifier validates declared semantic roots against the real compiler graph and typed ESLint configuration, rejects ignored declared source and disabled typechecking, and checks narrow explicit module boundaries. Files outside declared roots and computed runtime imports are outside that coverage promise. See [the package contract](../packages/env/README.md).

Native Node and Vitest can consider a run with no executable tests successful in some cases. Their adapters retain native diagnostics and reject empty/skip-only results. These are runner-specific contracts, not a substitute test runner. Rust's distinct doctest policy is described below.

Formatting is a separate check and an explicit fix. Vite builds transpile TypeScript; a separate compiler gate is required. [Prettier integration](https://prettier.io/docs/integrating-with-linters), [Vite TypeScript](https://vite.dev/guide/features.html#typescript).

## Rust keeps native configuration

An npm-style environment dependency cannot propagate a Cargo workspace, toolchain pin, rustfmt configuration or Clippy command across independent repositories. Each Rust app owns native Cargo metadata and locks plus short explicit configuration. The release manages the toolchain pin and ships the native checking helper and a Docker recipe. Rustup resolves from the real app working directory, with no masking `RUSTUP_TOOLCHAIN` override. [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html), [rustup overrides](https://rust-lang.github.io/rustup/overrides.html).

The pinned official Rust image lacks rustfmt/Clippy. Explicit setup derives an image from its digest-pinned base, checks native component identities and records a recipe-labelled immutable local image ID. Gates use that ID without rebuilding or pulling. Recipe hash, base digest and resulting image ID are distinct; independently rebuilt images are not promised bitwise identical.

Setup fetches with `--locked`. Ordinary native gates use `--locked --offline`, explicit rustfmt configuration and Clippy workspace/all-target coverage with warnings denied. Demonstrated unknown/unstable formatter-option warnings are errors. Native default Cargo tests include doctests. The nonempty-test gate requires a non-ignored native case, including compile-only `no_run` doctests; it does not claim every case executes a body. Custom `harness=false` discovery is outside this policy. [Cargo test](https://doc.rust-lang.org/cargo/commands/cargo-test.html), [rustdoc tests](https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html).

## Accepted versions stay explicit

A project commits a versioned runtime snapshot and accepted ownership manifest. It does not execute a downloader during check. The metadata verifier binds the manifest, file hashes/modes, checksum table, physical app registration and owned tool dependencies. A small pinned Node control container verifies generic JSON metadata even for Rust-only projects; native Rust gates still call Cargo directly. The controller has no network, writable root or dependency/cache mounts.

The runtime uses a per-checkout admission lock, setup receipts and a manifest snapshot barrier. A failed repeat install invalidates earlier acceptance before it mutates dependencies. An update between Make leaves cannot result in a successful check mixing two generations. Reserved writable paths reject symlinks before being opened or mounted. These checks detect accidental drift and unsafe redirection; the checkout is not a hostile-code sandbox or an attacker-proof filesystem transaction against concurrent malicious edits.

Update ownership is intentionally narrow: runtime files, local npm archives, Rust toolchain pin and named native tooling dependency fields. Product source/configuration stays owned by the project. New configuration migrations need an explicit versioned implementation and regression proof; this version does not pretend to be a general-purpose source codemod engine.

## Container lifecycle is part of a gate

Tool tasks use finite noninteractive one-shot containers with caller UID/GID and read-only inputs. A guard owns the Docker creator process group, reaps it before exact name/label cleanup and reports cleanup failures. This order addresses a reproduced cancellation race before a container was ready. Product development uses separate detached Compose services.

Each canonical checkout path has its own Compose project identity. Starter ports bind to localhost with ephemeral host allocation; `make status` reports them. Down preserves data and does not touch another checkout. This avoids project-name/port collisions without a new port allocator. [Compose project names](https://docs.docker.com/compose/how-tos/project-name/), [Compose services](https://docs.docker.com/reference/compose-file/services/).

Terminal process-group INT/TERM and TERM to the Make PID are validated. INT sent only to Make's PID is not a supported cancellation contract. SIGKILL, daemon outage/hang, reboot and detached creator descendants have no cleanup guarantee. Runtime cleanup does not act on unowned containers or prune Docker state.
