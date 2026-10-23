# Packaged consumers

Run after producing immutable local releases:

```sh
bash test/verify-consumers.sh .artifacts/releases/0.2.0 \
  --next-release .artifacts/releases/0.2.1 \
  --legacy-release .artifacts/devkit-0.1.0.tar.gz
```

The author harness needs Python 3.12+, Bash, GNU Make/coreutils and Linux Docker
with Compose. It blocks host language SDK commands. Release trees and generated
consumers are copied outside the author checkout; the original releases remain
unchanged. A release argument may be a built directory or its tar.gz archive.
`DEVKIT_VERIFY_NEXT_RELEASE` and `DEVKIT_VERIFY_LEGACY_RELEASE` also supply the
optional upgrade inputs. The legacy default is `.artifacts/releases/0.1.0` when
present. Missing optional inputs are recorded as untested in the receipt. A bounded
preflight may pass `--skip-legacy`; its receipt explicitly leaves legacy
migration untested.

The v2 matrix covers explicit Node API, Rust API and React web selections plus
the `fullstack-desktop` preset. It exercises real released CLI init/add-app/update
and generated Make setup/check/test/build/package commands, root workspace
locks, source purity and expected failing app/library gates. The shared UI edit
is observed by web and Electron development renderers through native HMR and in
both production outputs. Packaged Electron is extracted from its generated
archive and launched with its own ELF/ASAR, using the real Rust API. Test-only CDP
helpers live here and run in owned containers; application source has no testing
hooks. Screenshots and browser result JSON accompany the command logs.

Versioned updates check consumer code, dependency declarations, environment,
Make/Compose additions, managed conflicts and identical-version behavior. The
recovery case constructs a known interrupted journal, verifies refusal to
overwrite an unknown edit and invokes the released recovery CLI. This tests
recovery semantics; it does not claim to reproduce the timing of a live crash.
Core transaction tests cover that separate case. Remote bootstrap and registry
resolution have separate harnesses.

Every run records `receipt.json` and numbered logs under
`.artifacts/packaged/<run-id>`. Failed work trees remain for diagnosis; set
`DEVKIT_VERIFY_KEEP=1` to retain successful consumers too. Cleanup is limited to
exact owned Docker containers and checkout networks. Tool images and caches are
retained. No host display configuration, packages, global registry settings,
public publication or external product migration are changed.
