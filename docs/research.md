# Research behind the first foundation

Research and executable comparisons were performed on 23 October 2026. The selected implementation is described in [architecture](architecture.md); the decisions below distinguish ecosystem mechanisms from the experiments that justified them. Exact accepted tools are pinned in the release and native locks.

## Runtime delivery and project updates

| Candidate                                       | Useful property                                                                                  | Decision for this scope                                                                                                                                                |
| ----------------------------------------------- | ------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Checked-in versioned runtime snapshot           | Checkout and Git diff contain the accepted execution code; rollback restores it with the project | Selected, with explicit file ownership and identities                                                                                                                  |
| Launcher downloads a checksum-addressed runtime | Smaller consumer checkout                                                                        | Deferred: acquisition, extraction, cache concurrency and launcher compatibility add a bootstrap subsystem for a small payload                                          |
| SDK Git submodule                               | Git already pins a source commit                                                                 | Rejected for the first baseline: an extra clone/init workflow and worktree caveats, while the consumer needs a runtime artifact rather than this whole author monorepo |
| Copier template updates                         | Preserves a product's local template diff, supports migrations/conflicts                         | Viable alternative for broad template evolution; current updates deliberately own only runtime/tool integration and do not reapply product source                      |

These are judgments about this product's needs, not claims that the alternatives are generally unsuitable. Git's `submodule update --remote` chooses remote-tracking state rather than merely using the recorded gitlink. Copier's template merge does not itself provide semantic validation of npm peer constraints and staged native locks. The selected updater therefore has a smaller ownership contract and tests that specific contract. [Git submodule update](https://git-scm.com/docs/git-submodule), [Git worktree limitations](https://git-scm.com/docs/git-worktree#_bugs), [Copier updates](https://copier.readthedocs.io/en/stable/updating/).

A custom universal package graph was unnecessary for the demonstrated workload: native app graphs, Make edges and HTTP service interaction suffice. A single-language app remains ordinary native code. The initial consumer checks prove one explicit cross-app build edge rather than claiming automatic dependency discovery.

## TypeScript comparison

Two installed families were exercised in separate consumers with real packed shared configuration:

| Family | Installed candidate                                        | Observed result                                                                                                           |
| ------ | ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| ESLint | ESLint 10.13.0, typescript-eslint 8.72.0, TypeScript 6.0.3 | Typed promise/unsafe diagnostics, compiler errors, runtime-global separation and emitted Node modules behaved as expected |
| Oxlint | Oxlint 1.89.0, oxlint-tsgolint 7.0.2005, TypeScript 7.0.2  | The same selected semantic fixtures worked; this was fixture parity, not full plugin parity or a performance comparison   |

ESLint/TS6 was selected for the tested supported peer range and plugin/custom-rule surface. TS7 remains a viable future change requiring the full React/custom-rule and coverage matrix. No benchmark claim was inferred from these small, unequal rulesets. [typescript-eslint configuration support](https://typescript-eslint.io/users/configs/), [Oxlint typed linting](https://oxc.rs/docs/guide/usage/linter/type-aware).

The important counterexample was a validly typed floating promise in an ignored source file: both the compiler and linter returned success. A malformed ignored file was an inadequate probe because the compiler still caught its syntax. ESLint `--no-ignore` caught the semantic fixture; Oxlint's flag did not override configured ignore patterns, which its documented model distinguishes. This led to the declared-source/effective-configuration guard rather than reliance on a zero exit alone. [Oxlint ignoring](https://oxc.rs/docs/guide/usage/linter/ignore-files).

The package was installed from real tarballs outside the provider tree. Rooted configs worked from app and repository working directories, public exports loaded without source links, incompatible TS peers failed installation, and emitted JS executed. Node's inability to strip TypeScript in `node_modules` led to publishing JavaScript helpers. Separate Prettier checking removed the need for a formatter-through-linter layer.

Native runner probes exposed another false-success case: an empty Node test module, and successful runs containing only skipped cases. The production Node/Vitest adapters reject these while preserving native failure diagnostics. Independent review additionally found a genuine triple-slash `ts-nocheck` bypass; its regression disables the optional lint rule and demonstrates compiler success before verifying that the guard rejects it.

## Rust comparison

Cargo workspace dependency/lint inheritance applies to opting-in workspace members. It is not a reusable configuration import from an environment dependency crate into unrelated repositories. Cargo config includes accept local files; they do not deliver a crates.io or URL-hosted tool environment. Rustfmt and Clippy also have distinct configuration surfaces. [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html), [Cargo configuration includes](https://doc.rust-lang.org/cargo/reference/config.html#including-extra-configuration-files), [rustfmt configuration](https://github.com/rust-lang/rustfmt#configuring-rustfmt), [Clippy configuration](https://doc.rust-lang.org/stable/clippy/configuration.html).

The selected model is a pinned container plus native per-app configuration and a small shared command helper. A published `cargo-xtask` style executable could centralize commands but would still need bootstrapping, native config delivery and a toolchain. Nix/devshell-style host environments were outside the container-first baseline. Neither alternative was needed to meet the demonstrated first workload.

Actual Rust probes established that working directory affects toolchain/config selection; `--manifest-path` alone is insufficient. The official pinned image lacked rustfmt/Clippy, so the shipping recipe explicitly installs/checks those components. Formatter probes required an explicit configuration path and strict handling of native warnings for ignored/unsupported options. Locked offline commands rejected dependency drift. Native discovery includes ignored tests and compile-only doctests, so the final policy counts non-ignored cases and explicitly does not equate that count with executed bodies.

## Container and resolver experiments

Attached Compose `up` can return success after interruption; it is therefore not the success signal for a finite gate. Production tasks use one-shot noninteractive containers and a guard that preserves failure/cancellation. A deterministic cancellation before readiness exposed an inspect-before-creator-exit race. The guard now terminates/reaps the creator first and verifies only exact owned resources afterward. These experiments are permanent runtime tests. [Compose up](https://docs.docker.com/reference/cli/docker/compose/up/), [Compose run](https://docs.docker.com/reference/cli/docker/compose/run/).

Two same-name checkouts proved that canonical-path project identities plus ephemeral localhost ports isolate real development stands. One checkout's down left the other running; named data survived down/up. Read-only source mounts, reserved-path symlink checks, receipt invalidation before repeat setup and a final manifest-generation barrier were all tested against concrete failure cases.

For npm 11, a scope-only temporary registry override did not exercise canonical public lock URLs: fresh `ci` still fetched the public URL. A full test-only registry override with real own-scope archives and public dependency redirects worked without changing the canonical lock. The fixture needs no publication/auth API or permanent server. The production local mode is separately explicit and copies real tarballs into the consumer. [npm registry behavior](https://docs.npmjs.com/cli/v11/using-npm/registry/), [npm registry-host replacement](https://docs.npmjs.com/cli/v11/using-npm/config/#replace-registry-host).

The current executable proofs, artifact identities and known limits are recorded in [verification](verification.md). Early probes selected mechanisms; final isolated consumer and independent review results establish the shipped implementation.
