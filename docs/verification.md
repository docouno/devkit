# Verification

Use the root Make interface. The implementation tests run in the pinned Node container. Python black-box harnesses drive actual Make/Docker entrypoints; Python is an author prerequisite only.

| Target                 | Scope                                                                                                                                     |
| ---------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `make check`           | Factory/release TypeScript, implementation lint/format, core and environment tests                                                        |
| `make verify-runtime`  | Runtime admission, registration, read-only inputs, receipts, cancellation, isolation and negative cases                                   |
| `make verify-rust`     | Native Rust helper behavior, doctest policy, formatting, Clippy, pins, locks and app cwd                                                  |
| `make verify-packaged` | Standalone archives in independent Node/React/Rust/mixed repositories, source purity, failures, development interaction and local upgrade |
| `make verify-registry` | Canonical registry init/update/ci with real npm archives, fresh caches, checks/builds and resolver negatives                              |
| `make verify`          | All of the above, including building the two local release versions                                                                       |

The packaged/registry update pair defaults to 0.1.0 and 0.1.1; use `version=X.Y.Z next_version=A.B.C` for a different pair. These are local author previews, not public releases.

Evidence is written under ignored `.artifacts/`. Tests use exact unique owned resources, preserve toolchain images/build cache and do not prune Docker. The standalone consumer tests operate outside the author source tree and use release contents rather than workspace links. The registry test mounts only the two releases and bounded fixture helper, with no author source/dependency tree.

Passing unit tests alone does not establish package export correctness, default registry resolution, actual Cargo components, Make cancellation or mixed-service communication. Each surface has its own proof. Test outcomes are tied to the source tree/artifact identities recorded with them; changing bytes requires rechecking the affected surface.

Platform limits and native source/test coverage boundaries are stated in [architecture](architecture.md) and [the environment package](../packages/env/README.md). This foundation enforces configured gates; it is not a hostile-project sandbox, application-security audit or proof of test completeness.

## Verified local artifacts: 23 October 2026

The final implementation passed the root type/lint/format/shell checks, **18 core tests** and **36 environment tests**. On identical runtime/helper bytes, the permanent black-box suites passed **21 runtime tests** and **9 native Rust helper tests**. The runtime/helper source hashes and exact coverage are recorded in their test READMEs; independent follow-up review closed their reproduced findings without an unnecessary full-suite repeat.

The final standalone consumer run recorded **60 results from 53 real commands** in 157.101 seconds. It created Node, React, Rust and mixed projects in independent directories outside this checkout and blocked host language SDKs. It verified host-valid creation hints, actual positive/negative gates, untracked-input purity, emitted HTTP servers, malformed-request survival, direct graceful shutdown, mixed proxy requests, explicit build ordering, finite logs, checkout isolation and local 0.1.0 → 0.1.1 update/preservation/conflict handling. The ignored receipt is `.artifacts/packaged/322821b006174012a70185ce8e5f1989/receipt.json`. The same receipt folder contains `fnd01-regression.json`, proving the hint guard rejects the old actual stdout and accepts the new output. Crash recovery and cancellation also have separate core/runtime proofs.

The final registry run installed both real package versions with fresh caches, ran native checks/builds before and after the released updater API, preserved canonical locks through `ci`, and rejected wrong integrity/missing versions with the expected native diagnostics. The bounded fixture recorded 900 requests. Output is `.artifacts/registry/latest.log`. This is installation/update verification, not an npm publish/auth test.

| Version | Standalone archive SHA256                                          | Release manifest SHA256                                            |
| ------- | ------------------------------------------------------------------ | ------------------------------------------------------------------ |
| 0.1.0   | `93ceb7e474cbd242c7cfbefbc07fdde50a8356e3d20bc7d9947dc0979912a594` | `6931139e4575bdd4ff71f25b87bcb0ed4238e09b5b00569e2151baae09ef8a69` |
| 0.1.1   | `2802e12542b80409df637a0533e9a4e76170d85a978f24631e0501af2ce216ab` | `6a95647dfd2b0b3f7d860df3a724ab420b6438e99230ec494dda07fec80b1994` |

Each release contains 56 hash/mode-bound payload files plus `release.json`. These are local previews; publication was not performed. The 0.1.1 preview exists to prove version adoption, not to imply a second public release.

React's native UI tests and real Vite-to-Rust HTTP interaction passed. A full-browser smoke was attempted, but this session had no enabled browser (`Browser is not available: iab`); no real-browser result is claimed. The original reference snapshot remained unchanged: 10 repositories and four submodule checkouts verified.
