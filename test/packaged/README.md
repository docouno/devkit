# Packaged consumers

Run `bash test/verify-consumers.sh .artifacts/releases/0.1.0` after `make pack`.
The author test requires Python 3, Bash, GNU Make/coreutils and Linux Docker with
Compose. It blocks host language SDK commands and uses copied standalone release
artifacts and consumer trees outside the author checkout.

The harness exercises the real released launcher and generated Make entrypoints
for Node, React, Rust and mixed projects: initialization with local npm archives,
setup, checks, tests, builds, source purity, expected failing quality gates,
emitted HTTP servers, development proxy interaction and isolation of two
same-named checkouts. Docker cleanup is limited to exact owned resources. It
records command logs and `receipt.json` under `.artifacts/packaged/<run-id>` and
retains failed work trees for diagnosis. Set `DEVKIT_VERIFY_KEEP=1` to retain a
successful work tree.

Pass `--next-release /path/to/another/version` (or set
`DEVKIT_VERIFY_NEXT_RELEASE`) to also verify real local-version updates,
preservation of consumer files/scripts/dependencies, managed-file conflicts,
required dependency setup, identical-version no-op and no-pending recovery.

This suite checks HTTP interaction; browser execution, registry package
resolution, cancellation and interrupted-update recovery have separate tests.
