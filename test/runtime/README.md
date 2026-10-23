# Runtime verification

Run from the author checkout:

```sh
python3 test/runtime/test_runtime.py
```

This test harness needs Python 3, Linux, GNU Make, Bash, coreutils/flock and a working Docker Compose installation. Consumers need no host Python, Node or Rust. The pinned Node image must already be present. The Rust integration invokes explicit setup, which builds the shipping recipe and fetches native dependencies. Its derived image and Docker build cache are retained as toolchain cache.

## Entrypoints and shared state

Consumer Make declares `DEVKIT_APPS := main` (or `web api`) and includes `.devkit/runtime/devkit.mk`. Native per-app targets retain ordinary Make dependency edges. Selection travels through exported environment values; app names must be registered lower-case slugs. `make -C <checkout>` is supported, including spaces in paths.

The factory supplies `.devkit/runtime/toolchains.env`, `.devkit/runtime.sha256` and `.devkit/manifest.json`. Make captures the manifest hash as `DEVKIT_EXPECTED_MANIFEST`. Each leaf compares it under `.devkit/state/task.lock`; aggregate setup/check/fix/test/build also performs a final snapshot preflight. The updater must use the same lock and change manifest generation on every successful update, including a return to an earlier release. Any `.devkit/update-pending.json` entry, including a dangling symlink or malformed object, blocks operations. Manual concurrent Git edits are outside this updater transaction.

Before any mkdir, lock open or Docker call, existing prefixes of reserved state, cache and writable native output paths must be directories/regular files of the expected type, without symlinks. This includes dependency receipts and temporary receipt paths. The task lock is opened without truncation. This prevents accidental redirection; it does not promise an attacker-proof filesystem transaction against concurrent malicious rewrites.

Each ordinary command also runs `verify-manifest.mjs` in the pinned Node control service: no network, a read-only container filesystem, and only one read-only checkout mount. No native dependency or cache mounts are present. It checks schema/version/UUID identities, every managed file hash and mode, the checksum table's own identity and complete entries, safe paths and symlink parents, and extra unlisted runtime files. Physical native apps must be registered both in manifest metadata and `DEVKIT_APPS`; registration without the corresponding Make edit fails helpfully. Unknown noncritical metadata is preserved. Setup explicitly prepares the Node control image even for Rust-only projects; ordinary commands never pull it. Rust native gates still invoke Cargo directly.

Accepted Rust state is one immutable local image ID in `.devkit/state/rust-image`. Its recipe label must match the shipping Dockerfile SHA256. Node setup records the package.json/package-lock.json content fingerprint in `.devkit/state/node-<app>.lock`; stale state requires explicit setup. The old receipt is removed before mutating npm ci; only successful setup with unchanged inputs atomically publishes a fresh receipt. Failed/interrupted repeat installation therefore cannot reuse an earlier acceptance. Ordinary gates do not pull or build tool images. Setup uses native locked installs, fix permits source writes, and other tool commands mount source/config/locks read-only with separate writable native dependency/output/cache mounts. Shared SELinux `z` labels, explicit app cwd and caller UID/GID are exercised on Fedora.

Product-owned `compose.yaml` receives `DEVKIT_PROJECT_ROOT`, `DEVKIT_PROJECT_NAME`, `DEVKIT_UID`, `DEVKIT_GID`, `DEVKIT_NODE_IMAGE` and, after Rust setup, `DEVKIT_RUST_IMAGE`. Service IDs match registered apps. The project identity includes the canonical checkout path. Compose receives explicit project directory/name/file and `/dev/null` env-file; a product may explicitly declare its own env files. `up` is detached, `status` reports actual port bindings, logs are finite unless `follow=1`, and `down` preserves named volumes.

## Reusable guard

```sh
bash runtime/guard.sh CONTAINER_NAME INVOCATION_TOKEN -- CREATOR ARGS...
```

The creator must label the exact one-off container `io.docouno.devkit.invocation=INVOCATION_TOKEN`; names and tokens accept letters, digits, hyphens and underscores. An existing name is rejected without adoption. The guard owns a creator process group, stops/reaps it before container inspection, then verifies ownership before bounded graceful stop/removal. Cancellation briefly rechecks after client exit for an in-flight create, followed by a final exact-name check. A cleanup verification failure is visible and turns an otherwise successful result into failure. Native failure status is preserved by the guard; GNU Make reports nonzero rather than promising the same numeric status.

Terminal process-group INT/TERM and TERM to the Make PID are tested. SIGINT to only the Make PID is not a supported cancellation contract. Runtime forwards cancellation to its guard; the app need not observe the original signal type. SIGKILL, daemon failure/hang, reboot and detached/setsid creator descendants have no cleanup guarantee. The creator should execute Docker directly and must not daemonize its work.

## Evidence: 2026-10-23

