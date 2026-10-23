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

The local preview pair follows the source version (currently 0.2.1 and 0.2.2). Override with `version=X.Y.Z next_version=A.B.C`. These are local previews, not public releases. Preserved 0.1.0/0.1.1 archives provide separate legacy migration inputs; the harnesses record whether those optional inputs were exercised.

Evidence lives under ignored `.artifacts/`. Tests record source/artifact identities and exact commands. Repeating a green test against identical inputs is unnecessary; changed implementation requires affected proofs to be refreshed. Consumers are outside the author tree and cannot rely on its dependencies. Resources are identified by exact names/labels; cleanup does not prune Docker or install host SDKs. Toolchain images/build cache remain available.

## What the suites establish

Core tests exercise composition, field ownership, lockfile graph preservation, stale inputs, interrupted commits and recovery conflicts. Runtime tests cover the actual bundled metadata controller and native task boundary. Native Rust tests include compile-only and executable doctests, ignored cases and formatting policies. Environment tests deliberately introduce source coverage, configuration, type/lint and workspace boundary failures.

The packaged matrix additionally uses the real Rust HTTP API, browser rendering, Electron main/preload/renderer and the packaged ELF/ASAR. A changed shared UI is observed in both development and production. It verifies product edits survive add/update operations and that checks leave source/configuration/locks unchanged. Test-only browser/CDP helpers are not shipped in application templates.

The bootstrap suite covers the acquisition path separately: explicit versions, accepted-version defaults, malformed/missing/corrupted releases, spaces in paths, cancellation and resource cleanup. The registry suite proves canonical registry installation using real package bytes; it is not an npm publication/authentication test.

## Limits

These suites establish the recorded scenarios on Linux/Docker Compose. They do not establish compatibility with another host OS, remote Docker, every native dependency or an arbitrary graphical session. Packaged Electron is verified in a controlled Xvfb display; interactive X11 forwarding has a separate probe.

Static source/configuration guards cover declared semantic roots and supported native runners. Computed imports, custom test harnesses and application test completeness remain outside that claim. Container cleanup cannot guarantee recovery from SIGKILL, daemon failure, reboot or detached unowned processes. See [architecture](architecture.md) and the [environment package](../packages/env/README.md) for the exact contract.

## Public releases

```sh
make verify-public from_version=0.2.0 version=0.2.1
```

This command downloads the actual GitHub assets and public npm package bytes,
compares their identities, and runs the consumer matrix through the published
bootstrap in registry mode. It uses no publisher credentials, local archive
fallback or temporary registry. The canonical bootstrap URL is exercised without
an override. Standalone consumers begin at the first version, the fullstack
product uses the successor, and an edited project explicitly updates to it.
See [the public harness](../test/public/README.md).

The public matrix covers native gates and expected failures, source purity,
shared HMR, production web and extracted packaged Electron with real Rust HTTP,
all component operations, conflicts, no-op and synthetic journal recovery.
It does not claim a live crash from that synthetic recovery case; real
transaction interruption has separate core tests.

## Foundation acceptance baseline

The pre-publication 0.2.0 foundation passed 38 core and 48 environment tests,
24 native runtime tests and 9 native Rust helper tests. Its packaged matrix
passed 109 results: 92 commands (19 expected failures) and 17 additional
assertions. Web and Electron shared state-preserving HMR and both production
clients received real Rust HTTP replies. The original 0.1.0 migration was also
exercised. Full matrix wall time was 414.670 seconds.

Transport preparation covered 73 current/legacy/migration assertions and 17
safety regressions. Thirty-five current transport cases were repeated on the
final 0.2.0 archives. Fresh-cache registry checks installed the real npm tarballs,
ran native checks/builds and rejected wrong integrity and missing versions.

The first public 0.2.0 release reuses those exact verified package/runtime/template
bytes. Only launcher help was corrected after the full matrix; its execution
body stayed identical and the final artifact delta was independently audited.

| Version | Standalone archive SHA256                                          | Release manifest SHA256                                            |
| ------- | ------------------------------------------------------------------ | ------------------------------------------------------------------ |
| 0.2.0   | `6b2d31c35bbc3c7961e59790491e8aa00e0e39e18c1a927a01a573f6782008cc` | `39b085a22605e99e457c8c945866dc8f987afa4ef493dbe188d4dd2b59f21092` |

Public verification receipts are recorded under `.artifacts/public/<run-id>` and
the linked consumer receipt. Release checksums and `release.json` are served with
each GitHub release. Evidence is tied to exact bytes; an unpublished local
preview is not a published version identity.
