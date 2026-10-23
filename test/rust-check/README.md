# Rust helper verification

Run from the author checkout:

```sh
python3 test/rust-check/test_rust_check.py
```

The harness requires Linux, Python 3 and Docker; it installs no host Rust or Node.
It explicitly builds the shipping Rust Dockerfile, supplies its recipe hash and
checks the resulting recipe label, then uses the immutable local image ID. This
acquisition may need network access or Docker build cache. Native checks use
stdlib-only Cargo fixtures, `--network none`, the actual shipping helper, caller
UID/GID, read-only sources/helper and isolated writable Cargo cache/target mounts.
Cargo itself generates the fixture lock offline.

The tests cover native unit/doctest execution and failures, accepted compile-only
`no_run` doctests, empty and ignored-only discovery, conflicting/missing formatter
configurations, ignored/unsupported/global-disable formatter options, app cwd and
toolchain overrides, Clippy test targets and locked path-dependency drift. The
`no_run` fixture contains a panic body: Cargo must compile it successfully without
running it, and the helper must describe non-ignored native cases without claiming
runtime execution. Custom `harness=false` discovery is outside this contract.

Each run stores command arguments, native output, source/config/lock hashes and
image identity under ignored `.artifacts/rust-check/<run-id>/receipt.json`; fixtures
and build outputs live in a temporary directory and are removed. Every container
uses a unique exact name and invocation ownership label with `--rm`. Any remaining
owned container is reported as a failure before exact cleanup; an unexpected
ownership label is never adopted. The derived image/build cache remain available.
This suite verifies the Rust helper contract. The separate runtime suite verifies
Make/Compose integration, admission and cancellation.
