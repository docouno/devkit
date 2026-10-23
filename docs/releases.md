# Releases and updates

## Prepare locally

`make pack` builds `.artifacts/releases/0.1.0`, `.artifacts/devkit-0.1.0.tar.gz` and its `.sha256` sidecar. `make pack version=X.Y.Z` builds a different preview version without modifying source manifests. The archive includes:

```text
devkit                       # host Bash launcher
compose.factory.yaml
cli/                         # compiled, dependency-free Node factory
runtime/                     # Make/Compose helpers, pins, Rust recipe, license
templates/                   # native Node, React and Rust starters, with MIT notices
npm/docouno-env-X.Y.Z.tgz
release.json                 # version, package integrity and full payload identities
LICENSE
```

The package archive contains public exports, executable verifier, compiler baselines, README and MIT license. Source tests, caches, scratch fixtures, private instructions and release-builder/test-registry helpers are excluded. Template npm locks contain public registry URLs and exact release dependency identities. The archive checksum detects accidental corruption; it is not a signature or a substitute for trusted release distribution.

The builder stages output, creates a real `npm pack` archive, generates template locks using that archive, validates every payload identity and then replaces the local preview. Preview replacement is allowed for author iteration. An existing consumer rejects different bytes bearing the same version; accepted releases must be immutable. Publish a new version after any accepted content change.

## Local and registry package sources

Local mode copies a real environment archive into `.devkit/npm` and records an explicit relative file dependency under each Node app. The archive is part of the consumer repository; it is independent of the author checkout.

Registry mode records the exact environment version and canonical npm lock identities. Default init is registry mode; source-checkout `make init` explicitly chooses local mode. Updating preserves the mode, including for Rust-only projects, unless the operator asks to change it. Changing modes is a reviewable native manifest/lock change.

The registry verification fixture serves real archive metadata/bytes only for supplied `@docouno` packages and redirects public dependency requests to npm. It supports no publish/auth API. Both versions are installed with fresh caches and canonical locks; wrong integrity and absent versions must fail. The fixture is author-test-only and is never shipped as a runtime dependency.

## Ownership and recovery

`.devkit/manifest.json` records the accepted release identity, fresh generation ID, registered apps, owned tooling dependency fields and managed file identities. Root Makefile, Compose, app source, ordinary app config, product dependencies/scripts and environment values remain project-owned. Registration checks the app's native tool set against the accepted release.

The new release's updater validates old managed bytes and tooling values, stages native lock regeneration against a source snapshot, checks for concurrent edits and only then applies changes. It shares the runtime admission lock. Native install/peer failures and ownership conflicts happen before applying the payload. An incomplete apply writes a journal and saved before/after files; ordinary commands then refuse to run.

`devkit recover TARGET` restores the known pre-update state. It validates all saved and current identities before touching files and refuses unknown edits. Preserve conflicting edits separately and restore a known state before retrying recovery. The updater does not stash, reset, commit, push or create a PR. The product's normal Git review process supplies the final acceptance step.

## Publication boundary

No public publication is performed by this repository's current validation workflow. Before an authorized first publication:

1. Review the final named source tree, version, MIT notices, package contents and verification evidence.
2. Confirm `docouno` repository ownership and the npm scope/account's permission to publish `@docouno/env` publicly.
3. Publish the already-verified npm archive without rebuilding different bytes; distribute the matching standalone archive/checksum.
4. Verify the actual public installation and record the immutable release identities.

These are release-operator steps, not a request to install a private registry or add user credentials to Devkit. No publish credential, token or private Doco endpoint is needed for local creation or the test matrix. Release automation/signing, hosted CI and public provenance policy can be added when the publishing destination is established.
