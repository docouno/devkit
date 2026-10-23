# Verification

Use the root Make interface. Implementation checks run in the pinned Node container. Python black-box harnesses drive actual released CLI, Make and Docker entrypoints; Python is an author prerequisite only.

| Target                  | Scope                                                                                                           |
| ----------------------- | --------------------------------------------------------------------------------------------------------------- |
| `make check`            | Types, lint, formatting, shell syntax, core transactions and environment rules                                  |
| `make verify-runtime`   | Runtime metadata, native registration, read-only inputs, receipts, cancellation and checkout isolation          |
| `make verify-rust`      | Native Rust helper, nonempty tests/doctests, formatting, Clippy, pins and locked offline behavior               |
| `make verify-bootstrap` | Rendered remote bootstrap, version selection, real HTTP init/add/update/recover and failure cases               |
| `make verify-packaged`  | Isolated Node/Rust/web/fullstack consumers, shared UI, real browser/packaged Electron, add and upgrade          |
| `make verify-registry`  | Real npm archives and canonical registry locks with fresh caches, exports, native checks and integrity failures |
| `make verify`           | All targets, building the two local release versions first                                                      |

The author environment suite installs the exact Vitest peer into a temporary nested layout to test its public declaration against native types; this integration case can access npm. Consumer check/test/build commands use their installed locked dependencies without acquisition.

The default pair is 0.2.0 and 0.2.1. Override with `version=X.Y.Z next_version=A.B.C`. These are local previews, not public releases. Preserved 0.1.0/0.1.1 archives provide separate legacy migration inputs; the harnesses record whether those optional inputs were exercised.

Evidence lives under ignored `.artifacts/`. Tests record source/artifact identities and exact commands. Repeating a green test against identical inputs is unnecessary; changed implementation requires affected proofs to be refreshed. Consumers are outside the author tree and cannot rely on its dependencies. Resources are identified by exact names/labels; cleanup does not prune Docker or install host SDKs. Toolchain images/build cache remain available.

## What the suites establish

Core tests exercise composition, field ownership, lockfile graph preservation, stale inputs, interrupted commits and recovery conflicts. Runtime tests cover the actual bundled metadata controller and native task boundary. Native Rust tests include compile-only and executable doctests, ignored cases and formatting policies. Environment tests deliberately introduce source coverage, configuration, type/lint and workspace boundary failures.

The packaged matrix additionally uses the real Rust HTTP API, browser rendering, Electron main/preload/renderer and the packaged ELF/ASAR. A changed shared UI is observed in both development and production. It verifies product edits survive add/update operations and that checks leave source/configuration/locks unchanged. Test-only browser/CDP helpers are not shipped in application templates.

The bootstrap suite covers the acquisition path separately: explicit versions, accepted-version defaults, malformed/missing/corrupted releases, spaces in paths, cancellation and resource cleanup. The registry suite proves canonical registry installation using real package bytes; it is not an npm publication/authentication test.

## Limits

These suites establish the recorded scenarios on Linux/Docker Compose. They do not establish compatibility with another host OS, remote Docker, every native dependency or an arbitrary graphical session. Packaged Electron is verified in a controlled Xvfb display; interactive X11 forwarding has a separate probe.

Static source/configuration guards cover declared semantic roots and supported native runners. Computed imports, custom test harnesses and application test completeness remain outside that claim. Container cleanup cannot guarantee recovery from SIGKILL, daemon failure, reboot or detached unowned processes. See [architecture](architecture.md) and the [environment package](../packages/env/README.md) for the exact contract.

## Local release evidence — 23 October 2026

The final source checks passed types, lint, formatting and shell syntax, plus **38 core tests and 48 environment tests**. The final native runtime suite passed **24/24** in 157.240 seconds, with its original time limits. The separate native Rust helper suite passed **9/9** on unchanged helper/recipe bytes. Runtime evidence is `.artifacts/v2-runtime-final-clean.log`; source evidence is `.artifacts/v2-final-check.log`.

Version 0.2.0 is prepared locally; 0.2.1 is the upgrade test version. Neither was published. Each standalone payload contains 126 hash/mode-bound files; each environment npm archive contains 18 entries. Archive inventories, package exports, required notices and source/build parity were independently checked.

The full packaged matrix passed **109/109 results**: 92 commands (including 19 expected failures) and 17 additional assertions, in 414.670 seconds. It covered standalone Node, web and Rust, shared web/Electron UI, state-preserving HMR, production web, extracted packaged Electron ELF/ASAR, real Rust HTTP replies, add-app after product edits, 0.2.0 → 0.2.1 adoption, real 0.1.0 migration and conflict/recovery behavior. Source purity and read-only assertions passed; no host language SDK ran. All five checkout container/network scopes were cleaned. Receipt: `.artifacts/packaged/3be05b0ca6514279b3913e81d5cdac98/receipt.json`.

The final standalone payload differs from that tested candidate only in the launcher's help text. The execution body, compiled CLI, environment package, runtime and templates remain byte-identical; the final help and archive identities were checked separately. The recovery matrix uses an explicitly synthetic interrupted journal; real interruption points have independent core tests.

The remote transport matrix passed 73 assertions: 35 current, 33 legacy and 5 migration. Seventeen transport safety regressions also passed on the same rendered script. After the help-only packaging change, all 35 current transport cases were repeated against the exact final archives listed below; receipt `.artifacts/bootstrap/c2187178baf643b3a1e93c58daee4df0/receipt.json` confirms their hashes and clean resource removal. The earlier legacy/safety results are evidence for the unchanged transport implementation, not a claim that those older archive bytes are the final release. Fresh-cache registry tests installed both actual environment archives, ran checks/builds before and after update, and rejected wrong integrity and an absent version; the fixture recorded 640 requests.

| Version      | Standalone archive SHA256                                          | Release manifest SHA256                                            |
| ------------ | ------------------------------------------------------------------ | ------------------------------------------------------------------ |
| 0.2.0        | `6b2d31c35bbc3c7961e59790491e8aa00e0e39e18c1a927a01a573f6782008cc` | `39b085a22605e99e457c8c945866dc8f987afa4ef493dbe188d4dd2b59f21092` |
| 0.2.1 (test) | `4c76a3563aa2015f6b19038f8c26022bd46fa8a1760fd8bf65ae63ee304d1e19` | `6e882963ca60051616ed53a13d3546a4f213a20cbd2ad4a10d4f42f929fc2c61` |