Final full run after review fixes: **21 tests, 86.023 seconds, OK**, Docker Compose **v5.6.0**, GNU Make **4.4.1**. Source hashes below were unchanged before/after that run. The pinned Node image is `node:24.18.0-bookworm-slim@sha256:6f7b03f7c2c8e2e784dcf9295400527b9b1270fd37b7e9a7285cf83b6951452d`.

The tests exercise:

- Native Node setup/check/test/build, explicit fix, untracked-source/manifest/lock purity, source read-only enforcement, writable outputs, absence of the Docker socket and host secret environment, stale locks and missing mandatory scripts/images.
- Native Make edges and app selection; native exit 42 becomes Make failure. Empty/ambiguous/unknown registration, managed checksum mismatch, pending update and admission contention fail visibly.
- Manifest generation changes under the admission lock between apps and after the last app gate; both prevent aggregate success.
- Writable-path symlinks across state/cache/dependencies/dist/target, dist pointing to source and outside the checkout, unexpected output/lock types, and nontruncating regular lock opens. Outside/source sentinels remain unchanged and native gates do not execute.
- Pending regular/malformed/directory/symlink/dangling objects; failed and cancelled repeated npm setup with identical manifest/lock bytes leaves no accepted receipt.
- Unchanged-manifest checksum omission plus managed drift, a checksum table missing a still-managed file, managed mode mismatch, an unlisted runtime file, managed symlink parents, unsafe paths, malformed essential metadata, version mismatch and complete physical-app registration. Fixture state contains identities generated from fixture bytes and valid UUIDs; it is not a registry-consumption proof.
- A real control-container inspection proves network none, read-only root/filesystem, caller identity and absence of dependency/cache mounts. Unknown noncritical metadata remains byte-for-byte unchanged by successful gates.
- Guard native failure, pre-existing-name refusal, INT/TERM cancellation after READY with container child-process identity checks, and cancellation through the production Make/runtime chain.
- Two distinct pre-READY scenarios: an ordinary delayed creator cannot survive cancellation to create later; an in-flight creator TERM handler completes a real `docker create` before the guard's final inspection. A marker proves creation happened, and the container is absent afterward. This last case distinguishes creator-first cleanup from the earlier inspect-first guard.
- A simulated unavailable cleanup query produces explicit diagnostics/nonzero status. This tests reporting, not real daemon availability guarantees.
- Two same-basename checkouts, including a space in the path: unique project identities, ephemeral host ports, finite logs, one checkout's down leaving the other's exact container running, and named data surviving down/up.
- Real Rust setup from the shipping Dockerfile, recipe-labelled immutable accepted image state, native locked fetch, offline check/test/build through the common runtime, and unchanged source/config/lock hashes.

All temporary containers, networks and volumes use unique identities, ownership labels and exact cleanup. Negative observations are asserted before outer test cleanup. No prune, public publish, host language install or baseline-clone mutation is performed.

Final verified source SHA256:

```text
2923fba8fc024ff13b7d6d9796f727fd61393a993ed821cd58488430fa53dbc5  runtime/devkit.mk
049a5580a7c7d33bd57a605969d4abd91849f73dfc3c8c8912ffa1491390ede9  runtime/devkit.sh
4914640688c10a1fadb007d2e7c67a81e73e7b15d39bf849c1efbcc8a5e3625e  runtime/compose.tools.yaml
6f70e753bda47358da943746a4837db6b7e70d417ef09c790b0acc9b224d589d  runtime/guard.sh
1f8e5e1b5497c238babd60d9a4a39b765f752233736032321022781cea60ab62  runtime/verify-manifest.mjs
88b4dfb67f04eb6639988453a62e3489bc2492aade0e811e32983ceca5abab94  runtime/rust.Dockerfile
1e62e24f586b54af70a7e11aa4fa6228042f0d8b4a06f36e6d2dfbe4d98fb9f7  runtime/rust-check.sh
dffd05dd4f6daa02aa9a304673aea08e5a9900fa55d3d532b47a72cd800768a2  test/runtime/test_runtime.py
```

The earlier 14-test run (52.894 seconds) covered the first implementation; the independent frozen review then found RV-01 through RV-04 in writable-path validation, dangling pending state, repeated-setup receipt transitions and manifest/checksum consistency. The seven new regression tests plus the relevant full suite above verify the revised implementation; they do not retroactively change the earlier review verdict. The Rust helper hash reflects the root owner's separate RV-05 policy clarification.

The earlier research probe established a simpler guard after READY. The production pre-create contract required creator-group cleanup first and cancellation-safe launcher bookkeeping; the production tests above are the evidence for this implementation. The initial Rust cross-owner integration failed only on the then-unformatted template tests; after the Rust owner applied native format, the same runtime integration passed without a runtime change.
