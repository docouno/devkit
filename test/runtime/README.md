# Runtime verification

From the author checkout:

```sh
make build
python3 test/runtime/test_runtime.py
```

The harness needs Python 3, Linux, GNU Make, Bash, coreutils/flock and Docker Compose. The pinned Node image must be present; explicit Rust setup builds the shipping recipe. Consumers need no host Python or language SDK. Images and build caches are retained.

## What runs

The suite snapshots the current runtime, actual compiled helpers and bundled `project.mjs` once. Disposable fixtures use native npm workspace locks and real generated Make/Compose projections. Builtin-only fixture scripts use an explicitly accepted empty tooling profile to isolate runtime behavior from registry installation; the validator is not replaced. Released packages and registry resolution have separate suites.

The tests cover:

- Native setup/check/test/build and explicit fix, read-only source/config/locks, writable dependency/cache/output paths, source purity and failed/stale setup receipts.
- One Node workspace installation, shared-library prerequisite checks/builds, selectors and inherited `make -j`, native failure propagation and transitive local-input fingerprint changes.
- Real metadata, managed bytes/modes/checksum completeness, physical component registration, exclusive native manifests, tooling profiles and exact Node control version.
- Source generation changes between Make leaves and at aggregate completion, shared admission locking, pending transactions and symlink/type refusal before writes.
- Rust image recipe/ID validation and native offline commands; development admission for every registered component reachable through native Compose dependencies.
- Explicitly selected Compose profiles, unrelated inactive services with missing env files, global/selected logs, status and down. Down stops profiled services while retaining named data and unmodelled orphans.
- Creator-process cleanup, native failures, cancellation before and after readiness, exact resource ownership, cleanup-error reporting and two-checkout isolation.

Each run prints the captured runtime aggregate, controller hash and Node image. These identities bind the output to the implementation tested. See [verification](../../docs/verification.md) for the current release evidence.

## Execution contract

The root Makefile includes the versioned runtime; `.devkit/project.mk` supplies component registration and native library dependencies. The manifest hash is captured at Make start and checked under a per-checkout lock at leaves and aggregate completion. Reserved state/cache/output paths reject symlinks and unexpected filesystem types. An incomplete update blocks ordinary commands.

The read-only, networkless control container validates accepted metadata without dependency/cache mounts. Setup publishes a Node receipt only after a successful native installation with unchanged inputs. Rust and desktop images are accepted only by matching their recipe labels to current pins. Normal tasks neither pull nor rebuild images.

Development preflight reads Compose's normalized model with all profiles visible and without resolving service env files. It checks the actual selected dependency closure before startup; it does not force preparation of unrelated components. Actual startup retains normal profile selection. A product's concurrent manual file edits are outside the cooperative task lock.

## Guard and cleanup

```sh
bash runtime/guard.sh CONTAINER_NAME INVOCATION_TOKEN -- CREATOR ARGS...
```

The creator labels its exact one-off container with `io.docouno.devkit.invocation`. Existing names are not adopted. On cancellation, the guard terminates and reaps the creator process group before inspecting/stopping/removing the owned container; cleanup failures are visible. Make preserves failure, without promising the native command's numeric exit status.

Terminal process-group INT/TERM and TERM to the Make PID are covered. INT sent only to Make's PID, SIGKILL, daemon failure/hang, reboot and deliberately detached creator descendants have no cleanup guarantee. Tests assert important negative observations before outer cleanup and only remove exact owned resources. They never prune Docker or modify host software/settings.
